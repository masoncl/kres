//! Task + TaskManager.
//!
//! Invariants (bugs.md):
//! - C1: `inner: RwLock<Inner>` wraps ALL shared state: task list,
//!   symbol cache, context cache, findings, todo. Every reader path
//!   goes through a `.read()` guard; every writer through `.write()`.
//!   There is no unlocked `.tasks` access to race with.
//! - C2: every Task carries a `Shutdown` child; dropping a Task
//!   (`stop`, `clear`, `abandon_turn_limit`) MUST call `cancel()`
//!   first. The task's agent loop polls `shutdown.cancelled()` and
//!   exits.
//! - C3: `stop_all` / `clear` / `abandon_turn_limit` wait (with a
//!   grace timeout) for joined JoinHandles before returning. No
//!   abandoned threads keep burning budget.
//! - L1: no parallel "completed_ids" collection — Done tasks are
//!   queried off the ordered list.
//!
//! The TaskManager here is transport-agnostic: it spawns tasks, routes
//! cancellation, tracks state. The actual agent work is injected as a
//! closure. kres-agents (Phase 4) will plug that closure in.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, Notify, RwLock};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::findings::Finding;
use crate::shutdown::Shutdown;
use crate::todo::{TodoItem, TodoStatus};

pub type TaskId = u64;

static NEXT_TASK_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Pending,
    Running,
    Cancelling,
    Done,
    Errored,
}

impl TaskState {
    pub fn is_terminal(&self) -> bool {
        matches!(self, TaskState::Done | TaskState::Errored)
    }
}

pub struct Task {
    pub id: TaskId,
    /// Random v4 uuid generated at spawn time. Distinct from `id`
    /// (a process-local monotonic u64): the uuid is stable across
    /// restarts only in the sense of never colliding, so it's safe
    /// to stamp into `findings.json` for provenance across sessions.
    pub uuid: Uuid,
    pub name: String,
    /// Short tag for the todo that dispatched this task (TodoItem.id
    /// when non-empty, otherwise TodoItem.name). None for operator-
    /// typed prompts that don't come from the todo list. Fed into
    /// `FindingsStore::apply_delta` as part of the stamp so a
    /// finding's provenance records which todo produced it.
    pub todo_name: Option<String>,
    pub shutdown: Shutdown,
    /// State is behind a single RwLock on the manager; a Task itself
    /// holds only references.
    ///
    /// The JoinHandle is kept on the manager side so cancellation can
    /// await termination.
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Everything the manager tracks about one in-flight or recently-done
/// task. The `handle` is Option so we can `.take()` it when reaping.
struct TaskEntry {
    task: Task,
    state: TaskState,
    error: Option<String>,
    /// Findings contributed by this task during this turn. Cleared
    /// when reaped.
    findings_delta: Vec<Finding>,
    /// Raw followup objects from the task's response.
    followups: Vec<serde_json::Value>,
    analysis: String,
    /// Pipeline this task ran through. Default `Analysis`; set by
    /// finish_ok from the TaskOutcome.
    mode: crate::TaskMode,
    /// Code files the task produced. Only ever populated for
    /// Coding-mode tasks.
    code_output: Vec<crate::CodeFile>,
    /// String-replacement edits the task emitted. Only ever
    /// populated for Coding-mode tasks.
    code_edits: Vec<crate::CodeEdit>,
    handle: Option<JoinHandle<()>>,
    /// Gets notified when the task transitions into a terminal state.
    done_notify: Arc<Notify>,
}

/// Manager of the whole multi-task pipeline.
///
/// The public surface is entirely async + `Arc<Self>`; the manager
/// mutates its inner state under a single RwLock.
pub struct TaskManager {
    inner: RwLock<Inner>,
    /// LRU caches live behind their own Mutex so a cache `get` (which
    /// must mutate LRU order) doesn't serialise with reads of the
    /// task list.
    caches: Mutex<Caches>,
    /// Root shutdown for the whole session. Every Task's Shutdown is
    /// a child of this. Cancelling root cancels all tasks at once.
    root_shutdown: Shutdown,
    /// Mutex held by findings merge/write path — see bugs.md#H1.
    /// This is separate from `inner` so a long merge doesn't block
    /// reads of task state.
    findings_extract_lock: Mutex<()>,
    /// Per-session parallelism cap. Every spawn acquires a permit
    /// before its closure runs; releasing happens when the closure
    /// exits. Default is unbounded so tests and callers that do not
    /// opt into a cap keep their existing behavior.
    parallel_semaphore: Arc<tokio::sync::Semaphore>,
    /// Configured parallelism cap, 0 meaning unbounded. Kept beside
    /// the semaphore because `Semaphore` cannot report the permit
    /// count it was built with, and dispatch needs to know how many
    /// slots exist to decide how many to fill. One source of truth:
    /// callers must ask [`TaskManager::free_slots`] rather than
    /// carrying their own copy of the number.
    max_parallel: usize,
}

struct Inner {
    tasks: Vec<TaskEntry>,
    /// Running todo list (ordered).
    todo: Vec<TodoItem>,
    /// Running findings list — cross-task state after merge.
    findings: Vec<Finding>,
    /// Counter against `--turns N`. Incremented ONLY on successful
    /// task completion (no error AND produced analysis).
    completed_run_count: u32,
    /// Optional plan (produced once per top-level prompt). When set,
    /// the session persistence layer saves it alongside the todo
    /// list so a resumed session sees the same decomposition.
    plan: Option<crate::plan::Plan>,
    /// Work intentionally removed from active scheduling by goal,
    /// turn-cap, stop, or error handling.
    deferred: Vec<TodoItem>,
    /// Tasks started since the last reap batch finished publishing.
    ///
    /// Dispatch no longer waits for the reap queue to drain — that
    /// serialised every new task behind a ~65s publication and gave
    /// most of the reclaimed idle time straight back. It is bounded
    /// instead: at most `max_parallel` starts may happen without a
    /// reap completing. Fast tasks therefore cannot churn indefinitely
    /// while the reaper never gets a turn at the rate limiter, and a
    /// slow reaper cannot stall dispatch outright.
    starts_since_reap: usize,
    /// Todo ids, best first, as of the last prioritization. A
    /// preference list over `todo`, NOT a reordering of it: the todo
    /// list stays stable storage, and this is the separate artifact
    /// that says what to run next.
    ///
    /// Dispatch reads it and claims immediately, so no dispatch ever
    /// waits on a ranking round-trip. It is refreshed out of band
    /// after each todo-agent update. Ids that have since been claimed,
    /// completed or retired simply never match; rows it does not
    /// mention sort after the ones it does, in storage order, which is
    /// what makes a new row "append until the next ranking places it".
    ranked_order: Vec<String>,
    /// Rows claimed so far per survey group, keyed by `step_id` with
    /// the empty string standing for the unattributed bucket.
    ///
    /// Dispatch prefers the least-served group, so a region of the
    /// file that keeps producing followups cannot take every slot from
    /// one that produced few. Measured on the 2026-08-21
    /// arch/x86/kvm/mmu/mmu.c review: `audit-cluster-05` accumulated
    /// 119 followups against `audit-cluster-10`'s 21, and nothing in
    /// the ranking had any reason to correct that.
    ///
    /// Counts CLAIMS, not completions, so a group cannot monopolise
    /// the queue while its work is still in flight. Not persisted: a
    /// resumed session restarts the round robin, which is right,
    /// because it also restarts the dispatch history the counts
    /// describe.
    claims_by_group: BTreeMap<String, usize>,
}

/// Is this row one of the survey groups themselves, rather than a
/// followup discovered while auditing one?
///
/// Tested on `id` and `step_id` because those are the two fields
/// `reconcile_update` restores from the original. `kind` is NOT usable:
/// the todo agent owns `type`, and on the 2026-08-21 mmu.c review it
/// had dropped the field from all 13 surviving group rows — 0 of 448
/// rows still carried `kind == "review"`.
pub fn is_survey_group_row(item: &TodoItem) -> bool {
    !item.step_id.is_empty() && item.id == format!("review-{}", item.step_id)
}

#[derive(Debug, Clone)]
pub struct PlanChange {
    pub prior: Option<crate::plan::Plan>,
    pub current: crate::plan::Plan,
}

pub struct InferredTodoUpdate {
    pub items: Vec<TodoItem>,
    /// Rows completed by this reap batch. A batch can complete more
    /// than one, so this is a set rather than a single id.
    pub completed_todo_ids: Vec<String>,
    pub inference_snapshot: Vec<TodoItem>,
    pub plan_rewrite: Option<crate::plan::PlanRewrite>,
}

#[derive(Debug, Default)]
pub struct TodoClaims {
    pub items: Vec<TodoItem>,
    pub blocked: usize,
    pub remaining: usize,
}

/// Dispatchable rows plus how many were held back, as seen without
/// claiming. `budget` is how many the caller may actually start given
/// `--turns` and the tasks already in flight.
#[derive(Debug, Default, Clone)]
pub struct ReadyTodos {
    pub items: Vec<TodoItem>,
    pub blocked: usize,
    pub budget: usize,
}

/// How many new tasks may start: the `--turns` remainder minus what
/// is already in flight, so a completion racing dispatch cannot open
/// extra budget. `turns_limit == 0` means unlimited.
fn turn_budget(inner: &Inner, turns_limit: u32) -> usize {
    if turns_limit == 0 {
        return usize::MAX;
    }
    let active = inner
        .tasks
        .iter()
        .filter(|entry| !matches!(entry.state, TaskState::Done | TaskState::Errored))
        .count();
    turns_limit
        .saturating_sub(inner.completed_run_count)
        .saturating_sub(u32::try_from(active).unwrap_or(u32::MAX)) as usize
}

/// Ids and names of every terminal row — what `depends_on` is
/// satisfied against.
fn terminal_identities(todo: &[TodoItem]) -> std::collections::BTreeSet<String> {
    todo.iter()
        .filter(|item| item.status == TodoStatus::Done)
        .flat_map(|item| [item.id.clone(), item.name.clone()])
        .filter(|identity| !identity.is_empty())
        .collect()
}

fn same_todo_item(a: &TodoItem, b: &TodoItem) -> bool {
    if !a.id.is_empty() && !b.id.is_empty() {
        a.id == b.id
    } else {
        a.kind == b.kind && a.name.eq_ignore_ascii_case(&b.name)
    }
}

fn normalize_todo_dependencies(items: &mut [TodoItem], live_items: &[TodoItem]) {
    let mut identities = std::collections::BTreeMap::new();
    for (index, item) in items.iter().enumerate() {
        if !item.id.is_empty() {
            identities.insert(item.id.clone(), index);
        }
        identities.insert(item.name.clone(), index);
    }

    fn reaches(graph: &[Vec<usize>], start: usize, target: usize) -> bool {
        let mut stack = vec![start];
        let mut seen = vec![false; graph.len()];
        while let Some(node) = stack.pop() {
            if node == target {
                return true;
            }
            if seen[node] {
                continue;
            }
            seen[node] = true;
            stack.extend(graph[node].iter().copied());
        }
        false
    }

    // Install scheduler-established edges first, using live-list order rather
    // than model-provided order. Proposed reverse edges can therefore never
    // win a cycle tie and displace an existing prerequisite.
    let mut graph = vec![Vec::new(); items.len()];
    let mut protected = vec![Vec::new(); items.len()];
    for live in live_items {
        let Some(index) = items.iter().position(|item| same_todo_item(item, live)) else {
            continue;
        };
        for dependency in &live.depends_on {
            let Some(&dependency_index) = identities.get(dependency) else {
                continue;
            };
            if dependency_index == index || protected[index].contains(dependency) {
                continue;
            }
            graph[index].push(dependency_index);
            protected[index].push(dependency.clone());
        }
    }

    for (index, item) in items.iter_mut().enumerate() {
        let mut accepted = protected[index].clone();
        let mut seen: std::collections::BTreeSet<String> = accepted.iter().cloned().collect();
        for dependency in std::mem::take(&mut item.depends_on) {
            let Some(&dependency_index) = identities.get(&dependency) else {
                continue;
            };
            if dependency_index == index || !seen.insert(dependency.clone()) {
                continue;
            }
            if reaches(&graph, dependency_index, index) {
                continue;
            }
            graph[index].push(dependency_index);
            accepted.push(dependency);
        }
        item.depends_on = accepted;
    }
}

fn todo_id_base(item: &TodoItem) -> String {
    let raw = format!("{}-{}", item.kind, item.name);
    let mut out = String::new();
    let mut separator = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            separator = false;
        } else if !separator && !out.is_empty() {
            out.push('-');
            separator = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "todo".to_string()
    } else {
        out
    }
}

fn install_plan_locked(inner: &mut Inner, plan: Option<crate::plan::Plan>) {
    let step_ids: std::collections::BTreeSet<&str> = plan
        .as_ref()
        .map(|plan| plan.steps.iter().map(|step| step.id.as_str()).collect())
        .unwrap_or_default();
    for todo in &mut inner.todo {
        if !todo.step_id.is_empty() && !step_ids.contains(todo.step_id.as_str()) {
            todo.step_id.clear();
        }
    }
    inner.plan = plan;
}

fn sync_plan_locked(inner: &mut Inner) {
    let todo = inner.todo.clone();
    if let Some(plan) = inner.plan.as_mut() {
        plan.sync_from_todo(&todo);
    }
}

fn apply_plan_rewrite_locked(inner: &mut Inner, rewrite: crate::plan::PlanRewrite) -> PlanChange {
    let prior = inner.plan.clone();
    let mut new_plan = rewrite.apply_to(prior.as_ref());
    // Step execution state is scheduler-owned. A planning model may
    // reshape steps, but cannot declare work complete or skipped.
    for step in &mut new_plan.steps {
        step.status = crate::plan::PlanStepStatus::Pending;
    }
    install_plan_locked(inner, Some(new_plan));
    sync_plan_locked(inner);
    let current = inner.plan.clone().expect("plan was just installed");
    PlanChange { prior, current }
}

struct Caches {
    /// Shared symbol cache. Cap is enforced at insert time
    /// (bugs.md#M1).
    symbol_cache: LruCache<String, serde_json::Value>,
    /// Shared context cache.
    context_cache: LruCache<String, serde_json::Value>,
}

impl TaskManager {
    pub fn new() -> Arc<Self> {
        Self::with_caps(2000, 2000)
    }

    pub fn with_caps(symbol_cap: usize, context_cap: usize) -> Arc<Self> {
        // Default: effectively unbounded so existing tests that spawn
        // dozens of tasks in a tight loop keep running. The REPL calls
        // `with_max_parallel` instead.
        Self::build(symbol_cap, context_cap, 0)
    }

    /// Build a manager with a specific parallelism cap. `n = 0` is
    /// treated as "unbounded" (matches `new()` / `with_caps()`).
    pub fn with_max_parallel(n: usize) -> Arc<Self> {
        Self::build(2000, 2000, n)
    }

    fn build(symbol_cap: usize, context_cap: usize, max_parallel: usize) -> Arc<Self> {
        let permits = if max_parallel == 0 {
            tokio::sync::Semaphore::MAX_PERMITS
        } else {
            max_parallel
        };
        Arc::new(Self {
            inner: RwLock::new(Inner {
                tasks: Vec::new(),
                todo: Vec::new(),
                findings: Vec::new(),
                completed_run_count: 0,
                plan: None,
                deferred: Vec::new(),
                starts_since_reap: 0,
                ranked_order: Vec::new(),
                claims_by_group: BTreeMap::new(),
            }),
            caches: Mutex::new(Caches {
                symbol_cache: LruCache::new(symbol_cap),
                context_cache: LruCache::new(context_cap),
            }),
            root_shutdown: Shutdown::new(),
            findings_extract_lock: Mutex::new(()),
            max_parallel,
            parallel_semaphore: Arc::new(tokio::sync::Semaphore::new(permits)),
        })
    }

    pub fn root_shutdown(&self) -> &Shutdown {
        &self.root_shutdown
    }

    pub async fn completed_run_count(&self) -> u32 {
        self.inner.read().await.completed_run_count
    }

    /// Spawn a Task. The `work` closure receives the Task (with its
    /// own Shutdown) and should return its analysis+findings as a
    /// `TaskOutcome` when it completes.
    ///
    /// The returned TaskId can be used with `stop`, `join`, etc.
    pub async fn spawn<F, Fut>(
        self: &Arc<Self>,
        name: impl Into<String>,
        todo_name: Option<String>,
        work: F,
    ) -> TaskId
    where
        F: FnOnce(TaskHandle) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<TaskOutcome, String>> + Send + 'static,
    {
        let id = NEXT_TASK_ID.fetch_add(1, Ordering::SeqCst);
        let shutdown = self.root_shutdown.child();
        let task = Task {
            id,
            uuid: Uuid::new_v4(),
            name: name.into(),
            todo_name,
            shutdown: shutdown.clone(),
            created_at: chrono::Utc::now(),
        };
        let done_notify = Arc::new(Notify::new());

        // The spawned future MUST NOT begin work before its
        // JoinHandle is installed into the TaskEntry. Otherwise a
        // caller that invokes stop()/stop_all() between `spawn`
        // returning and the handle being written can observe an
        // entry with handle=None, interpret it as AlreadyDone, and
        // leave the future running (bugs.md#C2/#C3). The oneshot
        // enforces "install first, run second" without locks.
        let (start_tx, start_rx) = tokio::sync::oneshot::channel::<()>();

        let mgr = Arc::clone(self);
        let handle_for_future = TaskHandle {
            id,
            name: task.name.clone(),
            shutdown: shutdown.clone(),
            mgr: Arc::downgrade(&mgr),
        };
        let done_notify_for_future = done_notify.clone();
        // §30: parallelism cap. Cloning the Arc<Semaphore> keeps
        // the permit independent of the TaskManager lifetime —
        // dropping the permit when the task finishes is what frees
        // the slot for the next spawn to acquire.
        let semaphore = self.parallel_semaphore.clone();
        let handle = tokio::spawn(async move {
            // Wait until the outer function publishes our handle.
            let _ = start_rx.await;
            // Acquire a slot before transitioning to Running. On a
            // manager with `MAX_PERMITS` this is a no-op; on a
            // manager configured with `with_max_parallel(3)` the
            // fourth concurrent spawn blocks here until one of the
            // running tasks releases its permit.
            let _permit = semaphore.acquire_owned().await.ok();
            mgr.set_state(id, TaskState::Running).await;
            let result = work(handle_for_future).await;
            match result {
                Ok(outcome) => {
                    mgr.finish_ok(id, outcome).await;
                }
                Err(err) => {
                    mgr.finish_err(id, err).await;
                }
            }
            drop(_permit);
            done_notify_for_future.notify_waiters();
        });

        // Install the TaskEntry with its JoinHandle in one go.
        {
            let mut g = self.inner.write().await;
            g.tasks.push(TaskEntry {
                task: Task {
                    id,
                    uuid: task.uuid,
                    name: task.name.clone(),
                    todo_name: task.todo_name.clone(),
                    shutdown: shutdown.clone(),
                    created_at: task.created_at,
                },
                state: TaskState::Pending,
                error: None,
                findings_delta: Vec::new(),
                followups: Vec::new(),
                analysis: String::new(),
                mode: crate::TaskMode::default(),
                code_output: Vec::new(),
                code_edits: Vec::new(),
                handle: Some(handle),
                done_notify,
            });
        }
        // Handle is now readable under the lock; let the future run.
        let _ = start_tx.send(());
        id
    }

    async fn set_state(&self, id: TaskId, state: TaskState) {
        let mut g = self.inner.write().await;
        if let Some(entry) = g.tasks.iter_mut().find(|e| e.task.id == id) {
            entry.state = state;
        }
    }

    async fn finish_ok(&self, id: TaskId, outcome: TaskOutcome) {
        let mut g = self.inner.write().await;
        if let Some(entry) = g.tasks.iter_mut().find(|e| e.task.id == id) {
            entry.state = TaskState::Done;
            entry.analysis = outcome.analysis;
            entry.findings_delta = outcome.findings;
            entry.followups = outcome.followups;
            entry.mode = outcome.mode;
            entry.code_output = outcome.code_output;
            entry.code_edits = outcome.code_edits;
            // Per bugs.md#H4: only count tasks that actually produced
            // analysis and did not error. Coding-mode tasks count
            // against --turns N the same way analysis tasks do: they
            // consumed a slow-agent call, which is what the cap is
            // meant to bound.
            let produced = !entry.analysis.is_empty()
                || !entry.code_output.is_empty()
                || !entry.code_edits.is_empty();
            if produced {
                g.completed_run_count = g.completed_run_count.saturating_add(1);
            }
        }
    }

    async fn finish_err(&self, id: TaskId, err: String) {
        let mut g = self.inner.write().await;
        if let Some(entry) = g.tasks.iter_mut().find(|e| e.task.id == id) {
            entry.state = TaskState::Errored;
            entry.error = Some(err);
        }
    }

    /// Ask a single task to shut down. Returns once the task's future
    /// has terminated or the grace expires. bugs.md#C2 + #C3.
    pub async fn stop(&self, id: TaskId, grace: Duration) -> StopOutcome {
        let (shutdown, handle, done_notify) = {
            let mut g = self.inner.write().await;
            let Some(entry) = g.tasks.iter_mut().find(|e| e.task.id == id) else {
                return StopOutcome::NotFound;
            };
            if entry.state.is_terminal() {
                return StopOutcome::AlreadyDone;
            }
            entry.state = TaskState::Cancelling;
            (
                entry.task.shutdown.clone(),
                entry.handle.take(),
                entry.done_notify.clone(),
            )
        };
        shutdown.cancel();
        // done_notify was used by an earlier design; await-on-handle
        // below is sufficient for synchronization.
        let _ = &done_notify;
        if let Some(h) = handle {
            match tokio::time::timeout(grace, h).await {
                Ok(_join_result) => StopOutcome::Stopped,
                Err(_elapsed) => StopOutcome::GraceExpired,
            }
        } else {
            StopOutcome::AlreadyDone
        }
    }

    /// Cancel every non-terminal task and wait up to `grace` for all
    /// of them. bugs.md#C2, #C3.
    pub async fn stop_all(&self, grace: Duration) -> StopAllOutcome {
        // Cancel first, then await.
        let ids: Vec<TaskId> = {
            let g = self.inner.read().await;
            g.tasks
                .iter()
                .filter(|e| !e.state.is_terminal())
                .map(|e| e.task.id)
                .collect()
        };
        // Broadcast cancel to ALL at once so they start tearing down
        // in parallel.
        {
            let mut g = self.inner.write().await;
            for entry in g.tasks.iter_mut() {
                if !entry.state.is_terminal() {
                    entry.task.shutdown.cancel();
                    entry.state = TaskState::Cancelling;
                }
            }
        }
        let mut handles: Vec<(TaskId, JoinHandle<()>)> = Vec::new();
        {
            let mut g = self.inner.write().await;
            for entry in g.tasks.iter_mut() {
                if let Some(h) = entry.handle.take() {
                    handles.push((entry.task.id, h));
                }
            }
        }
        let deadline = tokio::time::Instant::now() + grace;
        let mut stopped = 0u32;
        let mut expired = 0u32;
        for (_, mut h) in handles {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                h.abort();
                expired += 1;
                continue;
            }
            match tokio::time::timeout(remaining, &mut h).await {
                Ok(_) => stopped += 1,
                Err(_) => {
                    h.abort();
                    expired += 1;
                }
            }
        }
        StopAllOutcome {
            requested: ids.len() as u32,
            stopped,
            grace_expired: expired,
        }
    }

    /// How many tasks have reached a terminal state but have not been
    /// reaped yet — the depth of the reap queue.
    ///
    /// Dispatch requires this to be zero. A terminal-unreaped task has
    /// findings and todo edits that no agent has seen, so claiming
    /// work against the list in that window ranks and dispatches
    /// against a stale world.
    pub async fn reap_queue_depth(&self) -> usize {
        let g = self.inner.read().await;
        g.tasks
            .iter()
            .filter(|e| matches!(e.state, TaskState::Done | TaskState::Errored))
            .count()
    }

    /// Record that a reap batch finished publishing, re-arming the
    /// dispatch start budget.
    ///
    /// Call this AFTER the batch's findings, todo rows and followups
    /// have landed — not after `reap()` — because the budget exists to
    /// guarantee the reaper gets inference time, and it has not had it
    /// until its agents have run.
    pub async fn note_reap_completed(&self) {
        self.inner.write().await.starts_since_reap = 0;
    }

    /// Starts still allowed before a reap must complete. `usize::MAX`
    /// when unbounded.
    pub async fn start_budget(&self) -> usize {
        if self.max_parallel == 0 {
            return usize::MAX;
        }
        let g = self.inner.read().await;
        // Mirrors `claim_ranked_todos`: with no tasks tracked there is
        // nothing that can ever re-arm the budget, so it cannot be
        // allowed to block.
        if g.tasks.is_empty() {
            return usize::MAX;
        }
        self.max_parallel.saturating_sub(g.starts_since_reap)
    }

    /// Reap done/errored tasks from the list and return summaries.
    pub async fn reap(&self) -> Vec<ReapedTask> {
        let mut g = self.inner.write().await;
        let mut reaped = Vec::new();
        let mut keep = Vec::with_capacity(g.tasks.len());
        for entry in g.tasks.drain(..) {
            if entry.state.is_terminal() {
                reaped.push(ReapedTask {
                    id: entry.task.id,
                    uuid: entry.task.uuid,
                    name: entry.task.name,
                    todo_name: entry.task.todo_name,
                    state: entry.state,
                    error: entry.error,
                    analysis: entry.analysis,
                    findings_delta: entry.findings_delta,
                    followups: entry.followups,
                    mode: entry.mode,
                    code_output: entry.code_output,
                    code_edits: entry.code_edits,
                });
            } else {
                keep.push(entry);
            }
        }
        g.tasks = keep;
        reaped
    }

    /// Snapshot of task states for the /tasks command.
    pub async fn snapshot(&self) -> Vec<TaskSnapshot> {
        let g = self.inner.read().await;
        g.tasks
            .iter()
            .map(|e| TaskSnapshot {
                id: e.task.id,
                uuid: e.task.uuid,
                name: e.task.name.clone(),
                state: e.state,
                todo_name: e.task.todo_name.clone(),
            })
            .collect()
    }

    /// Count of tasks that have not reached a terminal state yet
    /// (i.e. are still consuming a worker slot). Used by the REPL's
    /// auto-continue idle loop (§46) to decide whether to fire a
    /// batch when the operator is AFK.
    /// Configured parallelism cap. 0 means unbounded.
    pub fn max_parallel(&self) -> usize {
        self.max_parallel
    }

    /// How many more tasks may start right now.
    ///
    /// `usize::MAX` when unbounded. Counts every non-terminal task,
    /// including ones queued on the semaphore but not yet Running, so
    /// a dispatch cannot double-count a slot it just filled. Computed
    /// here rather than in the REPL so the cap that gates `spawn` and
    /// the cap that sizes a dispatch cannot drift apart.
    pub async fn free_slots(&self) -> usize {
        if self.max_parallel == 0 {
            return usize::MAX;
        }
        self.max_parallel.saturating_sub(self.active_count().await)
    }

    pub async fn active_count(&self) -> usize {
        let g = self.inner.read().await;
        g.tasks
            .iter()
            .filter(|e| !matches!(e.state, TaskState::Done | TaskState::Errored))
            .count()
    }

    // -- cache helpers -------------------------------------------------

    pub async fn cache_symbol(&self, key: impl Into<String>, value: serde_json::Value) {
        let mut g = self.caches.lock().await;
        g.symbol_cache.put(key.into(), value);
    }

    pub async fn get_cached_symbol(&self, key: &str) -> Option<serde_json::Value> {
        let mut g = self.caches.lock().await;
        g.symbol_cache.get(&key.to_string()).cloned()
    }

    pub async fn cache_context(&self, key: impl Into<String>, value: serde_json::Value) {
        let mut g = self.caches.lock().await;
        g.context_cache.put(key.into(), value);
    }

    pub async fn get_cached_context(&self, key: &str) -> Option<serde_json::Value> {
        let mut g = self.caches.lock().await;
        g.context_cache.get(&key.to_string()).cloned()
    }

    pub async fn remove_cached_context(&self, key: &str) -> Option<serde_json::Value> {
        let mut g = self.caches.lock().await;
        g.context_cache.remove(&key.to_string())
    }

    pub async fn cached_symbol_names(&self) -> Vec<String> {
        let g = self.caches.lock().await;
        g.symbol_cache.keys()
    }

    // -- todo ----------------------------------------------------------

    #[cfg(test)]
    async fn replace_todo(&self, items: Vec<TodoItem>) {
        let mut g = self.inner.write().await;
        g.todo = items;
    }

    /// Apply an inferred todo rewrite without allowing a stale model
    /// round-trip to overwrite scheduler-owned execution state.
    ///
    /// Todo inference starts from a snapshot and may take long enough
    /// for pending work to be dispatched or running work to finish.
    /// Merge against the live list under the write lock so those
    /// transitions win over the stale snapshot. The just-reaped task
    /// is authoritative even though its live row is still InProgress.
    pub async fn merge_inferred_state(&self, update: InferredTodoUpdate) -> Option<PlanChange> {
        fn matches_completed(item: &TodoItem, completed_todo_ids: &[String]) -> bool {
            completed_todo_ids.iter().any(|completed| {
                (!item.id.is_empty() && &item.id == completed) || &item.name == completed
            })
        }

        let InferredTodoUpdate {
            mut items,
            completed_todo_ids,
            inference_snapshot,
            plan_rewrite,
        } = update;
        let completed_todo_ids = completed_todo_ids.as_slice();
        let mut g = self.inner.write().await;
        // A row that existed in the inference snapshot but no longer
        // exists live was removed deliberately (`/done`, `/clear`, a
        // drain, or a concurrent reconciliation). Do not resurrect it
        // from the stale model response. Rows absent from the snapshot
        // are genuinely new model proposals and remain eligible.
        items.retain(|item| {
            let existed_at_start = inference_snapshot
                .iter()
                .any(|snapshot_item| same_todo_item(snapshot_item, item));
            let exists_live = g.todo.iter().any(|live| same_todo_item(live, item));
            !existed_at_start || exists_live || matches_completed(item, completed_todo_ids)
        });
        for item in &mut items {
            let live = g.todo.iter().find(|live| same_todo_item(item, live));
            // Coverage is write-once. Settled evidence beats anything a
            // later round sends, so a real sentence already on the live
            // row wins; the placeholder does not, or the agent's first
            // real answer could never land.
            if let Some(live) = live {
                if !crate::coverage_is_unwritten(&live.coverage) {
                    item.coverage.clone_from(&live.coverage);
                }
            }
            if matches_completed(item, completed_todo_ids) {
                item.status = TodoStatus::Done;
                if crate::coverage_is_unwritten(&item.coverage) {
                    item.coverage = crate::PLACEHOLDER_COVERAGE.to_string();
                }
            } else if let Some(live) = live {
                // Dependencies admitted into the live scheduler are
                // monotonic across model-authored full-list rewrites. A
                // later inference may add prerequisites, but it must not
                // silently remove prerequisites established by an earlier
                // completed task. Explicit todo deletion scrubs dependency
                // edges separately in remove_todo().
                let mut dependencies = live.depends_on.clone();
                for dependency in &item.depends_on {
                    if !dependencies.contains(dependency) {
                        dependencies.push(dependency.clone());
                    }
                }
                item.depends_on = dependencies;
                // Once linked, keep plan attribution scheduler-owned. A
                // model may attach an unlinked row, but cannot move existing
                // work between plan steps during a stale round trip.
                if !live.step_id.is_empty() {
                    item.step_id.clone_from(&live.step_id);
                }
                if live.status == TodoStatus::InProgress || live.status.is_terminal() {
                    item.status = live.status;
                    if crate::coverage_is_unwritten(&item.coverage)
                        && !crate::coverage_is_unwritten(&live.coverage)
                    {
                        item.coverage.clone_from(&live.coverage);
                    }
                }
            }
        }

        // Omission is how the todo agent reports deduplication. Honor it only
        // for an unchanged pending row that no live todo depends on. Rows
        // added or mutated after the inference snapshot, executor-owned rows,
        // completed history, and dependency targets remain authoritative.
        for live in &g.todo {
            if items.iter().any(|item| same_todo_item(item, live)) {
                continue;
            }
            let snapshot = inference_snapshot
                .iter()
                .find(|snapshot| same_todo_item(snapshot, live));
            let unchanged_since_snapshot = snapshot.is_some_and(|snapshot| {
                snapshot.name == live.name
                    && snapshot.kind == live.kind
                    && snapshot.status == live.status
                    && snapshot.reason == live.reason
                    && snapshot.depends_on == live.depends_on
                    && snapshot.coverage == live.coverage
                    && snapshot.id == live.id
                    && snapshot.step_id == live.step_id
            });
            let is_dependency_target = g.todo.iter().any(|item| {
                item.depends_on.iter().any(|dependency| {
                    (!live.id.is_empty() && dependency == &live.id) || dependency == &live.name
                })
            });
            if unchanged_since_snapshot
                && live.status == TodoStatus::Pending
                && !is_dependency_target
                && !matches_completed(live, completed_todo_ids)
            {
                continue;
            }
            let mut retained = live.clone();
            if matches_completed(&retained, completed_todo_ids) {
                retained.status = TodoStatus::Done;
                if crate::coverage_is_unwritten(&retained.coverage) {
                    retained.coverage = crate::PLACEHOLDER_COVERAGE.to_string();
                }
            }
            items.push(retained);
        }
        normalize_todo_dependencies(&mut items, &g.todo);
        g.todo = items;
        plan_rewrite.map(|rewrite| apply_plan_rewrite_locked(&mut g, rewrite))
    }

    /// Append todos without cloning and replacing the live list.
    /// Stable ids win; rows without ids deduplicate by `(kind, name)`
    /// and receive an id before becoming dispatchable.
    pub async fn append_todo_unique(&self, candidates: Vec<TodoItem>) -> usize {
        let mut g = self.inner.write().await;
        let mut added = 0;
        for mut candidate in candidates {
            if g.todo.iter().any(|item| same_todo_item(item, &candidate)) {
                continue;
            }
            if candidate.id.is_empty() {
                let base = todo_id_base(&candidate);
                let mut id = base.clone();
                let mut suffix = 2;
                while g.todo.iter().any(|item| item.id == id) {
                    id = format!("{base}-{suffix}");
                    suffix += 1;
                }
                candidate.id = id;
            }
            g.todo.push(candidate);
            added += 1;
        }
        added
    }

    async fn defer_matching(&self, include_running: bool) -> usize {
        let mut g = self.inner.write().await;
        let mut kept = Vec::with_capacity(g.todo.len());
        let mut count = 0;
        for mut item in std::mem::take(&mut g.todo) {
            let should_move = matches!(item.status, TodoStatus::Pending | TodoStatus::Blocked)
                || (include_running && item.status == TodoStatus::InProgress);
            if should_move {
                count += 1;
                item.status = TodoStatus::Pending;
                if !g
                    .deferred
                    .iter()
                    .any(|deferred| same_todo_item(deferred, &item))
                {
                    g.deferred.push(item);
                }
            } else {
                kept.push(item);
            }
        }
        g.todo = kept;
        count
    }

    /// Defer work that has not been dispatched. Running rows remain
    /// active because an executor still owns them.
    pub async fn defer_pending(&self) -> usize {
        self.defer_matching(false).await
    }

    /// Defer all non-terminal work after every executor has been
    /// cancelled and joined (or aborted after its grace period).
    pub async fn defer_all_after_stop(&self) -> usize {
        self.defer_matching(true).await
    }

    /// Atomically restore deferred work to active scheduling.
    /// Returns `(deferred_rows_consumed, active_rows_added)`.
    pub async fn restore_deferred(&self) -> (usize, usize) {
        let mut g = self.inner.write().await;
        let deferred = std::mem::take(&mut g.deferred);
        let consumed = deferred.len();
        let mut added = 0;
        for mut item in deferred {
            item.status = TodoStatus::Pending;
            if g.todo.iter().any(|live| same_todo_item(live, &item)) {
                continue;
            }
            g.todo.push(item);
            added += 1;
        }
        (consumed, added)
    }

    pub async fn deferred_snapshot(&self) -> Vec<TodoItem> {
        self.inner.read().await.deferred.clone()
    }

    pub async fn clear_session_work(&self) {
        let mut g = self.inner.write().await;
        g.todo.clear();
        g.deferred.clear();
        g.plan = None;
        // Ids are name-derived slugs, so a re-run of the same prompt
        // regenerates colliding ones. Leaving the order behind would
        // let a cleared session's ranking silently apply to the next.
        g.ranked_order.clear();
        // Same argument, same reason: step ids restart at
        // 01 for every coverage plan, so a surviving count would hand
        // the next topic's first group a dispatch debt it never
        // incurred.
        g.claims_by_group.clear();
    }

    /// Remove one todo without replacing unrelated rows whose state
    /// may have changed since an operator snapshot was rendered.
    pub async fn remove_todo(&self, identity: &str) -> bool {
        let mut g = self.inner.write().await;
        let removed_ids: std::collections::BTreeSet<String> = g
            .todo
            .iter()
            .filter(|item| {
                if item.id.is_empty() {
                    item.name == identity
                } else {
                    item.id == identity
                }
            })
            .flat_map(|item| [item.id.clone(), item.name.clone()])
            .filter(|value| !value.is_empty())
            .collect();
        let before = g.todo.len();
        g.todo.retain(|item| {
            if item.id.is_empty() {
                item.name != identity
            } else {
                item.id != identity
            }
        });
        let removed = g.todo.len() != before;
        if removed {
            for item in &mut g.todo {
                item.depends_on
                    .retain(|dependency| !removed_ids.contains(dependency));
            }
        }
        removed
    }

    /// Seed an initial list only when no todo state exists. The
    /// emptiness check and installation are one operation.
    pub async fn seed_todo_if_empty(&self, items: Vec<TodoItem>) -> bool {
        let mut g = self.inner.write().await;
        if !g.todo.is_empty() {
            return false;
        }
        g.todo = items;
        true
    }

    pub async fn todo_snapshot(&self) -> Vec<TodoItem> {
        self.inner.read().await.todo.clone()
    }

    /// How many survey group rows have not finished.
    ///
    /// While this is non-zero the file is by construction not fully
    /// reviewed, so a whole-file goal cannot honestly be met: the
    /// groups partition every function in the target. Callers use it
    /// to skip work that can only restate what the row statuses
    /// already say.
    pub async fn outstanding_survey_groups(&self) -> usize {
        self.inner
            .read()
            .await
            .todo
            .iter()
            .filter(|item| is_survey_group_row(item) && !item.status.is_terminal())
            .count()
    }

    /// Publish successful executor completion before any continuation LLM
    /// calls. This makes the resumable snapshot authoritative even when todo
    /// inference is slow or the process exits during that inference.
    pub async fn mark_todo_done(&self, identity: &str) -> bool {
        let mut g = self.inner.write().await;
        let Some(item) = g
            .todo
            .iter_mut()
            .find(|item| (!item.id.is_empty() && item.id == identity) || item.name == identity)
        else {
            return false;
        };
        item.status = TodoStatus::Done;
        // A fallback so the row is never invisible to the agent's
        // dedup step, NOT an answer: `coverage_is_unwritten` lets the
        // todo agent's real sentence replace it when it arrives.
        if crate::coverage_is_unwritten(&item.coverage) {
            item.coverage = crate::PLACEHOLDER_COVERAGE.to_string();
        }
        sync_plan_locked(&mut g);
        true
    }

    /// Atomically select dependency-ready pending work and transfer its
    /// ownership to the scheduler. A row changed or removed before this
    /// lock is acquired cannot be dispatched from a stale snapshot.
    pub async fn claim_ready_todos(&self, limit: usize) -> TodoClaims {
        self.claim_ready_todos_with_turn_limit(limit, 0).await
    }

    /// Claim dependency-ready work without allowing newly-dispatched tasks
    /// to exceed a finite session turn budget. The counter, active-task
    /// count, and status transitions are inspected under the same lock so a
    /// completion racing dispatch cannot open extra budget.
    pub async fn claim_ready_todos_with_turn_limit(
        &self,
        limit: usize,
        turns_limit: u32,
    ) -> TodoClaims {
        let mut g = self.inner.write().await;
        let claim_limit = limit.min(turn_budget(&g, turns_limit));
        let done = terminal_identities(&g.todo);
        let mut result = TodoClaims::default();
        for item in &mut g.todo {
            if item.status != TodoStatus::Pending {
                continue;
            }
            if !item
                .depends_on
                .iter()
                .all(|dependency| done.contains(dependency))
            {
                result.blocked += 1;
                continue;
            }
            if result.items.len() == claim_limit {
                result.remaining += 1;
                continue;
            }
            item.status = TodoStatus::InProgress;
            result.items.push(item.clone());
        }
        result
    }

    /// Ready pending rows plus the number blocked, without claiming
    /// anything.
    ///
    /// Split out of `claim_ready_todos_with_turn_limit` so the
    /// prioritization agent can rank the candidates before any row is
    /// flipped to InProgress. The ranking call is an LLM round-trip
    /// and must not happen under the write lock; the lock is retaken
    /// by `claim_selected_todos` once the picks are known. Rows that
    /// become unready in between are simply not claimed.
    pub async fn ready_pending_snapshot(&self, turns_limit: u32) -> ReadyTodos {
        let g = self.inner.read().await;
        let done = terminal_identities(&g.todo);
        let mut ready = ReadyTodos {
            budget: turn_budget(&g, turns_limit),
            ..ReadyTodos::default()
        };
        for item in &g.todo {
            if item.status != TodoStatus::Pending {
                continue;
            }
            if item
                .depends_on
                .iter()
                .all(|dependency| done.contains(dependency))
            {
                ready.items.push(item.clone());
            } else {
                ready.blocked += 1;
            }
        }
        ready
    }

    /// Publish a new ranked order (todo ids, best first). Replaces the
    /// previous one wholesale; ids are not validated, since a row that
    /// no longer exists simply never matches during a claim.
    pub async fn set_ranked_order(&self, ids: Vec<String>) {
        self.inner.write().await.ranked_order = ids;
    }

    /// The stored ranked order, for diagnostics and `/todo`.
    pub async fn ranked_order(&self) -> Vec<String> {
        self.inner.read().await.ranked_order.clone()
    }

    /// Claim up to `limit` dependency-ready rows, best-ranked first.
    ///
    /// This is the whole dispatch path. It takes the write lock once
    /// and makes no inference call: ranking already happened, out of
    /// band, into `ranked_order`. A missing or stale order degrades to
    /// storage order rather than stalling — ranking is an
    /// optimisation and must never gate a dispatch.
    pub async fn claim_ranked_todos(&self, limit: usize, turns_limit: u32) -> TodoClaims {
        let mut g = self.inner.write().await;
        // Every bound is applied here, under the one write lock that
        // also flips the rows. Computing free slots in the caller and
        // passing a limit down would be a TOCTOU: two dispatches could
        // each read "5 free" and each claim 5.
        let (free_slots, start_budget) = if self.max_parallel == 0 {
            (usize::MAX, usize::MAX)
        } else {
            let active = g
                .tasks
                .iter()
                .filter(|e| !matches!(e.state, TaskState::Done | TaskState::Errored))
                .count();
            // No tasks tracked at all means no reap can ever complete,
            // so the budget must not be what blocks. Without this the
            // budget deadlocks the session whenever a claim fails to
            // become a task — a claimed row whose `submit_prompt_inner`
            // returns false is InProgress with no executor, spends
            // budget, and leaves nothing to reap. Recovery would
            // require a restart, and even `/continue` could not clear
            // it.
            let budget = if g.tasks.is_empty() {
                usize::MAX
            } else {
                self.max_parallel.saturating_sub(g.starts_since_reap)
            };
            (self.max_parallel.saturating_sub(active), budget)
        };
        let claim_limit = limit
            .min(turn_budget(&g, turns_limit))
            .min(free_slots)
            .min(start_budget);
        let done = terminal_identities(&g.todo);
        let rank_of: std::collections::HashMap<&str, usize> = g
            .ranked_order
            .iter()
            .enumerate()
            .map(|(position, id)| (id.as_str(), position))
            .collect();
        // Every survey group is analysed before any followup is. A
        // followup is work discovered while auditing one group; letting
        // it compete for slots before the other groups have run once
        // means a productive region can consume the whole run. Measured
        // on the 2026-08-21 mmu.c review: at 2h10m the prioritizer's
        // top ten were all reachability questions on already-filed
        // findings, and the row carrying the defect the run was
        // chasing had appeared in ZERO rankings.
        //
        // Quantified over the group rows that EXIST, not over the
        // plan's steps: retirement deletes a row, and on that same run
        // only 13 of 15 steps still had one. Waiting on the missing two
        // would never terminate.
        let group_rows_pending = g
            .todo
            .iter()
            .any(|item| is_survey_group_row(item) && !item.status.is_terminal());
        // "Can still make progress on its own" means running now, or
        // Pending WITH ITS DEPENDENCIES MET. A Pending group row whose
        // dependency can never be satisfied would otherwise hold the
        // gate shut forever while never being claimable — and
        // dependency-blocked rows keep the Pending status, they do not
        // become TodoStatus::Blocked, so testing the status alone
        // misses exactly that case.
        let group_rows_claimable = g.todo.iter().any(|item| {
            is_survey_group_row(item)
                && match item.status {
                    TodoStatus::InProgress => true,
                    TodoStatus::Pending => item
                        .depends_on
                        .iter()
                        .all(|dependency| done.contains(dependency)),
                    _ => false,
                }
        });
        // Shut only while a group row can still make progress on its
        // own. A gate that cannot open is not a gate, it is a deadlock:
        // the same reasoning as the `g.tasks.is_empty()` escape above.
        let gate_shut = group_rows_pending && group_rows_claimable;

        let mut result = TodoClaims::default();
        let mut ready: Vec<(usize, usize)> = Vec::new();
        let mut gated = 0usize;
        for (index, item) in g.todo.iter().enumerate() {
            if item.status != TodoStatus::Pending {
                continue;
            }
            if !item
                .depends_on
                .iter()
                .all(|dependency| done.contains(dependency))
            {
                result.blocked += 1;
                continue;
            }
            if gate_shut && !is_survey_group_row(item) {
                // Not dispatchable until the survey has been covered
                // once, but still Pending and still counted below, so
                // `/todo` and the reaper's drain logic see the real
                // queue depth rather than an empty one.
                gated += 1;
                continue;
            }
            // Unranked rows sort after every ranked one, and a stable
            // sort keeps them in storage order among themselves.
            let rank = rank_of.get(item.id.as_str()).copied().unwrap_or(usize::MAX);
            ready.push((rank, index));
        }
        result.remaining = ready.len().saturating_sub(claim_limit) + gated;
        // Least-served group first, then the prioritizer's opinion
        // WITHIN that group, then storage order. The ranking agent
        // still decides which of a group's rows runs next; it no longer
        // decides which group gets the slot.
        //
        // Re-selected on every iteration rather than sorted once,
        // because a claim changes the key: one dispatch of N slots must
        // spread across N groups, not hand all N to whichever group was
        // least-served when the wave began.
        let mut taken = 0usize;
        while taken < claim_limit {
            let Some(position) = ready
                .iter()
                .enumerate()
                .min_by_key(|(_, (rank, index))| {
                    let step = g.todo[*index].step_id.as_str();
                    (
                        g.claims_by_group.get(step).copied().unwrap_or(0),
                        *rank,
                        *index,
                    )
                })
                .map(|(position, _)| position)
            else {
                break;
            };
            let (_, index) = ready.swap_remove(position);
            let item = &mut g.todo[index];
            item.status = TodoStatus::InProgress;
            let step = item.step_id.clone();
            result.items.push(item.clone());
            *g.claims_by_group.entry(step).or_insert(0) += 1;
            taken += 1;
        }
        g.starts_since_reap = g.starts_since_reap.saturating_add(result.items.len());
        result
    }

    /// Claim exactly the named rows, in the order given, subject to
    /// the turn budget. Ids that are no longer Pending or whose
    /// dependencies are no longer satisfied are skipped.
    pub async fn claim_selected_todos(&self, ids: &[String], turns_limit: u32) -> Vec<TodoItem> {
        let mut g = self.inner.write().await;
        let budget = turn_budget(&g, turns_limit);
        let done = terminal_identities(&g.todo);
        let mut claimed = Vec::new();
        for id in ids {
            if claimed.len() >= budget {
                break;
            }
            let Some(item) = g
                .todo
                .iter_mut()
                .find(|item| !item.id.is_empty() && item.id == *id)
            else {
                continue;
            };
            if item.status != TodoStatus::Pending {
                continue;
            }
            if !item
                .depends_on
                .iter()
                .all(|dependency| done.contains(dependency))
            {
                continue;
            }
            item.status = TodoStatus::InProgress;
            claimed.push(item.clone());
        }
        claimed
    }

    pub async fn clear_active_todos(&self) {
        let mut g = self.inner.write().await;
        g.todo.clear();
        g.ranked_order.clear();
    }

    #[cfg(test)]
    async fn set_todo_status_for_test(&self, identity: &str, status: TodoStatus) {
        let mut g = self.inner.write().await;
        if let Some(item) = g.todo.iter_mut().find(|item| item.id == identity) {
            item.status = status;
        }
    }

    /// Flip every `InProgress` todo back to `Pending`. Called on
    /// exit paths that drain the todo list (ctrl-c, --turns cap,
    /// goal-met stop) so items are persisted/deferred instead of
    /// orphaned in a non-terminal status that no process owns any
    /// more.
    pub async fn reset_in_progress_to_pending(&self) -> usize {
        let mut g = self.inner.write().await;
        let mut n = 0usize;
        for i in g.todo.iter_mut() {
            if i.status == TodoStatus::InProgress {
                i.status = TodoStatus::Pending;
                n += 1;
            }
        }
        n
    }

    // -- plan ----------------------------------------------------------

    pub async fn plan_snapshot(&self) -> Option<crate::plan::Plan> {
        self.inner.read().await.plan.clone()
    }

    /// Install a plan (or clear it when `None`). When the new plan
    /// is `Some` and its step ids differ from the prior plan, walks
    /// the current todo list and clears `step_id` on any todo whose
    /// prior step id is not in the new plan — otherwise those
    /// orphans would drag the next `sync_plan_from_todo` pass over
    /// the plan's linkage directions and never roll up into any
    /// step. When the new plan is `None` (or carries no steps),
    /// strips `step_id` from every todo.
    pub async fn set_plan(&self, plan: Option<crate::plan::Plan>) {
        let mut g = self.inner.write().await;
        install_plan_locked(&mut g, plan);
    }

    /// Apply a model-authored plan rewrite against the live plan in
    /// one critical section, then derive execution status from the
    /// live todo list before publishing it.
    pub async fn apply_plan_rewrite(&self, rewrite: crate::plan::PlanRewrite) -> PlanChange {
        let mut g = self.inner.write().await;
        apply_plan_rewrite_locked(&mut g, rewrite)
    }

    /// Recompute plan step statuses from the current todo list.
    /// No-op when no plan is set. Call after any todo mutation that
    /// could flip a linked item's status.
    pub async fn sync_plan_from_todo(&self) {
        let mut g = self.inner.write().await;
        sync_plan_locked(&mut g);
    }

    /// Synchronize the plan and take the manager-owned portion of a
    /// resumable session snapshot under one lock. This prevents a
    /// persisted plan from describing a different todo generation
    /// than the rows stored beside it.
    pub async fn sync_and_snapshot_runtime(
        &self,
    ) -> (Option<crate::plan::Plan>, Vec<TodoItem>, Vec<TodoItem>, u32) {
        let mut g = self.inner.write().await;
        sync_plan_locked(&mut g);
        (
            g.plan.clone(),
            g.todo.clone(),
            g.deferred.clone(),
            g.completed_run_count,
        )
    }

    /// Replace all resumable manager state during startup, before
    /// task execution begins.
    pub async fn load_runtime_state(
        &self,
        todo: Vec<TodoItem>,
        deferred: Vec<TodoItem>,
        plan: Option<crate::plan::Plan>,
        completed_run_count: u32,
    ) {
        let mut g = self.inner.write().await;
        g.todo = todo;
        g.deferred = deferred;
        g.completed_run_count = completed_run_count;
        install_plan_locked(&mut g, plan);
    }

    // -- findings ------------------------------------------------------

    pub async fn findings_snapshot(&self) -> Vec<Finding> {
        self.inner.read().await.findings.clone()
    }

    pub async fn replace_findings(&self, findings: Vec<Finding>) {
        let mut g = self.inner.write().await;
        g.findings = findings;
    }

    /// Apply a finding delta directly to the live manager mirror.
    /// Used when no persistent FindingsStore exists; avoids a
    /// snapshot/apply/replace race between completed tasks.
    pub async fn apply_findings_delta(
        &self,
        delta: &[Finding],
        task: Option<&str>,
        task_analysis: Option<&str>,
    ) -> crate::findings::DeltaCounts {
        let mut g = self.inner.write().await;
        crate::findings::apply_delta_to_list(&mut g.findings, delta, task, task_analysis)
    }

    /// Lock the findings extract lock for the duration of the passed
    /// future. DO NOT call network-bound code inside this — it's
    /// meant to serialize the cheap steps only (bugs.md#H1).
    pub async fn with_findings_extract_lock<F, Fut, T>(&self, f: F) -> T
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        let _guard = self.findings_extract_lock.lock().await;
        f().await
    }
}

#[derive(Debug)]
pub enum StopOutcome {
    Stopped,
    GraceExpired,
    AlreadyDone,
    NotFound,
}

#[derive(Debug, PartialEq, Eq)]
pub struct StopAllOutcome {
    pub requested: u32,
    pub stopped: u32,
    pub grace_expired: u32,
}

#[derive(Debug)]
pub struct TaskSnapshot {
    pub id: TaskId,
    pub uuid: Uuid,
    pub name: String,
    pub state: TaskState,
    pub todo_name: Option<String>,
}

#[derive(Debug)]
pub struct ReapedTask {
    pub id: TaskId,
    pub uuid: Uuid,
    pub name: String,
    pub todo_name: Option<String>,
    pub state: TaskState,
    pub error: Option<String>,
    pub analysis: String,
    pub findings_delta: Vec<Finding>,
    pub followups: Vec<serde_json::Value>,
    /// Pipeline the task ran through. Reaper consumes this to decide
    /// whether to run the findings merger (Analysis) or persist code
    /// files (Coding).
    pub mode: crate::TaskMode,
    /// Code files emitted by a Coding-mode task.
    pub code_output: Vec<crate::CodeFile>,
    /// String-replacement edits emitted by a Coding-mode task.
    pub code_edits: Vec<crate::CodeEdit>,
}

#[derive(Debug, Clone, Default)]
pub struct TaskOutcome {
    pub analysis: String,
    pub findings: Vec<Finding>,
    /// Structured followup requests the task produced. Carried
    /// through to the reaper so a future todo-agent pass can
    /// promote them to todo items without re-parsing the analysis
    /// prose.
    pub followups: Vec<serde_json::Value>,
    /// Pipeline the task ran through. Reaper uses this to gate the
    /// merge/consolidator work: Analysis tasks feed the findings
    /// pipeline, Coding tasks write files and skip the merger.
    pub mode: crate::TaskMode,
    /// Code files produced by a Coding-mode task. Empty for
    /// Audit-mode tasks. The reaper writes each entry under
    /// `<results>/code/<path>`.
    pub code_output: Vec<crate::CodeFile>,
    /// Surgical edits produced by a Coding-mode task. The reaper
    /// applies each entry via kres_agents::tools::edit_file.
    pub code_edits: Vec<crate::CodeEdit>,
}

/// Handed to a task's work closure. Provides cancellation and access
/// to the manager for cache/findings reads.
#[derive(Clone)]
pub struct TaskHandle {
    pub id: TaskId,
    pub name: String,
    pub shutdown: Shutdown,
    mgr: std::sync::Weak<TaskManager>,
}

impl TaskHandle {
    /// Returns the manager if it still exists, else None (the caller
    /// should treat None as "shut down in progress").
    pub fn manager(&self) -> Option<Arc<TaskManager>> {
        self.mgr.upgrade()
    }
}

// -- small bounded LRU --------------------------------------------------

/// Tiny LRU for caches. Cap is enforced on every `put`. bugs.md#M1.
pub(crate) struct LruCache<K: Eq + std::hash::Hash + Clone, V> {
    map: HashMap<K, (V, u64)>,
    cap: usize,
    clock: u64,
}

impl<K: Eq + std::hash::Hash + Clone, V> LruCache<K, V> {
    pub fn new(cap: usize) -> Self {
        Self {
            map: HashMap::new(),
            cap,
            clock: 0,
        }
    }

    pub fn put(&mut self, k: K, v: V) {
        self.clock = self.clock.wrapping_add(1);
        self.map.insert(k, (v, self.clock));
        if self.map.len() > self.cap {
            // Evict oldest (smallest tick).
            if let Some((oldest_key, _)) = self
                .map
                .iter()
                .min_by_key(|(_, (_, t))| *t)
                .map(|(k, v)| (k.clone(), v.1))
            {
                self.map.remove(&oldest_key);
            }
        }
    }

    pub fn get(&mut self, k: &K) -> Option<&V> {
        self.clock = self.clock.wrapping_add(1);
        let tick = self.clock;
        let val = self.map.get_mut(k)?;
        val.1 = tick;
        Some(&val.0)
    }

    pub fn remove(&mut self, k: &K) -> Option<V> {
        self.map.remove(k).map(|(value, _)| value)
    }

    pub fn keys(&self) -> Vec<K> {
        self.map.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn max_parallel_caps_concurrently_running_tasks() {
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
        // The cap is the only thing bounding concurrency once the
        // full-drain dispatch barrier is gone, so assert the
        // semaphore actually gates `spawn` rather than trusting that
        // the permit is acquired somewhere.
        let mgr = TaskManager::with_max_parallel(3);
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        for _ in 0..12 {
            let live = live.clone();
            let peak = peak.clone();
            mgr.spawn("capped", None, move |_| async move {
                let now = live.fetch_add(1, AtomicOrdering::SeqCst) + 1;
                peak.fetch_max(now, AtomicOrdering::SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
                live.fetch_sub(1, AtomicOrdering::SeqCst);
                Ok(TaskOutcome {
                    analysis: "done".into(),
                    ..Default::default()
                })
            })
            .await;
        }
        // Poll rather than sleeping a fixed interval: 12 tasks at 20ms
        // through 3 slots is ~80ms, but CI timing is not a contract.
        for _ in 0..200 {
            if mgr.reap_queue_depth().await == 12 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(mgr.reap_queue_depth().await, 12, "all tasks finished");
        assert!(
            peak.load(AtomicOrdering::SeqCst) <= 3,
            "peak concurrency {} exceeded the cap of 3",
            peak.load(AtomicOrdering::SeqCst)
        );
    }

    #[tokio::test]
    async fn reap_queue_depth_counts_terminal_unreaped_only() {
        let mgr = TaskManager::new();
        // A task that finishes but has not been reaped is queue depth,
        // not idleness. This is the distinction the old
        // `snapshot().is_empty()` dispatch barrier could not express:
        // it lumped running tasks in with unpublished terminal ones.
        mgr.spawn("finished", None, |_| async {
            Ok(TaskOutcome {
                analysis: "done".into(),
                ..Default::default()
            })
        })
        .await;
        let gate = Arc::new(tokio::sync::Notify::new());
        let hold = gate.clone();
        mgr.spawn("still running", None, move |_| async move {
            hold.notified().await;
            Ok(TaskOutcome::default())
        })
        .await;
        for _ in 0..200 {
            if mgr.reap_queue_depth().await == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(mgr.reap_queue_depth().await, 1, "one terminal, one running");
        assert_eq!(mgr.active_count().await, 1, "the runner is still active");
        assert_eq!(mgr.snapshot().await.len(), 2, "both are still tracked");

        assert_eq!(mgr.reap().await.len(), 1);
        assert_eq!(
            mgr.reap_queue_depth().await,
            0,
            "reaping drains the queue without touching the runner"
        );
        assert_eq!(mgr.active_count().await, 1);
        gate.notify_waiters();
    }

    #[tokio::test]
    async fn ranked_claim_prefers_the_stored_order_then_storage_order() {
        let mgr = TaskManager::new();
        let row = |id: &str| {
            let mut t = TodoItem::new(id, "review");
            t.id = id.to_string();
            t
        };
        mgr.replace_todo(vec![row("a"), row("b"), row("c"), row("d")])
            .await;
        // Ranking names two rows; the other two are new since the last
        // refresh and must follow, in storage order.
        mgr.set_ranked_order(vec!["c".into(), "a".into()]).await;
        let claims = mgr.claim_ranked_todos(3, 0).await;
        let ids: Vec<&str> = claims.items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["c", "a", "b"]);
        assert_eq!(claims.remaining, 1);
        assert_eq!(
            mgr.claim_ranked_todos(9, 0).await.items[0].id,
            "d",
            "claimed rows are no longer Pending, so they drop out"
        );
    }

    #[tokio::test]
    async fn ranked_claim_falls_back_to_storage_order_without_a_ranking() {
        // Ranking is an optimisation. No stored order, a stale one, or
        // one naming rows that have since vanished must all still
        // dispatch rather than stall the wave.
        let mgr = TaskManager::new();
        let row = |id: &str| {
            let mut t = TodoItem::new(id, "review");
            t.id = id.to_string();
            t
        };
        mgr.replace_todo(vec![row("a"), row("b")]).await;
        let claims = mgr.claim_ranked_todos(2, 0).await;
        let ids: Vec<&str> = claims.items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);

        mgr.replace_todo(vec![row("x"), row("y")]).await;
        mgr.set_ranked_order(vec!["long-gone".into(), "also-gone".into()])
            .await;
        let claims = mgr.claim_ranked_todos(2, 0).await;
        let ids: Vec<&str> = claims.items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["x", "y"], "stale ranking degrades, not stalls");
    }

    #[tokio::test]
    async fn ranked_claim_skips_blocked_rows_and_respects_the_turn_budget() {
        let mgr = TaskManager::new();
        let mut a = TodoItem::new("a", "review");
        a.id = "a".into();
        let mut b = TodoItem::new("b", "review");
        b.id = "b".into();
        b.depends_on = vec!["a".into()];
        let mut c = TodoItem::new("c", "review");
        c.id = "c".into();
        mgr.replace_todo(vec![a, b, c]).await;
        // Rank the blocked row first: it must still be held back.
        mgr.set_ranked_order(vec!["b".into(), "c".into(), "a".into()])
            .await;
        let claims = mgr.claim_ranked_todos(10, 2).await;
        let ids: Vec<&str> = claims.items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["c", "a"]);
        assert_eq!(claims.blocked, 1);
    }

    #[tokio::test]
    async fn one_update_completes_every_row_the_batch_finished() {
        // The reaper reconciles a whole reaped batch in one round, so
        // a single update carries several completions. Each must land
        // Done with coverage; none may be left Pending for a later
        // round that will never come.
        let mgr = TaskManager::new();
        let row = |id: &str| {
            let mut t = TodoItem::new(id, "review");
            t.id = id.to_string();
            t
        };
        mgr.replace_todo(vec![row("a"), row("b"), row("c")]).await;
        let snapshot = mgr.todo_snapshot().await;
        mgr.merge_inferred_state(InferredTodoUpdate {
            // The agent returns pending rows only; the completed ones
            // are reconstructed from Rust's copy.
            items: vec![row("c")],
            completed_todo_ids: vec!["a".into(), "b".into()],
            inference_snapshot: snapshot,
            plan_rewrite: None,
        })
        .await;
        let after = mgr.todo_snapshot().await;
        for id in ["a", "b"] {
            let item = after.iter().find(|t| t.id == id).expect("row survives");
            assert_eq!(item.status, TodoStatus::Done, "{id} should be done");
            assert!(!item.coverage.is_empty(), "{id} needs coverage");
        }
        assert_eq!(
            after.iter().find(|t| t.id == "c").unwrap().status,
            TodoStatus::Pending
        );
    }

    #[tokio::test]
    async fn free_slots_tracks_the_cap_and_survives_it_being_unset() {
        // Dispatch sizes itself from this, and the manager owns both
        // it and the semaphore that enforces it, so the number that
        // gates `spawn` and the number that sizes a dispatch cannot
        // disagree.
        let mgr = TaskManager::with_max_parallel(2);
        assert_eq!(mgr.max_parallel(), 2);
        assert_eq!(mgr.free_slots().await, 2);
        let hold = Arc::new(tokio::sync::Notify::new());
        for _ in 0..2 {
            let held = hold.clone();
            mgr.spawn("busy", None, move |_| async move {
                held.notified().await;
                Ok(TaskOutcome::default())
            })
            .await;
        }
        assert_eq!(mgr.free_slots().await, 0);
        hold.notify_waiters();

        let unbounded = TaskManager::new();
        assert_eq!(unbounded.max_parallel(), 0);
        assert_eq!(unbounded.free_slots().await, usize::MAX);
    }

    #[tokio::test]
    async fn clearing_work_clears_the_ranked_order_too() {
        // Ids are name-derived slugs, so a re-run of the same prompt
        // mints the same ones. A surviving order would silently rank
        // the next session's rows.
        let mgr = TaskManager::new();
        mgr.set_ranked_order(vec!["a".into(), "b".into()]).await;
        mgr.clear_active_todos().await;
        assert!(mgr.ranked_order().await.is_empty());

        mgr.set_ranked_order(vec!["a".into()]).await;
        mgr.clear_session_work().await;
        assert!(mgr.ranked_order().await.is_empty());
    }

    #[tokio::test]
    async fn tasks_finishing_during_a_reap_are_drained_by_one_later_call() {
        // The operator's requirement: if A's reap is being published
        // when B, C and D finish, all three are handled by a single
        // subsequent reap call, not one call each. Publication is
        // modelled by simply not calling `reap()` for a while — that
        // is exactly what the reaper's batch pass looks like to the
        // rest of the manager.
        let mgr = TaskManager::new();
        mgr.spawn("a", None, |_| async { Ok(TaskOutcome::default()) })
            .await;
        for _ in 0..200 {
            if mgr.reap_queue_depth().await == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(mgr.reap().await.len(), 1);
        for name in ["b", "c", "d"] {
            mgr.spawn(name, None, |_| async { Ok(TaskOutcome::default()) })
                .await;
        }
        for _ in 0..200 {
            if mgr.reap_queue_depth().await == 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(mgr.reap_queue_depth().await, 3, "all three queued");
        let second = mgr.reap().await;
        assert_eq!(second.len(), 3, "one call drains the whole queue");
        assert_eq!(mgr.reap_queue_depth().await, 0);
    }

    #[tokio::test]
    async fn start_budget_bounds_claims_between_reap_completions() {
        // At most `max_parallel` tasks may start without a reap
        // completing, so a stream of fast tasks cannot starve the
        // reaper of inference time on the shared rate limiter.
        //
        // One task is held open throughout. It keeps the task list
        // non-empty (the budget deliberately does not block when
        // nothing could ever re-arm it) and it pins `free_slots` at
        // cap-1, so the claims that stop below that are stopped by the
        // budget and nothing else. Claiming does not spawn tasks here,
        // so free_slots stays at 3 for the whole test.
        let mgr = TaskManager::with_max_parallel(4);
        let hold = Arc::new(tokio::sync::Notify::new());
        let held = hold.clone();
        mgr.spawn("holder", None, move |_| async move {
            held.notified().await;
            Ok(TaskOutcome::default())
        })
        .await;
        for _ in 0..200 {
            if mgr.active_count().await == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let rows: Vec<TodoItem> = (0..9)
            .map(|i| {
                let mut t = TodoItem::new(format!("row{i}"), "review");
                t.id = format!("row{i}");
                t
            })
            .collect();
        mgr.replace_todo(rows).await;
        assert_eq!(mgr.free_slots().await, 3);
        assert_eq!(mgr.start_budget().await, 4);

        // Bounded by free slots first.
        assert_eq!(mgr.claim_ranked_todos(9, 0).await.items.len(), 3);
        assert_eq!(mgr.start_budget().await, 1);
        // Now the budget is the binding constraint: 3 slots are still
        // free, but only one start remains.
        assert_eq!(mgr.free_slots().await, 3);
        assert_eq!(mgr.claim_ranked_todos(9, 0).await.items.len(), 1);
        assert_eq!(
            mgr.claim_ranked_todos(9, 0).await.items.len(),
            0,
            "budget exhausted until a reap publishes"
        );

        mgr.note_reap_completed().await;
        assert_eq!(mgr.start_budget().await, 4);
        assert_eq!(mgr.claim_ranked_todos(9, 0).await.items.len(), 3);
        hold.notify_waiters();
    }

    fn group_row(step: &str) -> TodoItem {
        let mut t = TodoItem::new(format!("audit {step}"), "review");
        t.id = format!("review-{step}");
        t.step_id = step.to_string();
        t
    }

    fn followup_row(id: &str, step: &str) -> TodoItem {
        let mut t = TodoItem::new(id, "source");
        t.id = id.to_string();
        t.step_id = step.to_string();
        t
    }

    /// The todo agent owns `type`, and on the 2026-08-21 mmu.c review
    /// it had dropped it from all 13 surviving group rows: 0 of 448
    /// still carried `kind == "review"`. Only `id` and `step_id` are
    /// restored by `reconcile_update`, so only those may be tested.
    #[test]
    fn a_group_row_is_identified_by_id_not_by_kind() {
        let mut row = group_row("audit-cluster-01");
        assert!(is_survey_group_row(&row));
        row.kind = String::new();
        assert!(is_survey_group_row(&row), "kind must not be load-bearing");

        assert!(!is_survey_group_row(&followup_row(
            "some-followup",
            "audit-cluster-01"
        )));
        // A row with a step but no matching id is a followup.
        let mut orphan = group_row("audit-cluster-02");
        orphan.id = "review-audit-cluster-99".into();
        assert!(!is_survey_group_row(&orphan));
    }

    /// Every survey group is analysed before any followup is.
    /// The goal check is skipped while any group is unfinished, so it
    /// must report that state exactly. Being wrong in the "0" direction
    /// lets a premature goal-met DRAIN retire groups that never ran --
    /// the fairness gate cannot stop that, because a drain is not a
    /// dispatch.
    #[tokio::test]
    async fn outstanding_survey_groups_counts_only_unfinished_groups() {
        let mgr = TaskManager::with_caps(8, 8);
        mgr.seed_todo_if_empty(vec![
            group_row("g1"),
            group_row("g2"),
            followup_row("f1", "g1"),
        ])
        .await;
        assert_eq!(mgr.outstanding_survey_groups().await, 2);

        mgr.mark_todo_done("review-g1").await;
        assert_eq!(mgr.outstanding_survey_groups().await, 1);

        mgr.mark_todo_done("review-g2").await;
        assert_eq!(
            mgr.outstanding_survey_groups().await,
            0,
            "a pending followup is not an unfinished group"
        );
    }

    #[tokio::test]
    async fn a_run_without_survey_groups_reports_nothing_outstanding() {
        // Non-review work must not have its goal checks suppressed.
        let mgr = TaskManager::with_caps(8, 8);
        mgr.seed_todo_if_empty(vec![followup_row("a", ""), followup_row("b", "step")])
            .await;
        assert_eq!(mgr.outstanding_survey_groups().await, 0);
    }

    #[tokio::test]
    async fn followups_wait_until_every_group_has_run() {
        let mgr = TaskManager::with_caps(8, 8);
        mgr.seed_todo_if_empty(vec![
            group_row("g1"),
            group_row("g2"),
            followup_row("f1", "g1"),
            followup_row("f2", "g1"),
        ])
        .await;

        let first = mgr.claim_ranked_todos(9, 0).await;
        let ids: Vec<&str> = first.items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["review-g1", "review-g2"], "groups only");
        assert_eq!(first.remaining, 2, "followups still visible as queued");

        // One group done, one still running: the gate holds.
        mgr.mark_todo_done("review-g1").await;
        assert!(mgr.claim_ranked_todos(9, 0).await.items.is_empty());

        mgr.mark_todo_done("review-g2").await;
        let after = mgr.claim_ranked_todos(9, 0).await;
        let ids: Vec<&str> = after.items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["f1", "f2"]);
    }

    /// A gate that cannot open is a deadlock. With no group row left
    /// able to make progress, followups must dispatch.
    #[tokio::test]
    async fn the_gate_lifts_when_no_group_row_can_progress() {
        let mgr = TaskManager::with_caps(8, 8);
        let mut blocked = group_row("g1");
        blocked.status = TodoStatus::Blocked;
        mgr.seed_todo_if_empty(vec![blocked, followup_row("f1", "g1")])
            .await;

        let claims = mgr.claim_ranked_todos(9, 0).await;
        let ids: Vec<&str> = claims.items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["f1"],
            "a permanently-blocked group must not stall the run"
        );
    }

    /// A Pending group row whose dependency can never be met keeps
    /// the Pending status -- it never becomes TodoStatus::Blocked --
    /// so a gate that only looked at the status would hold shut
    /// forever on a row that is never claimable.
    #[tokio::test]
    async fn the_gate_lifts_when_a_group_row_is_dependency_deadlocked() {
        let mgr = TaskManager::with_caps(8, 8);
        let mut stuck = group_row("g1");
        stuck.depends_on = vec!["a-row-that-does-not-exist".into()];
        mgr.seed_todo_if_empty(vec![stuck, followup_row("f1", "g1")])
            .await;

        let claims = mgr.claim_ranked_todos(9, 0).await;
        let ids: Vec<&str> = claims.items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["f1"]);
    }

    /// ...but a dependency that CAN still be met must hold the gate,
    /// or the guarantee is worthless.
    #[tokio::test]
    async fn a_satisfiable_group_dependency_still_holds_the_gate() {
        let mgr = TaskManager::with_caps(8, 8);
        let mut later = group_row("g2");
        later.depends_on = vec!["review-g1".into()];
        mgr.seed_todo_if_empty(vec![group_row("g1"), later, followup_row("f1", "g1")])
            .await;

        let first = mgr.claim_ranked_todos(9, 0).await;
        let ids: Vec<&str> = first.items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["review-g1"], "g2 blocked, f1 gated");

        mgr.mark_todo_done("review-g1").await;
        let second = mgr.claim_ranked_todos(9, 0).await;
        let ids: Vec<&str> = second.items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["review-g2"], "g2 now runnable, f1 still gated");

        mgr.mark_todo_done("review-g2").await;
        let third = mgr.claim_ranked_todos(9, 0).await;
        assert_eq!(third.items.len(), 1);
        assert_eq!(third.items[0].id, "f1");
    }

    /// No survey groups at all -- /fix, /triage, an operator prompt --
    /// means no gate.
    #[tokio::test]
    async fn a_run_without_survey_groups_is_never_gated() {
        let mgr = TaskManager::with_caps(8, 8);
        mgr.seed_todo_if_empty(vec![followup_row("f1", ""), followup_row("f2", "")])
            .await;
        assert_eq!(mgr.claim_ranked_todos(9, 0).await.items.len(), 2);
    }

    /// Equal share per group, so a region that keeps producing
    /// followups cannot take every slot from one that produced few.
    /// On the 2026-08-21 review audit-cluster-05 held 119 followups
    /// against audit-cluster-10's 21.
    #[tokio::test]
    async fn dispatch_is_shared_equally_across_groups() {
        let mgr = TaskManager::with_caps(8, 8);
        let mut rows = vec![group_row("busy"), group_row("quiet")];
        for n in 0..6 {
            rows.push(followup_row(&format!("busy-{n}"), "busy"));
        }
        rows.push(followup_row("quiet-0", "quiet"));
        rows.push(followup_row("quiet-1", "quiet"));
        mgr.seed_todo_if_empty(rows).await;

        // Clear the gate.
        mgr.claim_ranked_todos(9, 0).await;
        mgr.mark_todo_done("review-busy").await;
        mgr.mark_todo_done("review-quiet").await;

        // Four slots must not all go to the group with six rows.
        let wave = mgr.claim_ranked_todos(4, 0).await;
        let quiet = wave.items.iter().filter(|i| i.step_id == "quiet").count();
        assert_eq!(
            quiet, 2,
            "both quiet rows run before busy takes a third slot"
        );
        assert_eq!(wave.items.len(), 4);

        // Once quiet is drained it stops competing and busy proceeds.
        let next = mgr.claim_ranked_todos(4, 0).await;
        assert!(next.items.iter().all(|i| i.step_id == "busy"));
    }

    /// `/clear` wipes the todo list and the ranking for the same
    /// reason it must wipe the fairness counts: step ids restart at
    /// 01 for every coverage plan, so a surviving count would hand the
    /// next topic's first group a dispatch debt it never incurred.
    #[tokio::test]
    async fn clearing_the_session_resets_the_fairness_counts() {
        let mgr = TaskManager::with_caps(8, 8);
        mgr.seed_todo_if_empty(vec![group_row("g1"), followup_row("f1", "g1")])
            .await;
        mgr.claim_ranked_todos(9, 0).await;
        mgr.clear_session_work().await;

        // A fresh topic reusing the same step id must start level with
        // a brand-new one.
        mgr.seed_todo_if_empty(vec![
            group_row("g1"),
            group_row("g2"),
            followup_row("a", "g1"),
            followup_row("b", "g2"),
        ])
        .await;
        mgr.claim_ranked_todos(9, 0).await;
        mgr.mark_todo_done("review-g1").await;
        mgr.mark_todo_done("review-g2").await;
        let wave = mgr.claim_ranked_todos(1, 0).await;
        assert_eq!(wave.items.len(), 1);
        assert_eq!(
            wave.items[0].step_id, "g1",
            "g1 must not be penalised for the cleared session's claims"
        );
    }

    #[tokio::test]
    async fn start_budget_cannot_deadlock_when_nothing_can_be_reaped() {
        // A claim that never becomes a task — `submit_prompt_inner`
        // returning false leaves the row InProgress with no executor —
        // spends budget and leaves nothing to reap. If the budget
        // still blocked in that state the session would be stuck with
        // zero tasks running and no way back, not even via /continue.
        let mgr = TaskManager::with_max_parallel(2);
        let rows: Vec<TodoItem> = (0..6)
            .map(|i| {
                let mut t = TodoItem::new(format!("row{i}"), "review");
                t.id = format!("row{i}");
                t
            })
            .collect();
        mgr.replace_todo(rows).await;
        assert_eq!(mgr.claim_ranked_todos(9, 0).await.items.len(), 2);
        // Budget is spent, but no task was ever spawned.
        assert!(mgr.snapshot().await.is_empty());
        assert_eq!(
            mgr.start_budget().await,
            usize::MAX,
            "with nothing to reap the budget must not be the blocker"
        );
        assert_eq!(mgr.claim_ranked_todos(9, 0).await.items.len(), 2);
    }

    #[tokio::test]
    async fn unbounded_parallelism_has_no_start_budget() {
        let mgr = TaskManager::new();
        assert_eq!(mgr.start_budget().await, usize::MAX);
        let rows: Vec<TodoItem> = (0..40)
            .map(|i| {
                let mut t = TodoItem::new(format!("row{i}"), "review");
                t.id = format!("row{i}");
                t
            })
            .collect();
        mgr.replace_todo(rows).await;
        assert_eq!(mgr.claim_ranked_todos(usize::MAX, 0).await.items.len(), 40);
    }

    #[tokio::test]
    async fn ranked_dispatch_cannot_overshoot_the_turn_budget_while_tasks_run() {
        // Dispatch now fires while tasks are still running, so the
        // turn budget has to subtract in-flight work — otherwise two
        // dispatches inside one session each see the full remainder.
        let mgr = TaskManager::new();
        let row = |id: &str| {
            let mut t = TodoItem::new(id, "review");
            t.id = id.to_string();
            t
        };
        mgr.replace_todo(vec![row("a"), row("b"), row("c"), row("d")])
            .await;
        // Budget 3. First dispatch takes two of them and starts them.
        let first = mgr.claim_ranked_todos(2, 3).await;
        assert_eq!(first.items.len(), 2);
        let hold = Arc::new(tokio::sync::Notify::new());
        for _ in 0..2 {
            let held = hold.clone();
            mgr.spawn("running", None, move |_| async move {
                held.notified().await;
                Ok(TaskOutcome::default())
            })
            .await;
        }
        // Two are in flight against a budget of 3, so only one more
        // may start even though two rows are still ready.
        let second = mgr.claim_ranked_todos(10, 3).await;
        assert_eq!(
            second.items.len(),
            1,
            "in-flight tasks must consume the remaining turn budget"
        );
        hold.notify_waiters();
    }

    /// The reaper marks a row done BEFORE the todo agent is asked to
    /// describe it, so the placeholder is always in place by the time
    /// the real sentence arrives. If write-once treats the placeholder
    /// as settled evidence, the agent's answer is discarded every
    /// time — 74 of 74 rows on the 2026-08-07 fair.c review — and the
    /// dedup step that reads coverage goes blind.
    #[tokio::test]
    async fn a_real_coverage_sentence_replaces_the_placeholder() {
        let mgr = TaskManager::new();
        let mut live = TodoItem::new("audit thing", "review");
        live.id = "review-audit-thing".into();
        mgr.replace_todo(vec![live]).await;

        // Reaper publishes the completion first.
        assert!(mgr.mark_todo_done("review-audit-thing").await);
        assert_eq!(
            mgr.todo_snapshot().await[0].coverage,
            crate::PLACEHOLDER_COVERAGE,
            "the fallback keeps the row visible to dedup in the meantime"
        );

        // Then the todo agent returns what the task actually examined.
        let snapshot = mgr.todo_snapshot().await;
        let mut described = snapshot[0].clone();
        described.coverage = "Examined avg_vruntime and place_entity in fair.c".into();
        mgr.merge_inferred_state(InferredTodoUpdate {
            items: vec![described],
            completed_todo_ids: vec!["review-audit-thing".into()],
            inference_snapshot: snapshot,
            plan_rewrite: None,
        })
        .await;

        let after = mgr.todo_snapshot().await;
        assert_eq!(after[0].status, TodoStatus::Done);
        assert_eq!(
            after[0].coverage,
            "Examined avg_vruntime and place_entity in fair.c"
        );
    }

    /// Write-once still holds for genuine coverage: a later round may
    /// not paraphrase settled evidence.
    #[tokio::test]
    async fn real_coverage_is_not_overwritten_by_a_later_round() {
        let mgr = TaskManager::new();
        let mut live = TodoItem::new("audit thing", "review");
        live.id = "review-audit-thing".into();
        live.status = TodoStatus::Done;
        live.coverage = "First, settled description".into();
        mgr.replace_todo(vec![live]).await;

        let snapshot = mgr.todo_snapshot().await;
        let mut reworded = snapshot[0].clone();
        reworded.coverage = "Second, competing description".into();
        mgr.merge_inferred_state(InferredTodoUpdate {
            items: vec![reworded],
            completed_todo_ids: vec!["review-audit-thing".into()],
            inference_snapshot: snapshot,
            plan_rewrite: None,
        })
        .await;
        assert_eq!(
            mgr.todo_snapshot().await[0].coverage,
            "First, settled description"
        );
    }

    #[tokio::test]
    async fn reset_in_progress_flips_only_inprogress() {
        let mgr = TaskManager::new();
        let mut a = TodoItem::new("a", "investigate");
        a.status = TodoStatus::InProgress;
        let mut b = TodoItem::new("b", "investigate");
        b.status = TodoStatus::Pending;
        let mut c = TodoItem::new("c", "investigate");
        c.status = TodoStatus::Done;
        let mut d = TodoItem::new("d", "investigate");
        d.status = TodoStatus::InProgress;
        mgr.replace_todo(vec![a, b, c, d]).await;
        let flipped = mgr.reset_in_progress_to_pending().await;
        assert_eq!(flipped, 2);
        let snap = mgr.todo_snapshot().await;
        assert_eq!(snap[0].status, TodoStatus::Pending);
        assert_eq!(snap[1].status, TodoStatus::Pending);
        assert_eq!(snap[2].status, TodoStatus::Done);
        assert_eq!(snap[3].status, TodoStatus::Pending);
    }

    #[tokio::test]
    async fn inferred_todo_cannot_redispatch_work_started_after_snapshot() {
        let mgr = TaskManager::new();
        let mut live = TodoItem::new("trace mmap", "review");
        live.id = "review-mmap".into();
        live.status = TodoStatus::InProgress;
        mgr.replace_todo(vec![live]).await;

        let mut stale = TodoItem::new("trace mmap", "review");
        stale.id = "review-mmap".into();
        stale.status = TodoStatus::Pending;
        mgr.merge_inferred_state(InferredTodoUpdate {
            items: vec![stale],
            completed_todo_ids: Vec::new(),
            inference_snapshot: vec![],
            plan_rewrite: None,
        })
        .await;

        assert_eq!(mgr.todo_snapshot().await[0].status, TodoStatus::InProgress);
    }

    #[tokio::test]
    async fn inferred_todo_cannot_reopen_completed_sibling() {
        let mgr = TaskManager::new();
        let mut live = TodoItem::new("trace read", "review");
        live.id = "review-read".into();
        live.status = TodoStatus::Done;
        live.coverage = "verified read path".into();
        mgr.replace_todo(vec![live]).await;

        let mut stale = TodoItem::new("trace read", "review");
        stale.id = "review-read".into();
        stale.status = TodoStatus::InProgress;
        mgr.merge_inferred_state(InferredTodoUpdate {
            items: vec![stale],
            completed_todo_ids: Vec::new(),
            inference_snapshot: vec![],
            plan_rewrite: None,
        })
        .await;

        let snapshot = mgr.todo_snapshot().await;
        let item = &snapshot[0];
        assert_eq!(item.status, TodoStatus::Done);
        assert_eq!(item.coverage, "verified read path");
    }

    #[tokio::test]
    async fn inferred_todo_cannot_remove_live_dependencies_or_step_link() {
        let mgr = TaskManager::new();
        let mut live = TodoItem::new("cross-check", "review");
        live.id = "cross-check".into();
        live.step_id = "final-review".into();
        live.depends_on = vec!["broad-pass".into(), "new-followup".into()];
        let prerequisites: Vec<TodoItem> = ["broad-pass", "new-followup", "another-followup"]
            .into_iter()
            .map(|id| {
                let mut item = TodoItem::new(id, "source");
                item.id = id.into();
                item
            })
            .collect();
        let mut inference_snapshot = vec![live.clone()];
        inference_snapshot.extend(prerequisites.clone());
        mgr.replace_todo(inference_snapshot.clone()).await;

        let mut proposed = TodoItem::new("cross-check", "review");
        proposed.id = "cross-check".into();
        proposed.step_id = "old-step".into();
        proposed.depends_on = vec!["broad-pass".into(), "another-followup".into()];
        mgr.merge_inferred_state(InferredTodoUpdate {
            items: std::iter::once(proposed).chain(prerequisites).collect(),
            completed_todo_ids: Vec::new(),
            inference_snapshot,
            plan_rewrite: None,
        })
        .await;

        let snapshot = mgr.todo_snapshot().await;
        let item = snapshot
            .iter()
            .find(|item| item.id == "cross-check")
            .unwrap();
        assert_eq!(item.step_id, "final-review");
        assert_eq!(
            item.depends_on,
            vec!["broad-pass", "new-followup", "another-followup"]
        );
    }

    #[tokio::test]
    async fn inferred_todo_drops_missing_self_and_cyclic_dependencies() {
        let mgr = TaskManager::new();
        let mut first = TodoItem::new("first", "review");
        first.id = "first".into();
        first.depends_on = vec!["second".into(), "missing".into(), "first".into()];
        let mut second = TodoItem::new("second", "review");
        second.id = "second".into();
        second.depends_on = vec!["first".into()];

        mgr.merge_inferred_state(InferredTodoUpdate {
            items: vec![first, second],
            completed_todo_ids: Vec::new(),
            inference_snapshot: Vec::new(),
            plan_rewrite: None,
        })
        .await;

        let snapshot = mgr.todo_snapshot().await;
        assert_eq!(snapshot[0].depends_on, vec!["second"]);
        assert!(snapshot[1].depends_on.is_empty());
    }

    #[tokio::test]
    async fn proposed_reverse_edge_cannot_displace_live_dependency() {
        let mgr = TaskManager::new();
        let mut first = TodoItem::new("first", "review");
        first.id = "first".into();
        first.depends_on = vec!["second".into()];
        let mut second = TodoItem::new("second", "review");
        second.id = "second".into();
        let inference_snapshot = vec![first.clone(), second.clone()];
        mgr.replace_todo(inference_snapshot.clone()).await;

        second.depends_on = vec!["first".into()];
        mgr.merge_inferred_state(InferredTodoUpdate {
            items: vec![second, first],
            completed_todo_ids: Vec::new(),
            inference_snapshot,
            plan_rewrite: None,
        })
        .await;

        let snapshot = mgr.todo_snapshot().await;
        let first = snapshot.iter().find(|item| item.id == "first").unwrap();
        let second = snapshot.iter().find(|item| item.id == "second").unwrap();
        assert_eq!(first.depends_on, vec!["second"]);
        assert!(second.depends_on.is_empty());
    }

    #[tokio::test]
    async fn omitted_prerequisite_and_live_edge_survive_inference() {
        let mgr = TaskManager::new();
        let mut prerequisite = TodoItem::new("prerequisite", "source");
        prerequisite.id = "prerequisite".into();
        let mut dependent = TodoItem::new("dependent", "review");
        dependent.id = "dependent".into();
        dependent.depends_on = vec!["prerequisite".into()];
        let inference_snapshot = vec![prerequisite.clone(), dependent.clone()];
        mgr.replace_todo(inference_snapshot.clone()).await;

        mgr.merge_inferred_state(InferredTodoUpdate {
            items: vec![dependent],
            completed_todo_ids: Vec::new(),
            inference_snapshot,
            plan_rewrite: None,
        })
        .await;

        let snapshot = mgr.todo_snapshot().await;
        assert!(snapshot.iter().any(|item| item.id == "prerequisite"));
        let dependent = snapshot.iter().find(|item| item.id == "dependent").unwrap();
        assert_eq!(dependent.depends_on, vec!["prerequisite"]);
    }

    #[tokio::test]
    async fn inferred_todo_marks_just_reaped_task_done() {
        let mgr = TaskManager::new();
        let mut live = TodoItem::new("trace write", "review");
        live.id = "review-write".into();
        live.status = TodoStatus::InProgress;
        mgr.replace_todo(vec![live.clone()]).await;

        mgr.merge_inferred_state(InferredTodoUpdate {
            items: vec![live],
            completed_todo_ids: vec!["review-write".into()],
            inference_snapshot: vec![],
            plan_rewrite: None,
        })
        .await;

        let snapshot = mgr.todo_snapshot().await;
        let item = &snapshot[0];
        assert_eq!(item.status, TodoStatus::Done);
        assert_eq!(item.coverage, "completed by the reaped task");
    }

    #[tokio::test]
    async fn inferred_todo_preserves_pending_item_added_after_snapshot() {
        let mgr = TaskManager::new();
        let mut original = TodoItem::new("original", "review");
        original.id = "original".into();
        let inference_snapshot = vec![original.clone()];

        let mut concurrent = TodoItem::new("new followup", "question");
        concurrent.id = "new-followup".into();
        mgr.replace_todo(vec![original.clone(), concurrent]).await;
        mgr.merge_inferred_state(InferredTodoUpdate {
            items: vec![original],
            completed_todo_ids: Vec::new(),
            inference_snapshot,
            plan_rewrite: None,
        })
        .await;

        let snapshot = mgr.todo_snapshot().await;
        assert!(snapshot.iter().any(|item| item.id == "new-followup"));
    }

    #[tokio::test]
    async fn inferred_todo_can_dedup_unchanged_unreferenced_pending_item() {
        let mgr = TaskManager::new();
        let mut removable = TodoItem::new("duplicate followup", "question");
        removable.id = "duplicate-followup".into();
        let inference_snapshot = vec![removable.clone()];
        mgr.replace_todo(vec![removable]).await;

        mgr.merge_inferred_state(InferredTodoUpdate {
            items: Vec::new(),
            completed_todo_ids: Vec::new(),
            inference_snapshot,
            plan_rewrite: None,
        })
        .await;

        assert!(mgr.todo_snapshot().await.is_empty());
    }

    #[tokio::test]
    async fn inferred_todo_cannot_remove_pending_item_changed_after_snapshot() {
        let mgr = TaskManager::new();
        let mut snapshot_item = TodoItem::new("followup", "question");
        snapshot_item.id = "followup".into();
        let mut live = snapshot_item.clone();
        live.reason = "concurrently refined reason".into();
        mgr.replace_todo(vec![live]).await;

        mgr.merge_inferred_state(InferredTodoUpdate {
            items: Vec::new(),
            completed_todo_ids: Vec::new(),
            inference_snapshot: vec![snapshot_item],
            plan_rewrite: None,
        })
        .await;

        assert_eq!(mgr.todo_snapshot().await[0].id, "followup");
    }

    #[tokio::test]
    async fn inferred_todo_cannot_resurrect_concurrently_removed_item() {
        let mgr = TaskManager::new();
        let mut removed = TodoItem::new("operator removed", "question");
        removed.id = "operator-removed".into();
        let inference_snapshot = vec![removed.clone()];
        mgr.replace_todo(Vec::new()).await;

        mgr.merge_inferred_state(InferredTodoUpdate {
            items: vec![removed],
            completed_todo_ids: Vec::new(),
            inference_snapshot,
            plan_rewrite: None,
        })
        .await;

        assert!(mgr.todo_snapshot().await.is_empty());
    }

    #[tokio::test]
    async fn append_todo_unique_preserves_live_execution_state() {
        let mgr = TaskManager::new();
        let mut running = TodoItem::new("running", "review");
        running.id = "running".into();
        running.status = TodoStatus::InProgress;
        mgr.replace_todo(vec![running.clone()]).await;

        let duplicate = running;
        let mut added = TodoItem::new("new followup", "question");
        added.id = "new-followup".into();
        assert_eq!(mgr.append_todo_unique(vec![duplicate, added]).await, 1);

        let snapshot = mgr.todo_snapshot().await;
        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot[0].status, TodoStatus::InProgress);
    }

    fn row(id: &str, deps: &[&str]) -> TodoItem {
        let mut item = TodoItem::new(format!("audit {id}"), "review");
        item.id = id.to_string();
        item.depends_on = deps.iter().map(|d| d.to_string()).collect();
        item
    }

    /// The snapshot must show exactly what a claim would have taken,
    /// so the prioritizer ranks the real candidate set — and it must
    /// not claim anything, since an LLM round-trip happens before the
    /// picks come back.
    #[tokio::test]
    async fn ready_snapshot_sees_dispatchable_rows_without_claiming() {
        let mgr = TaskManager::new();
        let mut finished = row("finished", &[]);
        finished.status = TodoStatus::Done;
        let mut running = row("running", &[]);
        running.status = TodoStatus::InProgress;
        mgr.replace_todo(vec![
            finished,
            running,
            row("ready-a", &[]),
            row("ready-b", &["finished"]),
            row("blocked", &["ready-a"]),
        ])
        .await;

        let ready = mgr.ready_pending_snapshot(0).await;
        let ids: Vec<&str> = ready.items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["ready-a", "ready-b"]);
        assert_eq!(ready.blocked, 1);
        assert_eq!(ready.budget, usize::MAX, "--turns 0 is unlimited");

        for item in mgr.todo_snapshot().await {
            assert_ne!(
                (item.id.as_str(), item.status),
                ("ready-a", TodoStatus::InProgress),
                "the snapshot must not claim"
            );
        }
    }

    /// The prioritizer returns ids in rank order; the claim must
    /// honour that order rather than the list's.
    #[tokio::test]
    async fn selected_todos_are_claimed_in_the_order_given() {
        let mgr = TaskManager::new();
        mgr.replace_todo(vec![row("a", &[]), row("b", &[]), row("c", &[])])
            .await;

        let claimed = mgr.claim_selected_todos(&["c".into(), "a".into()], 0).await;
        let ids: Vec<&str> = claimed.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["c", "a"]);

        let after = mgr.todo_snapshot().await;
        let status = |id: &str| after.iter().find(|i| i.id == id).unwrap().status;
        assert_eq!(status("a"), TodoStatus::InProgress);
        assert_eq!(status("b"), TodoStatus::Pending, "unpicked stays pending");
        assert_eq!(status("c"), TodoStatus::InProgress);
    }

    /// The ranking call happens outside the lock, so a row can stop
    /// being dispatchable between snapshot and claim. Skip it rather
    /// than double-dispatching or resurrecting finished work.
    #[tokio::test]
    async fn claim_skips_rows_that_stopped_being_ready() {
        let mgr = TaskManager::new();
        mgr.replace_todo(vec![
            row("taken", &[]),
            row("gated", &["never-done"]),
            row("fine", &[]),
        ])
        .await;
        mgr.set_todo_status_for_test("taken", TodoStatus::InProgress)
            .await;

        let claimed = mgr
            .claim_selected_todos(
                &[
                    "taken".into(),
                    "gated".into(),
                    "ghost".into(),
                    "fine".into(),
                ],
                0,
            )
            .await;
        let ids: Vec<&str> = claimed.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["fine"]);
    }

    /// A finite --turns budget bounds the ranked claim exactly as it
    /// bounds the unranked one; the prioritizer cannot buy extra runs.
    #[tokio::test]
    async fn selected_claim_respects_the_turn_budget() {
        let mgr = TaskManager::new();
        mgr.replace_todo(vec![row("a", &[]), row("b", &[]), row("c", &[])])
            .await;

        assert_eq!(mgr.ready_pending_snapshot(2).await.budget, 2);
        let claimed = mgr
            .claim_selected_todos(&["a".into(), "b".into(), "c".into()], 2)
            .await;
        assert_eq!(claimed.len(), 2);
    }

    #[tokio::test]
    async fn append_todo_unique_keeps_distinct_followup_kinds() {
        let mgr = TaskManager::new();
        let source = TodoItem::new("folio_end_read", "source");
        let callers = TodoItem::new("folio_end_read", "callers");

        assert_eq!(mgr.append_todo_unique(vec![source, callers]).await, 2);
        let snapshot = mgr.todo_snapshot().await;
        assert_eq!(snapshot.len(), 2);
        assert_ne!(snapshot[0].kind, snapshot[1].kind);
        assert_ne!(snapshot[0].id, snapshot[1].id);
        mgr.set_todo_status_for_test(&snapshot[1].id, TodoStatus::InProgress)
            .await;
        let snapshot = mgr.todo_snapshot().await;
        assert_eq!(snapshot[0].status, TodoStatus::Pending);
        assert_eq!(snapshot[1].status, TodoStatus::InProgress);
    }

    #[tokio::test]
    async fn remove_todo_does_not_replace_sibling_state() {
        let mgr = TaskManager::new();
        let removable = TodoItem::new("remove", "question");
        let mut running = TodoItem::new("running", "review");
        running.status = TodoStatus::InProgress;
        mgr.replace_todo(vec![removable, running]).await;

        assert!(mgr.remove_todo("remove").await);
        let snapshot = mgr.todo_snapshot().await;
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].status, TodoStatus::InProgress);
    }

    #[tokio::test]
    async fn remove_todo_scrubs_dependency_edges() {
        let mgr = TaskManager::new();
        let mut prerequisite = TodoItem::new("prerequisite", "source");
        prerequisite.id = "prerequisite-id".into();
        let mut dependent = TodoItem::new("dependent", "review");
        dependent.id = "dependent".into();
        dependent.depends_on = vec!["prerequisite-id".into(), "other".into()];
        mgr.replace_todo(vec![prerequisite, dependent]).await;

        assert!(mgr.remove_todo("prerequisite-id").await);
        assert_eq!(mgr.todo_snapshot().await[0].depends_on, vec!["other"]);
    }

    #[tokio::test]
    async fn turn_limited_claim_reserves_only_remaining_runs() {
        let mgr = TaskManager::new();
        let todo = (0..5)
            .map(|n| {
                let mut item = TodoItem::new(format!("todo-{n}"), "review");
                item.id = format!("todo-{n}");
                item
            })
            .collect();
        mgr.load_runtime_state(todo, Vec::new(), None, 8).await;

        let claims = mgr.claim_ready_todos_with_turn_limit(10, 10).await;
        assert_eq!(claims.items.len(), 2);
        assert_eq!(claims.remaining, 3);
    }

    #[tokio::test]
    async fn mark_todo_done_publishes_completion_without_inference() {
        let mgr = TaskManager::new();
        let mut item = TodoItem::new("finished", "review");
        item.id = "finished-id".into();
        item.status = TodoStatus::InProgress;
        mgr.replace_todo(vec![item]).await;

        assert!(mgr.mark_todo_done("finished-id").await);
        let item = &mgr.todo_snapshot().await[0];
        assert_eq!(item.status, TodoStatus::Done);
        assert_eq!(item.coverage, "completed by the reaped task");
    }

    #[tokio::test]
    async fn inferred_todo_and_plan_rewrite_publish_one_consistent_generation() {
        use crate::plan::{Plan, PlanRewrite, PlanStep, PlanStepStatus};

        let mgr = TaskManager::new();
        let mut plan = Plan::new("p", "g", crate::TaskMode::Audit);
        plan.steps.push(PlanStep::new("review", "Review"));
        mgr.set_plan(Some(plan)).await;
        let mut live = TodoItem::new("review", "review");
        live.id = "review-todo".into();
        live.step_id = "review".into();
        live.status = TodoStatus::InProgress;
        mgr.replace_todo(vec![live.clone()]).await;

        let rewrite = PlanRewrite {
            steps: vec![PlanStep::new("review", "Review updated")],
        };
        let change = mgr
            .merge_inferred_state(InferredTodoUpdate {
                items: vec![live],
                completed_todo_ids: vec!["review-todo".into()],
                inference_snapshot: vec![],
                plan_rewrite: Some(rewrite),
            })
            .await;

        assert!(change.is_some());
        let (plan, todo, _, _) = mgr.sync_and_snapshot_runtime().await;
        assert_eq!(todo[0].status, TodoStatus::Done);
        assert_eq!(plan.unwrap().steps[0].status, PlanStepStatus::Done);
    }

    #[tokio::test]
    async fn plan_rewrite_cannot_mark_pending_scheduler_work_done() {
        use crate::plan::{Plan, PlanRewrite, PlanStep, PlanStepStatus};

        let mgr = TaskManager::new();
        let mut plan = Plan::new("p", "g", crate::TaskMode::Audit);
        plan.steps.push(PlanStep::new("review", "Review"));
        mgr.set_plan(Some(plan)).await;
        let mut todo = TodoItem::new("review", "review");
        todo.id = "review-todo".into();
        todo.step_id = "review".into();
        mgr.replace_todo(vec![todo]).await;

        let mut rewritten = PlanStep::new("review", "Review updated");
        rewritten.status = PlanStepStatus::Done;
        let change = mgr
            .apply_plan_rewrite(PlanRewrite {
                steps: vec![rewritten],
            })
            .await;

        assert_eq!(change.current.steps[0].status, PlanStepStatus::Pending);
    }

    #[tokio::test]
    async fn deferred_move_and_session_snapshot_are_one_generation() {
        let mgr = TaskManager::new();
        let pending = TodoItem::new("pending", "review");
        let mut done = TodoItem::new("done", "review");
        done.status = TodoStatus::Done;
        mgr.replace_todo(vec![pending, done]).await;

        assert_eq!(mgr.defer_pending().await, 1);
        let (_, active, deferred, _) = mgr.sync_and_snapshot_runtime().await;
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].status, TodoStatus::Done);
        assert_eq!(deferred.len(), 1);
        assert_eq!(deferred[0].name, "pending");
    }

    #[tokio::test]
    async fn defer_pending_keeps_terminal_items() {
        // Goal-met / --turns drains used to wipe the todo list via
        // replace_todo(Vec::new()), erasing Done items' step_id
        // linkage so the plan could never roll up to Done. The new
        // drain keeps Done/Skipped on the list.
        let mgr = TaskManager::new();
        let mut a = TodoItem::new("a", "investigate");
        a.status = TodoStatus::Pending;
        let mut b = TodoItem::new("b", "investigate");
        b.status = TodoStatus::Blocked;
        let mut c = TodoItem::new("c", "investigate");
        c.status = TodoStatus::Done;
        let mut d = TodoItem::new("d", "investigate");
        d.status = TodoStatus::Skipped;
        mgr.replace_todo(vec![a, b, c, d]).await;
        assert_eq!(mgr.defer_pending().await, 2);
        let drained = mgr.deferred_snapshot().await;
        let drained_names: Vec<_> = drained.iter().map(|i| i.name.clone()).collect();
        assert_eq!(drained_names, vec!["a".to_string(), "b".to_string()]);
        let snap = mgr.todo_snapshot().await;
        let kept: Vec<_> = snap.iter().map(|i| i.name.clone()).collect();
        assert_eq!(kept, vec!["c".to_string(), "d".to_string()]);
    }

    #[tokio::test]
    async fn drain_preserves_step_id_linkage_for_plan_rollup() {
        // End-to-end guard for the bug that inspired the drain
        // change: a step with two linked todos, one done / one
        // pending. Pre-fix the pending todo drained AND the done
        // todo was wiped, leaving the step pending forever. Post-
        // fix the done todo stays, sync_plan_from_todo sees a
        // fully-terminal linkage, and the step flips to Done.
        use crate::plan::{Plan, PlanStep, PlanStepStatus};
        let mgr = TaskManager::new();
        let mut plan = Plan::new("p", "g", crate::TaskMode::Audit);
        plan.steps.push(PlanStep::new("s1", "audit"));
        mgr.set_plan(Some(plan)).await;
        let mut a = TodoItem::new("a", "investigate");
        a.step_id = "s1".into();
        a.status = TodoStatus::Done;
        let mut b = TodoItem::new("b", "investigate");
        b.step_id = "s1".into();
        b.status = TodoStatus::Pending;
        mgr.replace_todo(vec![a, b]).await;
        assert_eq!(mgr.defer_pending().await, 1);
        assert_eq!(mgr.deferred_snapshot().await[0].name, "b");
        mgr.sync_plan_from_todo().await;
        let out = mgr.plan_snapshot().await.unwrap();
        assert_eq!(out.steps[0].status, PlanStepStatus::Done);
    }

    #[tokio::test]
    async fn set_and_sync_plan_marks_step_done_when_todos_terminal() {
        use crate::plan::{Plan, PlanStep, PlanStepStatus};
        let mgr = TaskManager::new();
        let mut plan = Plan::new("p", "g", crate::TaskMode::Audit);
        let mut step = PlanStep::new("s1", "t");
        step.todo_ids = vec!["a".into(), "b".into()];
        plan.steps.push(step);
        mgr.set_plan(Some(plan)).await;
        let mut a = TodoItem::new("a", "investigate");
        a.status = TodoStatus::Done;
        let mut b = TodoItem::new("b", "investigate");
        b.status = TodoStatus::Skipped;
        mgr.replace_todo(vec![a, b]).await;
        mgr.sync_plan_from_todo().await;
        let out = mgr.plan_snapshot().await.unwrap();
        assert_eq!(out.steps[0].status, PlanStepStatus::Done);
    }

    #[tokio::test]
    async fn set_plan_strips_orphan_step_ids_from_todos() {
        // When the slow or todo agent rewrites the plan and drops
        // a step id the new plan no longer owns, existing todos
        // pointing at the dead id must be cleared so they are not
        // stranded. The todo's step_id goes back to empty and the
        // todo-agent's next turn re-links it against the new plan.
        use crate::plan::{Plan, PlanStep};
        let mgr = TaskManager::new();
        let mut old_plan = Plan::new("p", "g", crate::TaskMode::Audit);
        old_plan.steps.push(PlanStep::new("s1", "old-one"));
        old_plan.steps.push(PlanStep::new("s2", "old-two"));
        mgr.set_plan(Some(old_plan)).await;
        let mut a = TodoItem::new("a", "investigate");
        a.step_id = "s1".into();
        let mut b = TodoItem::new("b", "investigate");
        b.step_id = "s2".into();
        let c = TodoItem::new("c", "investigate"); // empty step_id
        mgr.replace_todo(vec![a, b, c]).await;

        // New plan drops s2, keeps s1, adds s3.
        let mut new_plan = Plan::new("p", "g", crate::TaskMode::Audit);
        new_plan.steps.push(PlanStep::new("s1", "new-one"));
        new_plan.steps.push(PlanStep::new("s3", "new-three"));
        mgr.set_plan(Some(new_plan)).await;

        let snap = mgr.todo_snapshot().await;
        assert_eq!(snap[0].step_id, "s1"); // still valid, preserved
        assert_eq!(snap[1].step_id, ""); // s2 dead, cleared
        assert_eq!(snap[2].step_id, ""); // was empty, unchanged
    }

    #[tokio::test]
    async fn set_plan_none_clears_every_step_id() {
        use crate::plan::{Plan, PlanStep};
        let mgr = TaskManager::new();
        let mut plan = Plan::new("p", "g", crate::TaskMode::Audit);
        plan.steps.push(PlanStep::new("s1", "x"));
        mgr.set_plan(Some(plan)).await;
        let mut a = TodoItem::new("a", "investigate");
        a.step_id = "s1".into();
        mgr.replace_todo(vec![a]).await;
        mgr.set_plan(None).await;
        assert_eq!(mgr.todo_snapshot().await[0].step_id, "");
    }

    #[tokio::test]
    async fn claim_ready_todos_transfers_only_ready_rows_atomically() {
        let mgr = TaskManager::new();
        let mut done = TodoItem::new("finished", "investigate");
        done.id = "done-id".into();
        done.status = TodoStatus::Done;
        let mut ready = TodoItem::new("ready", "investigate");
        ready.id = "ready-id".into();
        ready.depends_on.push("done-id".into());
        let mut blocked = TodoItem::new("blocked", "investigate");
        blocked.id = "blocked-id".into();
        blocked.depends_on.push("missing-id".into());
        mgr.replace_todo(vec![done, ready, blocked]).await;

        let claims = mgr.claim_ready_todos(1).await;
        assert_eq!(claims.items.len(), 1);
        assert_eq!(claims.items[0].id, "ready-id");
        assert_eq!(claims.blocked, 1);
        assert_eq!(claims.remaining, 0);
        let todo = mgr.todo_snapshot().await;
        assert_eq!(todo[1].status, TodoStatus::InProgress);
        assert_eq!(todo[2].status, TodoStatus::Pending);

        let second = mgr.claim_ready_todos(1).await;
        assert!(second.items.is_empty());
        assert_eq!(second.blocked, 1);
    }

    #[tokio::test]
    async fn repeated_defer_deduplicates_the_deferred_ledger() {
        let mgr = TaskManager::new();
        let mut item = TodoItem::new("same", "investigate");
        item.id = "stable-id".into();
        mgr.replace_todo(vec![item.clone()]).await;
        assert_eq!(mgr.defer_pending().await, 1);
        assert_eq!(mgr.append_todo_unique(vec![item]).await, 1);
        assert_eq!(mgr.defer_pending().await, 1);
        assert_eq!(mgr.deferred_snapshot().await.len(), 1);
    }

    #[tokio::test]
    async fn spawn_and_reap_ok() {
        let mgr = TaskManager::new();
        let id = mgr
            .spawn("t1", None, |_h| async {
                Ok(TaskOutcome {
                    analysis: "done".into(),
                    ..Default::default()
                })
            })
            .await;
        // wait for it to finish
        loop {
            let s = mgr.snapshot().await;
            if s.iter().find(|t| t.id == id).unwrap().state.is_terminal() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let reaped = mgr.reap().await;
        assert_eq!(reaped.len(), 1);
        assert_eq!(reaped[0].analysis, "done");
        assert_eq!(mgr.completed_run_count().await, 1);
    }

    #[tokio::test]
    async fn errored_task_does_not_increment_turn_counter() {
        // bugs.md#H4.
        let mgr = TaskManager::new();
        mgr.spawn("t-err", None, |_h| async {
            Err::<TaskOutcome, String>("boom".into())
        })
        .await;
        // Wait for terminal.
        loop {
            let s = mgr.snapshot().await;
            if s.iter().all(|t| t.state.is_terminal()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let reaped = mgr.reap().await;
        assert_eq!(reaped.len(), 1);
        assert_eq!(reaped[0].state, TaskState::Errored);
        assert_eq!(mgr.completed_run_count().await, 0);
    }

    #[tokio::test]
    async fn empty_analysis_does_not_increment_turn_counter() {
        let mgr = TaskManager::new();
        mgr.spawn("t-empty", None, |_h| async { Ok(TaskOutcome::default()) })
            .await;
        loop {
            let s = mgr.snapshot().await;
            if s.iter().all(|t| t.state.is_terminal()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let _ = mgr.reap().await;
        assert_eq!(mgr.completed_run_count().await, 0);
    }

    #[tokio::test]
    async fn stop_cancels_long_task() {
        let mgr = TaskManager::new();
        let id = mgr
            .spawn("forever", None, |h| async move {
                tokio::select! {
                    _ = h.shutdown.cancelled() => {
                        Err::<TaskOutcome, String>("cancelled".into())
                    }
                    _ = tokio::time::sleep(Duration::from_secs(30)) => {
                        Ok(TaskOutcome {
                            analysis: "never".into(),
                            ..Default::default()
                        })
                    }
                }
            })
            .await;
        // Give it a moment to reach Running.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let out = mgr.stop(id, Duration::from_secs(2)).await;
        assert!(matches!(out, StopOutcome::Stopped));
        let reaped = mgr.reap().await;
        assert_eq!(reaped.len(), 1);
        assert_eq!(reaped[0].state, TaskState::Errored);
    }

    #[tokio::test]
    async fn stop_all_cancels_every_task() {
        let mgr = TaskManager::new();
        for i in 0..5 {
            mgr.spawn(format!("t{i}"), None, |h| async move {
                tokio::select! {
                    _ = h.shutdown.cancelled() => {
                        Err::<TaskOutcome, String>("cancelled".into())
                    }
                    _ = tokio::time::sleep(Duration::from_secs(30)) => {
                        Ok(TaskOutcome {
                            analysis: "never".into(),
                            ..Default::default()
                        })
                    }
                }
            })
            .await;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        let out = mgr.stop_all(Duration::from_secs(2)).await;
        assert_eq!(out.requested, 5);
        assert_eq!(out.stopped, 5);
        assert_eq!(out.grace_expired, 0);
        let reaped = mgr.reap().await;
        assert_eq!(reaped.len(), 5);
        for r in reaped {
            assert_eq!(r.state, TaskState::Errored);
        }
    }

    #[tokio::test]
    async fn stop_unknown_task_reports_not_found() {
        let mgr = TaskManager::new();
        let out = mgr.stop(12345, Duration::from_millis(100)).await;
        assert!(matches!(out, StopOutcome::NotFound));
    }

    #[tokio::test]
    async fn stop_done_task_is_already_done() {
        let mgr = TaskManager::new();
        let id = mgr
            .spawn("fast", None, |_h| async {
                Ok(TaskOutcome {
                    analysis: "ok".into(),
                    ..Default::default()
                })
            })
            .await;
        // Wait for Done.
        loop {
            let s = mgr.snapshot().await;
            if s.iter().any(|t| t.id == id && t.state == TaskState::Done) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let out = mgr.stop(id, Duration::from_millis(100)).await;
        assert!(matches!(out, StopOutcome::AlreadyDone));
    }

    #[test]
    fn lru_evicts_oldest() {
        let mut c = LruCache::new(2);
        c.put("a".to_string(), 1);
        c.put("b".to_string(), 2);
        // touch "a" so "b" is oldest
        let _ = c.get(&"a".to_string());
        c.put("c".to_string(), 3);
        assert!(c.get(&"a".to_string()).is_some());
        assert!(c.get(&"c".to_string()).is_some());
        assert!(c.get(&"b".to_string()).is_none());
    }

    #[tokio::test]
    async fn caches_respect_cap() {
        // bugs.md#M1 — cache can't grow without bound.
        let mgr = TaskManager::with_caps(3, 3);
        for i in 0..10 {
            mgr.cache_symbol(format!("k{i}"), serde_json::json!({"n": i}))
                .await;
        }
        let keys = mgr.cached_symbol_names().await;
        assert!(keys.len() <= 3, "cap 3, got {}", keys.len());
    }

    #[tokio::test]
    async fn tasks_lock_snapshot_under_concurrent_spawns() {
        // bugs.md#C1 — snapshot should never see the list mid-mutation.
        let mgr = TaskManager::new();
        let mgr2 = mgr.clone();
        let producer = tokio::spawn(async move {
            for i in 0..50 {
                mgr2.spawn(format!("t{i}"), None, |_h| async {
                    Ok(TaskOutcome {
                        analysis: "ok".into(),
                        ..Default::default()
                    })
                })
                .await;
            }
        });
        // Simultaneously take snapshots; none should panic or see
        // inconsistent state.
        for _ in 0..100 {
            let _ = mgr.snapshot().await;
        }
        producer.await.unwrap();
    }
}
