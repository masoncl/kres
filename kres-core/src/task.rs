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

use std::collections::HashMap;
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
    /// §30: per-session parallelism cap. Every spawn acquires a
    /// permit before its closure runs; releasing happens when the
    /// closure exits. Default is unbounded so older tests aren't
    /// stalled by a cap they didn't set. `with_max_parallel(N)`
    /// shrinks it to match ["concurrency"]`
    /// (default 3,).
    parallel_semaphore: Arc<tokio::sync::Semaphore>,
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
        Arc::new(Self {
            inner: RwLock::new(Inner {
                tasks: Vec::new(),
                todo: Vec::new(),
                findings: Vec::new(),
                completed_run_count: 0,
                plan: None,
            }),
            caches: Mutex::new(Caches {
                symbol_cache: LruCache::new(symbol_cap),
                context_cache: LruCache::new(context_cap),
            }),
            root_shutdown: Shutdown::new(),
            findings_extract_lock: Mutex::new(()),
            // Default: effectively unbounded so existing tests that
            // spawn dozens of tasks in a tight loop keep running.
            // Call `with_max_parallel_from(cfg)` to lower it.
            parallel_semaphore: Arc::new(tokio::sync::Semaphore::new(
                tokio::sync::Semaphore::MAX_PERMITS,
            )),
        })
    }

    /// Build a manager with a specific parallelism cap. `n = 0` is
    /// treated as "unbounded" (matches `new()` / `with_caps()`).
    pub fn with_max_parallel(n: usize) -> Arc<Self> {
        let mgr = Self::with_caps(2000, 2000);
        if n > 0 {
            // Take advantage of Arc::get_mut — we're in a builder
            // window, no other clones exist yet.
            let permits = if n == 0 {
                tokio::sync::Semaphore::MAX_PERMITS
            } else {
                n
            };
            // Replace the semaphore by constructing a new manager
            // with the permit count dialed in.
            return Arc::new(Self {
                inner: RwLock::new(Inner {
                    tasks: Vec::new(),
                    todo: Vec::new(),
                    findings: Vec::new(),
                    completed_run_count: 0,
                    plan: None,
                }),
                caches: Mutex::new(Caches {
                    symbol_cache: LruCache::new(2000),
                    context_cache: LruCache::new(2000),
                }),
                root_shutdown: Shutdown::new(),
                findings_extract_lock: Mutex::new(()),
                parallel_semaphore: Arc::new(tokio::sync::Semaphore::new(permits)),
            });
        }
        mgr
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
        for (_, h) in handles {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                h.abort();
                expired += 1;
                continue;
            }
            match tokio::time::timeout(remaining, h).await {
                Ok(_) => stopped += 1,
                Err(_) => expired += 1,
            }
        }
        StopAllOutcome {
            requested: ids.len() as u32,
            stopped,
            grace_expired: expired,
        }
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

    pub async fn cached_symbol_names(&self) -> Vec<String> {
        let g = self.caches.lock().await;
        g.symbol_cache.keys()
    }

    // -- todo ----------------------------------------------------------

    pub async fn replace_todo(&self, items: Vec<TodoItem>) {
        let mut g = self.inner.write().await;
        g.todo = items;
    }

    pub async fn todo_snapshot(&self) -> Vec<TodoItem> {
        self.inner.read().await.todo.clone()
    }

    pub async fn mark_todo_status(&self, name: &str, status: TodoStatus) {
        let mut g = self.inner.write().await;
        if let Some(i) = g.todo.iter_mut().find(|i| i.name == name) {
            i.status = status;
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

    /// Remove and return all `Pending` and `Blocked` todos. Done and
    /// Skipped items stay on the manager's list so the next
    /// `sync_plan_from_todo` pass can still roll a plan step up to
    /// Done when its remaining linked todos are all terminal — the
    /// goal-met / --turns drains used to clear the todo list
    /// wholesale via `replace_todo(Vec::new())`, which erased the
    /// `step_id` linkage from completed work and pinned every plan
    /// step at Pending for the rest of the session.
    ///
    /// Callers that want InProgress items drained too should flip
    /// them first with `reset_in_progress_to_pending`.
    pub async fn drain_pending_blocked(&self) -> Vec<TodoItem> {
        let mut g = self.inner.write().await;
        let (drain, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut g.todo)
            .into_iter()
            .partition(|i| matches!(i.status, TodoStatus::Pending | TodoStatus::Blocked));
        g.todo = keep;
        drain
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
        let new_step_ids: std::collections::BTreeSet<String> = match plan.as_ref() {
            Some(p) => p.steps.iter().map(|s| s.id.clone()).collect(),
            None => std::collections::BTreeSet::new(),
        };
        let mut g = self.inner.write().await;
        g.plan = plan;
        for t in g.todo.iter_mut() {
            if !t.step_id.is_empty() && !new_step_ids.contains(&t.step_id) {
                t.step_id = String::new();
            }
        }
    }

    /// Recompute plan step statuses from the current todo list.
    /// No-op when no plan is set. Call after any todo mutation that
    /// could flip a linked item's status.
    pub async fn sync_plan_from_todo(&self) {
        let mut g = self.inner.write().await;
        let todo = g.todo.clone();
        if let Some(plan) = g.plan.as_mut() {
            plan.sync_from_todo(&todo);
        }
    }

    /// Overwrite `completed_run_count`. Only used by the session
    /// loader to restore a persisted count on resume — the normal
    /// path is the `finish_ok` auto-increment.
    pub async fn set_completed_run_count(&self, n: u32) {
        self.inner.write().await.completed_run_count = n;
    }

    // -- findings ------------------------------------------------------

    pub async fn findings_snapshot(&self) -> Vec<Finding> {
        self.inner.read().await.findings.clone()
    }

    pub async fn replace_findings(&self, findings: Vec<Finding>) {
        let mut g = self.inner.write().await;
        g.findings = findings;
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

    pub fn keys(&self) -> Vec<K> {
        self.map.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    async fn drain_pending_blocked_keeps_terminal_items() {
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
        let drained = mgr.drain_pending_blocked().await;
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
        let drained = mgr.drain_pending_blocked().await;
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].name, "b");
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
    async fn set_completed_run_count_overrides_counter() {
        let mgr = TaskManager::new();
        mgr.set_completed_run_count(42).await;
        assert_eq!(mgr.completed_run_count().await, 42);
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
