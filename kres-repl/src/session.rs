//! REPL session loop.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::mpsc;

use kres_agents::{AgentConfig, DataFetcher, Orchestrator, RunContext};
use kres_core::log::TurnLogger;
use kres_core::{FindingsStore, TaskManager, TaskState, UsageTracker};
use kres_llm::{client::Client, RateLimiter};

use crate::commands::{parse_command, Command};

#[derive(Debug, Clone)]
pub struct ReplConfig {
    pub stop_grace: Duration,
    /// Path to the canonical `findings.json` (jsondb-backed). When
    /// None, nothing is written to disk and findings stay in memory.
    pub findings_base: Option<PathBuf>,
    /// Stop the REPL after N completed task runs (0 = unlimited).
    /// Matches semantics.
    pub turns_limit: u32,
    /// When `turns_limit == 0`:
    ///   * `false` (default): trust the goal agent — keep running
    ///     until the goal-met handler drains the todo list, so the
    ///     session stops only when the goal agent says it is done.
    ///     When no goal agent is configured, fall back to stopping
    ///     as soon as the active batch finishes (pending followups
    ///     go to /followup).
    ///   * `true`: also accept 3 consecutive analysis-producing runs
    ///     with no new findings as a stop condition — a cost cap
    ///     for when the goal agent stays stubbornly "not met".
    ///
    /// No effect when `turns_limit > 0`: the run-count cap still
    /// wins there.
    pub follow_followups: bool,
    /// Per-task append-target for the report markdown (§26). When
    /// set, every reaped task's analysis lands as a new `## [type]
    /// name` section with a timestamp. When None, nothing is
    /// appended — operators can still call `/report PATH` manually.
    pub report_path: Option<PathBuf>,
    /// Explicit `--results DIR` from the CLI. Only Some when the
    /// operator passed --results; defaulted session directories do
    /// not count. Drives prompt.md persistence and /summary output
    /// placement — behaviour requested 2026-04-20.
    pub results_dir: Option<PathBuf>,
    /// Explicit `--template FILE` from the CLI. Passed through to
    /// SummaryInputs.template_path when /summary fires. When None
    /// the summariser falls back to ~/.kres/commands/summary.md (or
    /// summary-markdown.md with `/summary-markdown`), then to the
    /// compiled-in default (see kres_repl::summary and
    /// kres_agents::user_commands).
    pub template_path: Option<PathBuf>,
    /// When true, skip the persistent status line (no DECSTBM scroll
    /// region). Useful for dumb terminals / pipes / finicky muxers.
    pub stdio: bool,
    /// Opt into the ratatui TUI (stage 1 of the prompt-line
    /// migration). When set, [`Session::run`] owns the terminal via
    /// crossterm instead of rustyline. `stdio` takes precedence —
    /// `--stdio --tui` still uses the plain path so output
    /// redirection keeps working.
    pub tui: bool,
    /// Root for coding-mode file output. Coding tasks emit a
    /// `code_output` array whose paths are relative; the reaper
    /// writes them under this directory (`<workspace>/<path>` —
    /// not `<results>/code/<path>`, which buried files in the
    /// auto-generated session dir and surprised operators who
    /// expected "write hello-world.c" to land beside their cwd).
    /// Defaults to `.`; overridden by `--workspace` in main.rs.
    pub workspace: PathBuf,
    /// Path to `<results>/session.json`. When set, the reaper and
    /// drain paths persist a [`kres_core::SessionState`] snapshot
    /// here on every mutation so an interrupted session can be
    /// resumed by re-invoking kres with the same `--results DIR`.
    /// None disables persistence (no-op writes).
    pub persist_path: Option<PathBuf>,
    /// When true, exit the REPL once the work-stop condition fires
    /// (`--turns 0` goal-met / no-progress / no-goal-batch-finished),
    /// instead of staying open waiting for further operator input.
    /// Defaulted to `!stdout.is_terminal()` from main.rs so a piped
    /// invocation (`kres ... > out.txt`) terminates after the
    /// turns stop, matching the existing `--turns N` exit path.
    pub exit_on_idle: bool,
}

impl Default for ReplConfig {
    fn default() -> Self {
        Self {
            stop_grace: Duration::from_secs(5),
            findings_base: None,
            turns_limit: 0,
            follow_followups: false,
            report_path: None,
            results_dir: None,
            template_path: None,
            stdio: false,
            tui: false,
            workspace: PathBuf::from("."),
            persist_path: None,
            exit_on_idle: false,
        }
    }
}

/// Build a one-line summary of live work for the status bar.
///
/// Prefers the in-flight stream registry (agent label + live token
/// counters) when any stream is active, since those update every
/// few hundred ms with the actual bytes arriving. Falls back to the
/// coarser task list when no streams are open (e.g. between turns,
/// during main-agent tool dispatch).
///
/// Each stream segment looks like:
///   `fast round 2: in=4.2k cr=1.1k rd=3.0k out=812 (12s)`
/// Everything truncated to fit `max_cols`.
pub fn render_status_line(snap: &[kres_core::task::TaskSnapshot], max_cols: usize) -> String {
    use kres_core::TaskState;
    let streams = kres_core::io::active_streams();
    if !streams.is_empty() {
        let segments: Vec<String> = streams
            .iter()
            .map(|s| {
                format!(
                    "{}: in={} cr={} rd={} out={} ({}s)",
                    s.label,
                    fmt_tokens(s.input_tokens),
                    fmt_tokens(s.cache_creation_tokens),
                    fmt_tokens(s.cache_read_tokens),
                    fmt_tokens(s.output_tokens),
                    s.elapsed_ms / 1000,
                )
            })
            .collect();
        let body = segments.join(" │ ");
        let label = format!(" kres │ {} stream(s) │ {}", streams.len(), body);
        return label.chars().take(max_cols).collect();
    }
    let active: Vec<String> = snap
        .iter()
        .filter(|t| !matches!(t.state, TaskState::Done | TaskState::Errored))
        .map(|t| {
            let state = match t.state {
                TaskState::Pending => "pending",
                TaskState::Running => "running",
                TaskState::Cancelling => "cancelling",
                TaskState::Done => "done",
                TaskState::Errored => "errored",
            };
            let short_name: String = t.name.chars().take(40).collect();
            format!("#{} {} {}", t.id, state, short_name)
        })
        .collect();
    let body = if active.is_empty() {
        "idle".to_string()
    } else {
        active.join(" │ ")
    };
    let label = format!(" kres ({} task(s)) │ {}", active.len(), body);
    label.chars().take(max_cols).collect()
}

/// Compact token display: 1234 → "1.2k", 42 → "42", 1_234_567 → "1.2m".
fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}m", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// §21: garble-free async output. The sink lives in `kres_core::io`
/// so every downstream crate (kres-agents, kres-llm) can route
/// progress messages through the same channel without a dep on
/// kres-repl. The REPL installs a rustyline `ExternalPrinter`-backed
/// handler at startup (see read_stdin); everyone else calls
/// `kres_core::io::async_println`. Before the handler is installed,
/// or in non-REPL contexts (kres turn), the fallback goes to stderr.
pub use kres_core::io::async_println;

pub struct Session {
    mgr: Arc<TaskManager>,
    cfg: ReplConfig,
    orchestrator: Option<Arc<Orchestrator>>,
    consolidator: Option<Arc<kres_agents::ConsolidatorClient>>,
    todo_client: Option<Arc<kres_agents::TodoClient>>,
    goal_client: Option<Arc<kres_agents::GoalClient>>,
    findings_store: Option<Arc<FindingsStore>>,
    usage: Arc<UsageTracker>,
    lenses: Vec<kres_core::LensSpec>,
    initial_prompt: Option<String>,
    /// Last reaped task's analysis — consumed by /reply.
    last_analysis: Arc<tokio::sync::Mutex<Option<String>>>,
    /// Findings loaded from disk at Session::new time. Applied to
    /// the TaskManager synchronously at the top of `run()` so the
    /// first submit_prompt observes a non-empty previous_findings.
    pending_bootstrap: Vec<kres_core::findings::Finding>,
    /// Per-session turn logger. Created lazily by `with_logger` or
    /// implicitly in `with_orchestrator` when the caller hasn't set
    /// one.
    logger: Option<Arc<TurnLogger>>,
    /// Per-task completion goals, keyed by TaskId. define_goal's
    /// result is parked here when submit_prompt spawns a new task;
    /// the reaper looks it up (and removes it) when that task ends.
    /// Previously a single Mutex<Option<String>> — that was
    /// session-wide, so a second submit_prompt overwrote the first
    /// task's goal before the reaper could check it, causing the
    /// reaper to compare task-A's analysis against task-B's goal
    /// (or, if cleared by goal-met, against no goal at all).
    task_goals: Arc<tokio::sync::Mutex<std::collections::HashMap<kres_core::TaskId, String>>>,
    /// Per-task original prompt text, keyed by TaskId. Paired with
    /// task_goals so the reaper can feed both to check_goal. The
    /// derived goal sometimes compresses sweep intent ("check every
    /// file") into something narrow the judge trivially marks met;
    /// passing the raw prompt restores the ground truth.
    task_prompts: Arc<tokio::sync::Mutex<std::collections::HashMap<kres_core::TaskId, String>>>,
    /// Session-wide goal + mode set by the most recent operator-typed
    /// submission. Pipeline-driven follow-ups (cmd_next, cmd_continue,
    /// auto-continue) inherit this instead of running a fresh
    /// define_goal — the goal classifier, given a single-followup
    /// brief like "run `git add ...`", produces a narrow per-task
    /// goal ("Confirm git add succeeded") that check_goal trivially
    /// marks met after the action runs. Goal-met then drains the
    /// rest of the todo list to /followup and the run terminates
    /// short of commit/compile/review.
    ///
    /// Debugged via session 6a58e4fc (2026-04-27): a /fix run got
    /// through `git add` then stopped because the follow-up's
    /// derived goal asked only for the staging confirmation. With
    /// this slot, follow-ups inherit the original /fix-flow goal
    /// ("Produce a reviewed, committed git patch ...") so check_goal
    /// keeps the loop running until the patch is actually committed
    /// and reviewed.
    session_goal: Arc<tokio::sync::Mutex<Option<(String, kres_agents::TaskMode)>>>,
    /// Accumulated per-task findings — the flat
    /// `{task, analysis}` list that `/summary` and `/report`
    /// consume (§6).
    accumulated: Arc<tokio::sync::Mutex<Vec<AccumulatedEntry>>>,
    /// Items deferred by goal-met or --turns cap; `/followup` lists
    /// them (§6).
    deferred: Arc<tokio::sync::Mutex<Vec<kres_core::TodoItem>>>,
    /// §22: stashed interrupted prompt. When a ctrl-c lands during a
    /// long inference, the prompt text moves here so the next
    /// `/continue` can re-submit it verbatim.
    interrupted_prompt: Arc<tokio::sync::Mutex<Option<String>>>,
    /// Most recent prompt text (captured at the top of
    /// `submit_prompt`). Persisted into `<results>/session.json` so
    /// a resumed session's `--resume` reporting can show what the
    /// operator was working on.
    last_prompt: Arc<tokio::sync::Mutex<Option<String>>>,
    /// Hash of the last successfully-persisted session state bytes.
    /// Lets the reaper tick skip no-op fsyncs when nothing changed.
    /// Zero means "never persisted" and always triggers a write.
    persist_sig: Arc<std::sync::atomic::AtomicU64>,
    /// Set by `/stop`; cleared by `submit_prompt` and `/continue`.
    /// While set, the idle-loop auto-continue does not fire. Without
    /// this latch `/stop` only cancels the currently-running tasks,
    /// and the 5s auto-continue timer then re-dispatches whatever
    /// was still sitting in the todo list — which is NOT what an
    /// operator who just hit Ctrl-C's moral equivalent wants.
    stop_latched: Arc<std::sync::atomic::AtomicBool>,
    /// Woken by `cmd_stop` alongside the `stop_latched` atomic so
    /// an in-flight reaper-side inference call (the promoter today;
    /// the consolidator / todo-agent / merger in principle) can
    /// `tokio::select!` on `notified()` and abandon its API
    /// round-trip instead of running to completion while the
    /// operator waits for /stop to take effect. Notify is edge-
    /// triggered — notifications with no waiter are discarded,
    /// which matches the reaper's behaviour: the latched atomic
    /// catches the next iteration when no call is mid-flight.
    stop_notify: Arc<tokio::sync::Notify>,
    /// Pauses the 200ms status-row repainter while a child process
    /// (vim launched by /edit, for instance) has the terminal.
    /// Without this, the repainter absolute-positions to row H-1
    /// every tick and scribbles through the child's display, making
    /// the child's cursor drift visibly. Set in cmd_edit before
    /// spawn, cleared after return.
    status_paused: Arc<std::sync::atomic::AtomicBool>,
    /// The main loop sends on this after finishing each command;
    /// the rustyline reader waits for the send before calling
    /// readline() again (see read_stdin). That way `/edit` can
    /// block in cmd_edit without the reader painting `"> "` on
    /// top of vim in the meantime. Optional because Session::new
    /// constructs a Session without a running reader; the channel
    /// is installed in run() when the reader thread is spawned.
    input_ack_tx: tokio::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<()>>>,
    /// §50: handles to every spawned MCP child process. On REPL
    /// exit we walk these and call `shutdown(2s)` on each so
    /// tracebacks flush cleanly instead of the child getting
    /// killed mid-write. Matches (...)`
    /// at.
    mcp_shutdown: Arc<tokio::sync::Mutex<Vec<Arc<tokio::sync::Mutex<kres_mcp::McpClient>>>>>,
}

/// Build a [`kres_core::SessionState`] from live manager + deferred
/// state and persist it atomically to `path`. No-op on write errors
/// (logged at warn level) — a persist failure should never crash a
/// running pipeline. Shared between [`Session::persist_state`] and
/// the reaper loop (which only has clones of the needed Arcs).
///
/// `last_sig` throttles no-op writes: the reaper loop hands in an
/// `AtomicU64` that holds the hash of the most recently persisted
/// bytes. When the new bytes hash to the same value we skip the
/// fsync'd rename entirely, so an idle session does not pound the
/// disk at 4 writes/sec. Pass a fresh (zeroed) slot to force a
/// write — the hash of valid JSON is never 0.
async fn persist_session_state_to(
    path: &Path,
    mgr: &Arc<TaskManager>,
    deferred: &tokio::sync::Mutex<Vec<kres_core::TodoItem>>,
    last_prompt: Option<String>,
    last_sig: Option<&std::sync::atomic::AtomicU64>,
) {
    use std::hash::{Hash, Hasher};
    // Snapshot the plan BEFORE syncing so we can diff step
    // statuses afterwards and log every transition (pending →
    // done etc.). Cheap clone — the plan is usually a handful of
    // steps — and only runs inside the reaper tick.
    let plan_before = mgr.plan_snapshot().await;
    // Keep the plan in sync with the current todo statuses before
    // snapshotting, so the persisted plan reflects what has actually
    // completed rather than whatever the planner last wrote.
    mgr.sync_plan_from_todo().await;
    let plan_after = mgr.plan_snapshot().await;
    log_plan_status_transitions(plan_before.as_ref(), plan_after.as_ref());
    let state = kres_core::SessionState {
        version: 1,
        last_prompt,
        plan: plan_after,
        todo: mgr.todo_snapshot().await,
        deferred: deferred.lock().await.clone(),
        completed_run_count: mgr.completed_run_count().await,
    };
    // Serialise once; hash the bytes for the change-detect latch AND
    // (on change) hand the same bytes to save() so we don't pay the
    // cost twice. save() does its own serialisation for now; cheap
    // enough that the duplication is not worth a wider API change.
    let bytes = match serde_json::to_vec(&state) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                target: "kres_repl",
                "persist session state to {}: serialise: {e}",
                path.display()
            );
            return;
        }
    };
    if let Some(slot) = last_sig {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        bytes.hash(&mut h);
        let sig = h.finish();
        // Seq-cst on write + load: the reaper is the sole writer of
        // this slot, so Relaxed would suffice; Relaxed it is.
        let prior = slot.load(std::sync::atomic::Ordering::Relaxed);
        if sig == prior && prior != 0 {
            return;
        }
        slot.store(sig, std::sync::atomic::Ordering::Relaxed);
    }
    if let Err(e) = state.save(path) {
        tracing::warn!(
            target: "kres_repl",
            "persist session state to {}: {e}",
            path.display()
        );
    }
}

/// One row of the accumulated-findings ledger — matches 's
/// `_accumulated_findings.append({"task": ..., "analysis": ...})`
#[derive(Debug, Clone)]
pub struct AccumulatedEntry {
    /// Short human label (e.g. `[investigate] scrub drivers/net/...`).
    pub task: String,
    pub analysis: String,
}

impl Session {
    pub async fn new(mgr: Arc<TaskManager>, cfg: ReplConfig) -> Self {
        // Eagerly create the parent of the findings base so the
        // jsondb-backed store can open without the user having to
        // `mkdir -p` themselves. Matches what the pre-jsondb store
        // did implicitly.
        if let Some(ref p) = cfg.findings_base {
            if let Some(parent) = p.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    kres_core::async_eprintln!(
                        "findings: cannot create parent dir {}: {e}",
                        parent.display()
                    );
                }
            }
        }
        let mut findings_store: Option<Arc<FindingsStore>> = None;
        if let Some(ref p) = cfg.findings_base {
            match FindingsStore::new(p.clone()).await {
                Ok(fs) => findings_store = Some(Arc::new(fs)),
                Err(e) => {
                    kres_core::async_eprintln!(
                        "findings: store init failed for {}: {e}",
                        p.display()
                    );
                }
            }
        }
        if let Some(ref fs) = findings_store {
            let turn_n = fs.last_turn().await;
            let findings = fs.snapshot().await;
            let count = findings.len();
            // Seed the manager via `pending_bootstrap`, consumed at
            // the top of `run()`. This preserves the prior behaviour
            // where the first reap tick establishes the in-memory
            // list BEFORE submit_prompt observes a stale snapshot.
            kres_core::async_eprintln!(
                "findings: initialised at turn {} ({} existing)",
                turn_n,
                count
            );
            return Self {
                mgr,
                cfg,
                orchestrator: None,
                consolidator: None,
                todo_client: None,
                goal_client: None,
                findings_store,
                usage: Arc::new(UsageTracker::new()),
                lenses: Vec::new(),
                initial_prompt: None,
                last_analysis: Arc::new(tokio::sync::Mutex::new(None)),
                pending_bootstrap: findings,
                logger: None,
                task_goals: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
                task_prompts: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
                session_goal: Arc::new(tokio::sync::Mutex::new(None)),
                accumulated: Arc::new(tokio::sync::Mutex::new(Vec::new())),
                deferred: Arc::new(tokio::sync::Mutex::new(Vec::new())),
                interrupted_prompt: Arc::new(tokio::sync::Mutex::new(None)),
                last_prompt: Arc::new(tokio::sync::Mutex::new(None)),
                persist_sig: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                stop_latched: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                stop_notify: Arc::new(tokio::sync::Notify::new()),
                status_paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                input_ack_tx: tokio::sync::Mutex::new(None),
                mcp_shutdown: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            };
        }
        Self {
            mgr,
            cfg,
            orchestrator: None,
            consolidator: None,
            todo_client: None,
            goal_client: None,
            findings_store,
            usage: Arc::new(UsageTracker::new()),
            lenses: Vec::new(),
            initial_prompt: None,
            last_analysis: Arc::new(tokio::sync::Mutex::new(None)),
            pending_bootstrap: Vec::new(),
            logger: None,
            task_goals: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            task_prompts: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            session_goal: Arc::new(tokio::sync::Mutex::new(None)),
            accumulated: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            deferred: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            interrupted_prompt: Arc::new(tokio::sync::Mutex::new(None)),
            last_prompt: Arc::new(tokio::sync::Mutex::new(None)),
            persist_sig: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            stop_latched: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            stop_notify: Arc::new(tokio::sync::Notify::new()),
            status_paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            input_ack_tx: tokio::sync::Mutex::new(None),
            mcp_shutdown: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        }
    }

    /// Register MCP clients for graceful shutdown on REPL exit (§50).
    pub async fn register_mcp_clients(
        &self,
        clients: Vec<Arc<tokio::sync::Mutex<kres_mcp::McpClient>>>,
    ) {
        let mut g = self.mcp_shutdown.lock().await;
        g.extend(clients);
    }

    /// Attach a TurnLogger. Created once at REPL startup and cloned
    /// into every agent/merge/todo call site so the session's
    /// code.jsonl and main.jsonl capture every round-trip.
    pub fn with_logger(mut self, logger: Arc<TurnLogger>) -> Self {
        self.logger = Some(logger);
        self
    }

    /// Return the session's TurnLogger (if any) — exposed so the
    /// orchestrator builder can splice it into Orchestrator.logger.
    pub fn logger(&self) -> Option<Arc<TurnLogger>> {
        self.logger.clone()
    }

    pub fn with_consolidator(mut self, c: Arc<kres_agents::ConsolidatorClient>) -> Self {
        self.consolidator = Some(c);
        self
    }

    pub fn with_todo_client(mut self, c: Arc<kres_agents::TodoClient>) -> Self {
        self.todo_client = Some(c);
        self
    }

    /// Attach a GoalClient. Absent → goal system disabled; the
    /// session runs tasks until --turns / empty-todo-list ('s
    /// pre-goal behaviour).
    pub fn with_goal_client(mut self, c: Arc<kres_agents::GoalClient>) -> Self {
        self.goal_client = Some(c);
        self
    }

    /// Snapshot of the accumulated findings ledger. Used by `/report`,
    /// `/summary`, and the end-of-session write path.
    pub async fn accumulated_snapshot(&self) -> Vec<AccumulatedEntry> {
        self.accumulated.lock().await.clone()
    }

    /// Snapshot of the deferred todos (`/followup`).
    pub async fn deferred_snapshot(&self) -> Vec<kres_core::TodoItem> {
        self.deferred.lock().await.clone()
    }

    /// Persist session state (plan + todo + deferred + counters) to
    /// `cfg.persist_path`. No-op when the config didn't set one.
    /// Called from the reaper tick and the various drain paths so
    /// an interrupted session can be resumed via
    /// `kres --results DIR` on the next invocation.
    pub async fn persist_state(&self) {
        let Some(path) = self.cfg.persist_path.as_ref() else {
            return;
        };
        let last_prompt = self.last_prompt.lock().await.clone();
        persist_session_state_to(
            path,
            &self.mgr,
            &self.deferred,
            last_prompt,
            Some(&self.persist_sig),
        )
        .await;
    }

    /// Load a prior session from `cfg.persist_path` (or an
    /// explicit override) and seed the manager + deferred list.
    /// Called once at REPL startup when `--resume` was passed, and
    /// by the `/resume` command. Returns `Ok(Some(state))` on a
    /// successful resume, `Ok(None)` when there's nothing to
    /// resume (no persist path or file absent), and `Err` on parse
    /// / I/O failure.
    pub async fn resume_state(&self) -> Result<Option<kres_core::SessionState>> {
        self.resume_state_from(self.cfg.persist_path.as_deref())
            .await
    }

    /// `resume_state` with an explicit source path override. `None`
    /// falls back to `cfg.persist_path`.
    pub async fn resume_state_from(
        &self,
        override_path: Option<&Path>,
    ) -> Result<Option<kres_core::SessionState>> {
        let Some(path) = override_path.or(self.cfg.persist_path.as_deref()) else {
            return Ok(None);
        };
        let state = match kres_core::SessionState::load(path) {
            Ok(Some(s)) => s,
            Ok(None) => return Ok(None),
            Err(e) => return Err(anyhow::anyhow!("load {}: {e}", path.display())),
        };
        // Seed manager state. `SessionState::load` already flipped
        // InProgress → Pending, so re-seeded items come back ready
        // for /continue or auto-continue to pick them up.
        self.mgr.replace_todo(state.todo.clone()).await;
        self.mgr.set_plan(state.plan.clone()).await;
        self.mgr
            .set_completed_run_count(state.completed_run_count)
            .await;
        {
            let mut def = self.deferred.lock().await;
            *def = state.deferred.clone();
        }
        if let Some(p) = state.last_prompt.clone() {
            *self.last_prompt.lock().await = Some(p);
        }
        Ok(Some(state))
    }

    pub fn with_prompt_file(mut self, pf: kres_agents::PromptFile) -> Self {
        self.lenses = pf.lenses;
        if !pf.prompt.is_empty() {
            self.initial_prompt = Some(pf.prompt);
        }
        self
    }

    pub fn usage_tracker(&self) -> Arc<UsageTracker> {
        self.usage.clone()
    }

    pub fn with_orchestrator(mut self, o: Arc<Orchestrator>) -> Self {
        self.orchestrator = Some(o);
        self
    }

    /// Run the REPL loop.
    pub async fn run(&self) -> Result<()> {
        // Apply the bootstrap synchronously BEFORE anything can
        // submit a prompt, so the first task sees the full
        // previous_findings list. Was previously tokio::spawn-ed in
        // Session::new and could race submit_prompt.
        if !self.pending_bootstrap.is_empty() {
            self.mgr
                .replace_findings(self.pending_bootstrap.clone())
                .await;
        }

        // Move the sender INTO the input thread so when rustyline
        // hits EOF (ctrl-d) the only sender drops and the channel
        // fully closes — otherwise rx.recv() blocks forever waiting
        // on the retained outer-scope clone and ctrl-d appears to
        // hang the REPL.
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        // Ack channel: main loop sends after every command finishes,
        // the reader waits for the ack before calling readline again.
        // That keeps rustyline from painting "> " on top of a child
        // process (vim) that cmd_edit is running, and keeps it from
        // racing the main loop in general. In TUI mode the prompt is
        // a persistent widget (no readline repaint race), so the ack
        // is plumbed through but currently unused — kept so the
        // rustyline and TUI paths share the same signature.
        let (ack_tx, ack_rx) = mpsc::unbounded_channel::<()>();
        *self.input_ack_tx.lock().await = Some(ack_tx);
        // --stdio always wins, even if --tui was also passed — so a
        // redirected-to-file run stays line-buffered and doesn't
        // enter the alt screen / raw mode.
        let use_tui = self.cfg.tui && !self.cfg.stdio;
        if use_tui {
            let scrollback = crate::tui::Scrollback::new();
            crate::tui::install_tui_printer(scrollback.clone());
            // Shared task-snapshot cell. A tokio task refreshes it
            // every 200 ms; the TUI status closure reads it
            // synchronously with no block_on / no Handle dance (the
            // TUI runs under spawn_blocking, off the tokio scheduler,
            // so calling block_on from there would deadlock or
            // panic).
            let snap_cell: Arc<std::sync::Mutex<Vec<kres_core::task::TaskSnapshot>>> =
                Arc::new(std::sync::Mutex::new(Vec::new()));
            let mgr_for_refresh = self.mgr.clone();
            let snap_cell_for_refresh = snap_cell.clone();
            let shutdown_for_refresh = self.mgr.root_shutdown().clone();
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(Duration::from_millis(200));
                loop {
                    tokio::select! {
                        _ = shutdown_for_refresh.cancelled() => break,
                        _ = ticker.tick() => {
                            let snap = mgr_for_refresh.snapshot().await;
                            *snap_cell_for_refresh.lock().unwrap() = snap;
                        }
                    }
                }
            });
            let snap_cell_for_status = snap_cell.clone();
            let status_fn: crate::tui::StatusFn = Box::new(move |cols| {
                // Render inside the lock — TaskSnapshot isn't Clone,
                // and render_status_line only reads the slice so
                // there's no reentrancy risk.
                let guard = snap_cell_for_status.lock().unwrap();
                render_status_line(&guard, cols)
            });
            // History file — matches the rustyline path at
            // session.rs:3660. Sharing the same file means Up/Down
            // recall works across interactive / TUI / --stdio
            // invocations without per-mode silos.
            let history_path = dirs::home_dir().map(|h| h.join(".kres").join("history"));
            tokio::task::spawn_blocking(move || {
                if let Err(e) = crate::tui::run_tui(tx, ack_rx, scrollback, status_fn, history_path)
                {
                    eprintln!("tui: {e}");
                }
            });
        } else {
            // Non-TUI paths (rustyline and --stdio fallback) install
            // a stdout bootstrap printer BEFORE read_stdin runs so
            // every migrated `kres_core::async_eprintln!` call site
            // reaches a real sink from the first line. The rustyline
            // branch inside read_stdin later `replace_printer`s this
            // with its ExternalPrinter once the editor finishes
            // booting, which is what makes the prompt-aware printing
            // kick in. --stdio keeps the stdout printer for the
            // whole session so redirected output (`kres --stdio …
            // > out.txt`) captures everything.
            crate::tui::install_stdout_printer();
            tokio::task::spawn_blocking(move || read_stdin(tx, ack_rx));
        }

        // Reserve the bottom two rows for a status bar + prompt.
        // Scrolling output stays above; status shows what each task
        // is currently doing. install() returns geometry only when
        // stderr is a tty and terminal is tall enough (≥3 rows).
        // --stdio forces the plain path even when stdout is a tty.
        // --tui owns the terminal via crossterm, so the DECSTBM
        // scroll region is suppressed too; the TUI paints its own
        // status row.
        let status_geom = if self.cfg.stdio || use_tui {
            None
        } else {
            crate::status::install()
        };
        // Shared geometry cell so the paint task and the SIGWINCH
        // handler both see the same (rows, cols). On resize the
        // handler re-runs install() and overwrites this.
        let status_geom_shared: Arc<tokio::sync::RwLock<Option<(u16, u16)>>> =
            Arc::new(tokio::sync::RwLock::new(status_geom));
        // Pause flag for the paint task. /edit and /stop set it so a
        // child process that's taken over the terminal (vim, say)
        // doesn't get its display scribbled over every 200 ms by
        // the status-row repainter. Cleared when the child exits.
        self.status_paused
            .store(false, std::sync::atomic::Ordering::Release);
        let status_paused_for_paint = self.status_paused.clone();
        // Paint task: every 200ms repaint the status row. Every
        // ~1s (every 5 paint ticks) also poll term_size() — if the
        // terminal has resized since last check, clear the screen
        // and reinstall the scroll region at the new geometry.
        // SIGWINCH turned out unreliable under tmux pane drags
        // (ghost status lines at the old row), and TIOCGWINSZ is
        // just a syscall so polling is cheap.
        let status_handle = if status_geom.is_some() {
            let mgr_for_status = self.mgr.clone();
            let geom_for_paint = status_geom_shared.clone();
            Some(tokio::spawn(async move {
                let mut ticker = tokio::time::interval(Duration::from_millis(200));
                let mut ticks_since_size_check: u32 = 0;
                loop {
                    ticker.tick().await;
                    // Skip the whole tick when something (cmd_edit,
                    // etc.) has the terminal: the size-check branch
                    // would re-install the scroll region behind the
                    // child's back, and paint() would scribble
                    // across the child's frame.
                    if status_paused_for_paint.load(std::sync::atomic::Ordering::Acquire) {
                        continue;
                    }
                    ticks_since_size_check += 1;
                    if ticks_since_size_check >= 5 {
                        ticks_since_size_check = 0;
                        let cached = *geom_for_paint.read().await;
                        let current = crate::status::term_size();
                        if current != cached {
                            // Size changed. Preserve scrollback
                            // content — only wipe the old status
                            // row (at the CACHED location, which is
                            // exactly where we last painted it)
                            // before install() resets the scroll
                            // region and clears the new row. The
                            // next paint tick fills the new row
                            // with fresh content.
                            if let Some((old_rows, _)) = cached {
                                crate::status::clear_row_and_reset_region(
                                    old_rows.saturating_sub(1),
                                );
                            }
                            let new_geom = crate::status::install();
                            *geom_for_paint.write().await = new_geom;
                        }
                    }
                    let maybe_geom = *geom_for_paint.read().await;
                    if let Some((rows, cols)) = maybe_geom {
                        let snap = mgr_for_status.snapshot().await;
                        let line = render_status_line(&snap, cols as usize);
                        crate::status::paint(rows, cols, &line);
                    }
                }
            }))
        } else {
            None
        };
        // SIGWINCH path dropped in favor of term_size polling above.
        // Kept as Option<JoinHandle> = None so the teardown code
        // paths compile unchanged.
        let sigwinch_handle: Option<tokio::task::JoinHandle<()>> = None;

        let root = self.mgr.root_shutdown().clone();
        let mgr_for_ctrlc = self.mgr.clone();
        let persist_for_ctrlc = self.cfg.persist_path.clone();
        let deferred_for_ctrlc = self.deferred.clone();
        let last_prompt_for_ctrlc = self.last_prompt.clone();
        let persist_sig_for_ctrlc = self.persist_sig.clone();
        let ctrlc_handle = tokio::spawn(async move {
            // Each round: wait for ctrl-c, cooperatively cancel, arm a
            // 3s second-hit window for a hard exit, then loop. The
            // loop matters: without it the handler returns after the
            // first round and subsequent ctrl-cs go unhandled, so a
            // later stuck-inference sequence can no longer be
            // interrupted.
            loop {
                if tokio::signal::ctrl_c().await.is_err() {
                    return;
                }
                kres_core::async_eprintln!(
                    "\n(ctrl-c received; cancelling running tasks — hit again to abort)"
                );
                // §24: walk the task list and flip any in-progress
                // todo items BACK to Pending so they get re-queued for
                // the next /continue. Without this a tasks-were-
                // running ctrl-c would strand those todos in
                // "in_progress" forever.
                mgr_for_ctrlc.reset_in_progress_to_pending().await;
                // Snapshot to disk so a subsequent `kres --results
                // DIR` invocation can resume from where the operator
                // pressed ctrl-c.
                if let Some(ref p) = persist_for_ctrlc {
                    let lp = last_prompt_for_ctrlc.lock().await.clone();
                    persist_session_state_to(
                        p,
                        &mgr_for_ctrlc,
                        &deferred_for_ctrlc,
                        lp,
                        Some(&persist_sig_for_ctrlc),
                    )
                    .await;
                }
                root.cancel();
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {
                        kres_core::async_eprintln!("\n(second ctrl-c — aborting)");
                        std::process::exit(130);
                    }
                    _ = tokio::time::sleep(Duration::from_secs(3)) => {}
                }
            }
        });

        // Background reaper: every 250ms, drain done/errored tasks,
        // print a one-line summary, and merge their findings into
        // the manager's running list.
        let mgr_for_reaper = self.mgr.clone();
        let reaper_shutdown = self.mgr.root_shutdown().clone();
        let last_analysis = self.last_analysis.clone();
        let todo_client = self.todo_client.clone();
        let lenses_for_reaper = self.lenses.clone();
        let logger_for_reaper = self.logger.clone();
        let goal_client_for_reaper = self.goal_client.clone();
        let task_goals_for_reaper = self.task_goals.clone();
        let task_prompts_for_reaper = self.task_prompts.clone();
        let accumulated_for_reaper = self.accumulated.clone();
        let deferred_for_reaper = self.deferred.clone();
        let persist_path_for_reaper = self.cfg.persist_path.clone();
        let last_prompt_for_reaper = self.last_prompt.clone();
        let persist_sig_for_reaper = self.persist_sig.clone();
        let store_for_reaper = self.findings_store.clone();
        let promoter_for_reaper = self.consolidator.clone();
        let interrupted_for_reaper = self.interrupted_prompt.clone();
        let report_path_for_reaper = self.cfg.report_path.clone();
        // Destination for coding-mode file output. Coding tasks emit
        // path-relative files; they land under the workspace (i.e.
        // the operator's cwd at kres-start time, or --workspace) so
        // "write hello-world.c" does what it says on the tin
        // instead of burying the file in
        // ~/.kres/sessions/<ts>/code/hello-world.c.
        let code_output_root_for_reaper: PathBuf = self.cfg.workspace.clone();
        let stop_latched_for_reaper = self.stop_latched.clone();
        let stop_notify_for_reaper = self.stop_notify.clone();
        let turns_limit = self.cfg.turns_limit;
        let follow_followups = self.cfg.follow_followups;
        let exit_on_idle = self.cfg.exit_on_idle;
        // §16: findings-signature watchdog. Every successful merge
        // increments `quiescent` when the merged list's signature
        // matches the prior one; five consecutive no-change merges
        // prints the "ANALYSIS CONSIDERED COMPLETE" banner once.
        let mut last_sig: Vec<(String, String, String, String, usize, usize)> = Vec::new();
        let mut quiescent: u32 = 0;
        let mut quiescent_announced = false;
        // When --turns 0 (unlimited) we still want a natural stopping
        // point. Track consecutive completed slow-agent runs that
        // produced analysis but did not grow the findings list; 3 in
        // a row means the agents are spinning without producing
        // actionable output and we exit. Reset whenever the findings
        // count strictly increases.
        let mut no_new_findings_streak: u32 = 0;
        const NO_NEW_FINDINGS_STOP: u32 = 3;
        // Watchdog: if N consecutive reaped tasks come back Errored,
        // the pipeline is busted (revoked key, dead model, network
        // dropped, etc.) and re-queueing the same items via the todo
        // agent just burns API budget. Bail loudly. Reset on any
        // Done reap — a single success means things are working.
        // Without this, sessions like .kres/logs/6f3f0daf-… (269
        // failed slow calls in 50 min, 0 successes) silently spin
        // forever because the todo agent keeps re-queueing
        // "Prior execution returned empty analysis; must re-run."
        let mut consecutive_errors: u32 = 0;
        const MAX_CONSECUTIVE_ERRORS: u32 = 3;
        // Latch for the --turns 0 auto-stop banner. The stop check
        // below runs on every 250ms tick, but the operator only
        // wants to SEE "goal met" once; re-firing it every tick
        // would spam the terminal. The latch is reset below as soon
        // as new pending/blocked todos appear, so a fresh prompt
        // re-arms the stop announcement.
        let mut turns0_stop_announced = false;
        let reaper_handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(250));
            loop {
                tokio::select! {
                    _ = reaper_shutdown.cancelled() => break,
                    _ = ticker.tick() => {}
                }
                let reaped = mgr_for_reaper.reap().await;
                for r in reaped {
                    report_reaped(&r);
                    // §22: a task that reaches a terminal state
                    // (Done or Errored) is no longer interruptable
                    // — clear the stash so /continue doesn't
                    // re-submit a completed prompt.
                    if matches!(r.state, TaskState::Done | TaskState::Errored) {
                        *interrupted_for_reaper.lock().await = None;
                    }
                    // Coding-mode side effects: persist code_output
                    // files and apply code_edits BEFORE we build the
                    // analysis trailer — we want per-edit results
                    // folded into effective_analysis so failures are
                    // visible to the next slow-agent turn, the goal
                    // agent, and /summary (not just stderr).
                    if matches!(r.mode, kres_core::TaskMode::Coding) && !r.code_output.is_empty() {
                        persist_code_output(&code_output_root_for_reaper, &r.name, &r.code_output)
                            .await;
                    }
                    let applied_edits: Vec<AppliedEdit> = if matches!(
                        r.mode,
                        kres_core::TaskMode::Coding
                    ) && !r.code_edits.is_empty()
                    {
                        apply_code_edits(&code_output_root_for_reaper, &r.name, &r.code_edits).await
                    } else {
                        Vec::new()
                    };
                    // Execute git add/commit followups directly in the
                    // reaper instead of turning them into pipeline
                    // tasks. The main agent is a data-retrieval agent
                    // and only runs read-only git commands; when a
                    // "git add" todo dispatches as a pipeline task,
                    // the fast agent declares ready_for_slow without
                    // executing it, the slow agent hallucinates
                    // success, and the todo is marked done with
                    // nothing staged. Executing here mirrors how
                    // code_edits are applied directly above.
                    let mut git_results: Vec<String> = Vec::new();
                    if matches!(r.mode, kres_core::TaskMode::Coding) {
                        for fu in &r.followups {
                            let Some(kind) = reaper_followup_kind(fu) else {
                                continue;
                            };
                            let Some(name) = fu
                                .get("name")
                                .and_then(|v| v.as_str())
                                .filter(|s| !s.is_empty())
                            else {
                                continue;
                            };
                            let workspace = &code_output_root_for_reaper;
                            let result = match kind {
                                ReaperFollowup::Git => run_reaper_git(workspace, name).await,
                                ReaperFollowup::PublishFix => {
                                    run_publish_fix(workspace, name).await
                                }
                            };
                            kres_core::async_eprintln!(
                                "[reaper {}] {}: {}",
                                kind.label(),
                                truncate(name, 60),
                                truncate(result.trim(), 120),
                            );
                            git_results.push(result);
                        }
                    }

                    // For Coding-mode tasks the slow agent is told to
                    // keep prose short and put the artifact in
                    // `code_output`. But check_goal only reads the
                    // analysis string — so without help it sees a
                    // paragraph that "describes the approach" and
                    // keeps saying met=false while the file is
                    // sitting on disk (session 597b4bf7). Append a
                    // short trailer listing what landed so the goal
                    // agent has concrete evidence to judge on.
                    let effective_analysis = if r.code_output.is_empty()
                        && applied_edits.is_empty()
                        && git_results.is_empty()
                    {
                        r.analysis.clone()
                    } else {
                        let mut s = r.analysis.clone();
                        if !s.is_empty() && !s.ends_with('\n') {
                            s.push('\n');
                        }
                        if !r.code_output.is_empty() {
                            s.push_str("\n---\nFiles written to workspace:\n");
                            for f in &r.code_output {
                                let purpose = if f.purpose.is_empty() { "" } else { &f.purpose };
                                if purpose.is_empty() {
                                    s.push_str(&format!("- {}\n", f.path));
                                } else {
                                    s.push_str(&format!("- {} — {}\n", f.path, purpose));
                                }
                                // Include the head of the file so the
                                // goal agent can see the actual script
                                // body, not just the filename. Cap at
                                // 2000 chars so a very long artifact
                                // doesn't blow out the goal-check
                                // token budget.
                                let head: String = f.content.chars().take(2000).collect();
                                s.push_str("```\n");
                                s.push_str(&head);
                                if f.content.chars().count() > 2000 {
                                    s.push_str("\n… (truncated, full content at ");
                                    s.push_str(&f.path);
                                    s.push_str(")\n");
                                }
                                if !head.ends_with('\n') {
                                    s.push('\n');
                                }
                                s.push_str("```\n");
                            }
                        }
                        s.push_str(&format_applied_edits_trailer(&applied_edits));
                        if !git_results.is_empty() {
                            s.push_str("\n---\nGit operations:\n");
                            for gr in &git_results {
                                s.push_str(&format!("- {}\n", gr));
                            }
                        }
                        s
                    };
                    if !effective_analysis.is_empty() {
                        let mut la = last_analysis.lock().await;
                        *la = Some(effective_analysis.clone());
                    }

                    // §6: append every reaped task's
                    // (task_label, analysis) to the accumulated
                    // ledger so /summary + /report have the per-
                    // task narrative to work from.
                    if !effective_analysis.is_empty() {
                        let entry = AccumulatedEntry {
                            task: r.name.clone(),
                            analysis: effective_analysis.clone(),
                        };
                        accumulated_for_reaper.lock().await.push(entry);
                        // §26: append the analysis to the report
                        // markdown (one section per task). The
                        // accumulated ledger drives `/summary` and
                        // `/report PATH`; this append mirrors 's
                        // `_append_report` for an always-up-to-date
                        // on-disk narrative.
                        if let Some(ref rp) = report_path_for_reaper {
                            if let Err(e) =
                                crate::report::append_task_section(rp, &r.name, &effective_analysis)
                            {
                                tracing::warn!(
                                    target: "kres_repl",
                                    "report append to {}: {e}",
                                    rp.display()
                                );
                            }
                        }
                    }
                    // Coding tasks skip the merger / consolidator /
                    // findings pipeline entirely — the goal agent
                    // runs against effective_analysis (now including
                    // the edit trailer) and the reaped files already
                    // landed above.
                    let pre_size = mgr_for_reaper.findings_snapshot().await.len();
                    // /stop is latched: skip every inference-heavy
                    // reaper post-step (findings merger, goal check,
                    // todo-agent update). The cancelled task is
                    // already reaped; report.md + accumulated
                    // already captured whatever prose survived.
                    // Continuing through merger/goal/todo-update
                    // would rack up API calls AND inject new todos
                    // into the queue the operator just drained with
                    // /stop, reproducing the "still going" feeling.
                    let stop_latched_now =
                        stop_latched_for_reaper.load(std::sync::atomic::Ordering::Acquire);
                    if stop_latched_now {
                        continue;
                    }
                    // Findings delta application runs for Analysis
                    // (review) and Generic tasks — both feed the
                    // findings pipeline. Coding tasks skip it: their
                    // output is source files, not findings.
                    //
                    // The LLM-based merger has been retired. The slow
                    // agent's prompt already tells it to reuse an
                    // existing finding's id when extending it; the
                    // store applies deterministic merge rules in Rust
                    // (kres_core::findings::apply_delta_to_list) — no
                    // token round-trip per turn.
                    //
                    // Promotion pass: the slow agent + consolidator
                    // PROMOTION RULE is instructional only. When they
                    // describe a bug in prose but don't emit the
                    // matching Finding (or when the response
                    // RawText-downgrades and the findings array is
                    // empty), the bug reaches report.md and is lost
                    // to findings.json. A one-shot fast-agent audit
                    // pass here reads effective_analysis against the
                    // current findings_delta and returns any net-new
                    // bugs it spots; we append those to the delta
                    // before apply_delta runs. Non-fatal: on any
                    // failure we skip and carry on with whatever the
                    // slow agent did emit.
                    let mut working_delta = r.findings_delta.clone();
                    // Ids the promoter contributed on top of
                    // r.findings_delta. Populated when the audit
                    // pass returns extras; consumed below to append
                    // a cross-reference trailer to report.md so a
                    // human reader of the narrative can find the
                    // new Findings by id.
                    let mut promoted_ids: Vec<String> = Vec::new();
                    if r.mode.produces_findings() && !effective_analysis.is_empty() {
                        if let Some(ref promoter) = promoter_for_reaper {
                            // Assemble the full universe of known
                            // ids (store snapshot ∪ this task's
                            // delta). `apply_delta_to_list` matches
                            // by id against the store, so we need
                            // the whole universe for the Rust-side
                            // dedup filter to catch collisions.
                            let mut all_known = mgr_for_reaper.findings_snapshot().await;
                            for d in &working_delta {
                                if !all_known.iter().any(|e| e.id == d.id) {
                                    all_known.push(d.clone());
                                }
                            }
                            // Narrow the LLM-bound subset to findings
                            // actually mentioned (by id, filename, or
                            // symbol name) in the prose. A 500-entry
                            // store with full source bodies blows up
                            // the prompt; a typical prose chunk only
                            // touches a handful of those. False
                            // negatives in this scan are handled
                            // downstream: filter_net_new sees the
                            // full `all_known` and RENAMES colliding
                            // ids rather than dropping them.
                            let prose_relevant =
                                kres_core::relevant_subset(&effective_analysis, &all_known);
                            // Both slices go to the promoter's
                            // prompt path — redact Finding.details
                            // so the per-task narrative captured for
                            // /summary never round-trips through
                            // another LLM call. dedup_against only
                            // touches ids, but redact uniformly for
                            // discipline.
                            let prose_relevant =
                                kres_core::redact_findings_for_agent(&prose_relevant);
                            let all_known_for_dedup =
                                kres_core::redact_findings_for_agent(&all_known);
                            kres_core::async_eprintln!(
                                "[promote] sending {} of {} existing finding(s) to auditor",
                                prose_relevant.len(),
                                all_known.len(),
                            );
                            match kres_agents::promote::promote_prose_bugs_with_logger(
                                promoter.client.clone(),
                                promoter.model.clone(),
                                // Use the dedicated promoter system
                                // prompt, NOT the consolidator's
                                // inherited fast-code-agent system.
                                // Same drift-avoidance reason the
                                // retired merger_system.txt existed.
                                Some(kres_agents::promote::PROMOTE_SYSTEM),
                                promoter.max_tokens,
                                promoter.max_input_tokens,
                                &r.name,
                                &effective_analysis,
                                &prose_relevant,
                                &all_known_for_dedup,
                                Some(stop_notify_for_reaper.clone()),
                                logger_for_reaper.clone(),
                            )
                            .await
                            {
                                Ok(extras) if !extras.is_empty() => {
                                    kres_core::async_eprintln!(
                                        "[promote] {} prose-only bug(s) promoted to findings",
                                        extras.len()
                                    );
                                    promoted_ids.extend(extras.iter().map(|f| f.id.clone()));
                                    working_delta.extend(extras);
                                }
                                Ok(_) => {}
                                Err(e) => {
                                    tracing::warn!(
                                        target: "kres_repl",
                                        "promote pass failed: {e}"
                                    );
                                }
                            }
                        }
                    }
                    // Provenance stamp written into findings'
                    // first_seen_task / last_updated_task. Shape:
                    //   "<uuid-simple>/<todo-tag>"   when a todo
                    //   dispatched this task (cmd_next /
                    //   cmd_continue paths), else just the uuid.
                    //   We avoid the prior `r.name` convention —
                    //   for an operator-typed `/review …` task
                    //   `r.name` is the full prompt body, which
                    //   got duplicated across every finding.
                    let stamp = match r.todo_name.as_deref() {
                        Some(tag) => format!("{}/{}", r.uuid.as_simple(), tag),
                        None => r.uuid.as_simple().to_string(),
                    };
                    // Persist the task's effective_analysis at the
                    // file level for `/summary`'s benefit, regardless
                    // of whether a finding delta landed. Captures the
                    // broader-than-finding narrative (overview,
                    // summary tables, cross-cutting conclusions,
                    // multi-step proofs) that no single
                    // `Finding.details[].analysis` claims ownership
                    // of — observed missing from session
                    // `kres-findings2`, where 21 `###` headings in
                    // report.md had no counterpart in any finding's
                    // JSON body. NEVER forwarded to an agent:
                    // `task_prose` sits on `FindingsFile`, and agents
                    // only see `&[Finding]` via
                    // `redact_findings_for_agent`.
                    if !effective_analysis.is_empty() {
                        if let Some(ref s) = store_for_reaper {
                            if let Err(e) = s.append_task_prose(&stamp, &effective_analysis).await {
                                kres_core::async_eprintln!("task_prose append: {e}");
                            }
                        }
                    }
                    let had_delta = r.mode.produces_findings() && !working_delta.is_empty();
                    let mut apply_changed = false;
                    let mut apply_added: u32 = 0;
                    let mut apply_updated: u32 = 0;
                    let mut apply_invalidated: u32 = 0;
                    let mut apply_reactivated: u32 = 0;
                    if had_delta {
                        let delta = working_delta.clone();
                        // effective_analysis is the prose we want on
                        // every finding this task touched, stored
                        // under `details` for /summary to consume
                        // later. Feed it to apply_delta alongside
                        // the stamp so the record_detail pass can
                        // attach one entry per finding per task.
                        let prose_for_details = effective_analysis.clone();
                        if let Some(ref s) = store_for_reaper {
                            let s_c = s.clone();
                            let stamp_c = stamp.clone();
                            let prose_c = prose_for_details.clone();
                            let report = mgr_for_reaper
                                .with_findings_extract_lock(|| async move {
                                    s_c.apply_delta(&delta, Some(&stamp_c), Some(&prose_c))
                                        .await
                                })
                                .await;
                            match report {
                                Ok(rep) => {
                                    apply_changed = rep.changed;
                                    apply_added = rep.added;
                                    apply_updated = rep.updated;
                                    apply_invalidated = rep.invalidated;
                                    apply_reactivated = rep.reactivated;
                                    mgr_for_reaper.replace_findings(rep.merged).await;
                                }
                                Err(e) => {
                                    kres_core::async_eprintln!("findings apply: {e}");
                                }
                            }
                        } else {
                            // No persistent store (no --results set):
                            // apply the same rules to the in-memory
                            // list so the pipeline still benefits
                            // from deterministic dedup.
                            let mut current = mgr_for_reaper.findings_snapshot().await;
                            let counts = kres_core::apply_delta_to_list(
                                &mut current,
                                &delta,
                                Some(&stamp),
                                Some(&prose_for_details),
                            );
                            apply_changed = counts.changed;
                            apply_added = counts.added;
                            apply_updated = counts.updated;
                            apply_invalidated = counts.invalidated;
                            apply_reactivated = counts.reactivated;
                            mgr_for_reaper.replace_findings(current).await;
                        }
                    }
                    // Promoted-findings cross-reference trailer on
                    // report.md. `effective_analysis` was appended
                    // earlier (before the /stop latch + promoter +
                    // apply_delta) — that ordering is load-bearing
                    // for the /stop-latched case, which otherwise
                    // would lose its prose. Appending a SECOND small
                    // section here now that we know which ids the
                    // promoter added lets a human reader of the
                    // narrative jump to the new Findings without
                    // re-reading the whole JSON store. Only append
                    // when apply_delta actually landed those ids —
                    // an apply_delta error above leaves promoted_ids
                    // unrecorded in findings.json, so a stray
                    // cross-reference would be misleading.
                    if !promoted_ids.is_empty() && apply_changed {
                        if let Some(ref rp) = report_path_for_reaper {
                            let joined = promoted_ids
                                .iter()
                                .map(|id| format!("`{id}`"))
                                .collect::<Vec<_>>()
                                .join(", ");
                            let trailer = format!(
                                "_promoted-from-prose: {}_ ({})",
                                joined,
                                promoted_ids.len()
                            );
                            if let Err(e) =
                                crate::report::append_task_section(rp, &r.name, &trailer)
                            {
                                tracing::warn!(
                                    target: "kres_repl",
                                    "report promoted-ids trailer append to {}: {e}",
                                    rp.display()
                                );
                            }
                        }
                    }
                    let final_list = mgr_for_reaper.findings_snapshot().await;
                    let new_sig = findings_signature(&final_list);
                    // Also treat apply_changed as a change signal even
                    // when the signature happens to match (e.g. the
                    // signature hash doesn't fold in relevant_symbols
                    // updates). `last_sig != new_sig` catches list
                    // membership shifts; apply_changed catches field
                    // updates on an existing id.
                    let changed = apply_changed || new_sig != last_sig;
                    last_sig = new_sig;
                    if changed {
                        quiescent = 0;
                        quiescent_announced = false;
                    } else {
                        quiescent += 1;
                        if quiescent >= 5 && !quiescent_announced {
                            kres_core::async_eprintln!("=== ANALYSIS CONSIDERED COMPLETE ===",);
                            quiescent_announced = true;
                        }
                    }
                    // §turns0: only count tasks that actually produced
                    // analysis. A strict growth in the merged findings
                    // list resets the streak; anything else — whether
                    // the delta was empty, or apply_delta folded it
                    // into existing findings — counts as "no new
                    // findings".
                    if !r.analysis.is_empty() {
                        let grew = final_list.len() > pre_size;
                        if grew {
                            no_new_findings_streak = 0;
                        } else {
                            no_new_findings_streak = no_new_findings_streak.saturating_add(1);
                        }
                    }
                    if had_delta {
                        kres_core::async_eprintln!(
                            "[findings] {} total (added={} updated={} invalidated={} reactivated={} changed={} quiescent={})",
                            final_list.len(),
                            apply_added,
                            apply_updated,
                            apply_invalidated,
                            apply_reactivated,
                            changed,
                            quiescent,
                        );
                    }
                    // Update todo list via todo-agent when one is
                    // configured. Non-fatal on any failure — the todo
                    // list is maintained best-effort.
                    if let Some(ref tc) = todo_client {
                        let current = mgr_for_reaper.todo_snapshot().await;
                        let completed_query = r.name.clone();
                        // Errored tasks reach this path with
                        // analysis="". Without surfacing the error
                        // here the todo agent reads "no analysis" as
                        // "task didn't run, re-queue" and we spin
                        // (see consecutive_errors comment above).
                        // Inject the error so the agent has something
                        // concrete to react to (skip vs. retry).
                        let analysis = if matches!(r.state, TaskState::Errored) {
                            format!(
                                "[task errored: {}]",
                                r.error.as_deref().unwrap_or("(no error text)")
                            )
                        } else {
                            r.analysis.clone()
                        };
                        // Filter out followups the reaper already
                        // executed directly (above). Only the rest
                        // go to the todo agent for promotion to
                        // pending tasks.
                        let followups: Vec<_> = if matches!(r.mode, kres_core::TaskMode::Coding) {
                            r.followups
                                .iter()
                                .filter(|f| reaper_followup_kind(f).is_none())
                                .cloned()
                                .collect()
                        } else {
                            r.followups.clone()
                        };
                        kres_core::async_eprintln!(
                            "[todo update] before: {} item(s) ({} pending, {} done); {} new followup(s)",
                            current.len(),
                            current
                                .iter()
                                .filter(|t| t.status == kres_core::TodoStatus::Pending)
                                .count(),
                            current
                                .iter()
                                .filter(|t| t.status == kres_core::TodoStatus::Done)
                                .count(),
                            followups.len(),
                        );
                        let plan_for_todo = mgr_for_reaper.plan_snapshot().await;
                        match kres_agents::update_todo_via_agent_with_logger(
                            tc,
                            &completed_query,
                            &analysis,
                            &followups,
                            &current,
                            &lenses_for_reaper,
                            plan_for_todo.as_ref(),
                            logger_for_reaper.clone(),
                        )
                        .await
                        {
                            Ok(updated) => {
                                kres_core::async_eprintln!(
                                    "[todo update] after:  {} item(s) ({} pending, {} done)",
                                    updated.todo.len(),
                                    updated
                                        .todo
                                        .iter()
                                        .filter(|t| t.status == kres_core::TodoStatus::Pending)
                                        .count(),
                                    updated
                                        .todo
                                        .iter()
                                        .filter(|t| t.status == kres_core::TodoStatus::Done)
                                        .count(),
                                );
                                // When the todo agent rewrote the
                                // plan, swap it in BEFORE replacing
                                // the todo list so the next
                                // sync_plan_from_todo tick sees the
                                // new plan matching the new step_ids
                                // the same turn emitted.
                                if let Some(rewrite) = updated.plan {
                                    let prior = mgr_for_reaper.plan_snapshot().await;
                                    let new_plan = rewrite.apply_to(prior.as_ref());
                                    log_plan_change(
                                        "todo agent: plan rewrite",
                                        prior.as_ref(),
                                        &new_plan,
                                    );
                                    mgr_for_reaper.set_plan(Some(new_plan)).await;
                                }
                                mgr_for_reaper.replace_todo(updated.todo).await;
                            }
                            Err(e) => {
                                tracing::warn!(
                                    target: "kres_repl",
                                    "todo-agent update failed: {e}"
                                );
                            }
                        }
                    }

                    // §4 goal check: if a goal is set, ask the
                    // main agent whether the current analyses
                    // satisfy it. When met, every pending todo
                    // moves to the deferred list and running tasks
                    // get cancelled so the operator reclaims the
                    // prompt.
                    //
                    // For coding-mode tasks (fix flow), skip the
                    // per-task goal check entirely. A "turn" in
                    // the fix flow is the full fix+compile+review
                    // cycle, not each individual sub-task (git add,
                    // git commit, make, etc.). Checking after each
                    // sub-step wastes tokens and risks a premature
                    // met=true. The todo list drives iteration;
                    // check_goal runs only when the list drains
                    // (no more pending items) or when an
                    // audit/generic task completes.
                    if matches!(r.mode, kres_core::TaskMode::Coding) {
                        kres_core::async_eprintln!(
                            "[goal check] skipped for coding-mode task — \
                             goal evaluated when todo list drains"
                        );
                        // Still clean up the per-task goal/prompt
                        // entries so they don't leak.
                        task_goals_for_reaper.lock().await.remove(&r.id);
                        task_prompts_for_reaper.lock().await.remove(&r.id);
                    } else {
                        // Goal is now per-task: each reaped task carries
                        // an id, and submit_prompt parked its goal under
                        // that id in task_goals. Pull it out (removing so
                        // it doesn't live forever) and evaluate against
                        // the accumulated analysis.
                        let per_task_goal = task_goals_for_reaper.lock().await.remove(&r.id);
                        let per_task_prompt = task_prompts_for_reaper
                            .lock()
                            .await
                            .remove(&r.id)
                            .unwrap_or_default();
                        if let (Some(gc), Some(goal)) =
                            (goal_client_for_reaper.clone(), per_task_goal)
                        {
                            let entries = accumulated_for_reaper.lock().await.clone();
                            kres_core::async_eprintln!(
                            "[goal check] checking against {} accumulated analysis/es ({}k chars)",
                            entries.len(),
                            entries.iter().map(|e| e.analysis.len()).sum::<usize>() / 1000,
                        );
                            let mut combined = String::new();
                            for (i, e) in entries.iter().enumerate() {
                                if i > 0 {
                                    combined.push_str("\n\n---\n\n");
                                }
                                combined.push_str(&format!("## {}\n\n{}", e.task, e.analysis));
                            }
                            let plan_for_check = mgr_for_reaper.plan_snapshot().await;
                            let check = kres_agents::check_goal(
                                &gc,
                                &per_task_prompt,
                                &goal,
                                &combined,
                                plan_for_check.as_ref(),
                            )
                            .await;
                            kres_core::async_eprintln!(
                                "[goal check] met={} reason={}",
                                check.met,
                                truncate(&check.reason, 120)
                            );
                            if check.met {
                                kres_core::async_eprintln!(
                                    "[goal met: {}]",
                                    truncate(&check.reason, 200)
                                );
                                // Any lingering InProgress items belong
                                // to tasks the reaper already handled;
                                // flip them to Pending so they join the
                                // deferred drain below instead of being
                                // silently dropped.
                                mgr_for_reaper.reset_in_progress_to_pending().await;
                                // Drain pending todos into the deferred
                                // ledger so /followup can list them.
                                // Done/Skipped items stay on the todo
                                // list so their step_id linkage survives
                                // — the next sync_plan_from_todo tick
                                // can then flip any fully-covered plan
                                // step to Done.
                                let drained = mgr_for_reaper.drain_pending_blocked().await;
                                let carry = drained.len();
                                let mut deferred = deferred_for_reaper.lock().await;
                                deferred.extend(drained);
                                drop(deferred);
                                if carry > 0 {
                                    kres_core::async_eprintln!(
                                    "[{carry} pending item(s) moved to deferred — run /followup to list, /continue to pursue]"
                                );
                                }
                                // Per-task goal already removed at the
                                // top of this branch by .remove(&r.id);
                                // nothing else to clear.
                            } else if !check.missing.is_empty() {
                                kres_core::async_eprintln!(
                                    "[goal not yet met — missing: {}]",
                                    check.missing.join(", ")
                                );
                                // Spec in CLAUDE.md: "Goal not met → only
                                // missing items become followups." Previous
                                // code only printed the list; the items
                                // were discarded. Match by
                                // converting each missing item to a
                                // 'question'-typed followup and funneling
                                // them through the todo agent so they get
                                // deduped against existing items and
                                // appended as new todos.
                                if let Some(ref tc) = todo_client {
                                    let reason_prefix = format!(
                                        "goal not met: {}",
                                        check.reason.chars().take(100).collect::<String>()
                                    );
                                    let missing_fus: Vec<serde_json::Value> = check
                                        .missing
                                        .iter()
                                        .map(|m| {
                                            serde_json::json!({
                                                "type": "question",
                                                "name": m,
                                                "reason": reason_prefix,
                                            })
                                        })
                                        .collect();
                                    let current = mgr_for_reaper.todo_snapshot().await;
                                    let completed_query = r.name.clone();
                                    kres_core::async_eprintln!(
                                    "[goal-not-met → todo update] injecting {} missing item(s) as question followups",
                                    missing_fus.len()
                                );
                                    let plan_for_todo = mgr_for_reaper.plan_snapshot().await;
                                    match kres_agents::update_todo_via_agent_with_logger(
                                        tc,
                                        &completed_query,
                                        "",
                                        &missing_fus,
                                        &current,
                                        &lenses_for_reaper,
                                        plan_for_todo.as_ref(),
                                        logger_for_reaper.clone(),
                                    )
                                    .await
                                    {
                                        Ok(updated) => {
                                            kres_core::async_eprintln!(
                                            "[goal-not-met → todo update] after: {} item(s) ({} pending, {} done)",
                                            updated.todo.len(),
                                            updated.todo.iter().filter(|t| t.status == kres_core::TodoStatus::Pending).count(),
                                            updated.todo.iter().filter(|t| t.status == kres_core::TodoStatus::Done).count(),
                                        );
                                            if let Some(rewrite) = updated.plan {
                                                let prior = mgr_for_reaper.plan_snapshot().await;
                                                let new_plan = rewrite.apply_to(prior.as_ref());
                                                log_plan_change(
                                                    "todo agent: plan rewrite (goal-not-met)",
                                                    prior.as_ref(),
                                                    &new_plan,
                                                );
                                                mgr_for_reaper.set_plan(Some(new_plan)).await;
                                            }
                                            mgr_for_reaper.replace_todo(updated.todo).await;
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                target: "kres_repl",
                                                "todo-agent update (missing items) failed: {e}"
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    } // else (non-coding goal check)
                      // Consecutive-error watchdog: surface a busted
                      // pipeline instead of letting the todo agent
                      // re-queue the same items forever (see counter
                      // declaration above for context).
                    match r.state {
                        TaskState::Errored => {
                            consecutive_errors = consecutive_errors.saturating_add(1);
                        }
                        TaskState::Done => {
                            consecutive_errors = 0;
                        }
                        _ => {}
                    }
                    if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                        kres_core::async_eprintln!(
                            "\n=== {consecutive_errors} CONSECUTIVE TASK FAILURES — halting ===\nlast error: {}\ncheck the kres terminal for [rate-limit]/[stream-interrupt] lines, verify the slow/fast API key + model id, then /continue or restart kres --resume.",
                            r.error.as_deref().unwrap_or("(no error text)")
                        );
                        // Reset so a /continue retry that fails again
                        // takes another full MAX_CONSECUTIVE_ERRORS run
                        // before re-firing, instead of re-printing the
                        // banner on every subsequent Errored reap.
                        consecutive_errors = 0;
                        mgr_for_reaper.reset_in_progress_to_pending().await;
                        let drained = mgr_for_reaper.drain_pending_blocked().await;
                        let carry = drained.len();
                        let mut deferred = deferred_for_reaper.lock().await;
                        deferred.extend(drained);
                        drop(deferred);
                        if carry > 0 {
                            kres_core::async_eprintln!(
                                "[{carry} pending item(s) moved to /followup]"
                            );
                        }
                        if exit_on_idle {
                            mgr_for_reaper.root_shutdown().cancel();
                            break;
                        }
                    }
                }
                // --turns N limit: once the slow-agent run count hits
                // the configured cap, broadcast cancel so the REPL
                // exits. Matches 's "stop after N completed task
                // runs" behaviour.
                if turns_limit > 0 {
                    let done = mgr_for_reaper.completed_run_count().await;
                    if done >= turns_limit {
                        kres_core::async_eprintln!(
                            "\n=== --turns {turns_limit} reached — {done} task run(s) completed ==="
                        );
                        mgr_for_reaper.reset_in_progress_to_pending().await;
                        let drained = mgr_for_reaper.drain_pending_blocked().await;
                        let carry = drained.len();
                        let mut deferred = deferred_for_reaper.lock().await;
                        deferred.extend(drained);
                        drop(deferred);
                        if carry > 0 {
                            kres_core::async_eprintln!(
                                "[{carry} pending item(s) deferred — see /followup]"
                            );
                        }
                        kres_core::async_eprintln!("exiting REPL.");
                        mgr_for_reaper.root_shutdown().cancel();
                        break;
                    }
                    // Goal met before --turns N reached: the goal-met
                    // branch above (line ~1556) drained pending todos
                    // to deferred but intentionally did not cancel
                    // root_shutdown (interactive REPL stays open). In
                    // exit_on_idle mode the process must exit — check
                    // the same followups_drained condition the --turns 0
                    // path uses.
                    if exit_on_idle && done > 0 {
                        let active = mgr_for_reaper.active_count().await;
                        let todo = mgr_for_reaper.todo_snapshot().await;
                        let pending_or_blocked = todo
                            .iter()
                            .filter(|t| {
                                matches!(
                                    t.status,
                                    kres_core::TodoStatus::Pending | kres_core::TodoStatus::Blocked
                                )
                            })
                            .count();
                        if active == 0 && pending_or_blocked == 0 {
                            kres_core::async_eprintln!(
                                "\n=== goal met, todo list drained ({done} run(s)) — exiting ==="
                            );
                            mgr_for_reaper.root_shutdown().cancel();
                            break;
                        }
                    }
                } else {
                    // --turns 0 (unlimited) — stopping rule:
                    //
                    //   Default: trust the goal agent.  The goal-met
                    //   branch upstream drains the todo list, so
                    //   `followups_drained` becoming true is our
                    //   signal that the goal agent declared
                    //   completion.  We keep running as long as the
                    //   goal agent keeps saying "not met" and
                    //   follow-up tasks keep appearing.
                    //
                    //   --follow: also accept 3 consecutive
                    //   analysis-producing runs with no new findings
                    //   as a stop, so an operator can cap the cost
                    //   even when the goal agent is stubborn.
                    //
                    //   No goal agent configured: fall back to
                    //   "active batch finished" so the REPL doesn't
                    //   loop forever in the no-goal-agent case.
                    //
                    // Gate the whole stop check on "at least one task
                    // has actually produced work in this session".
                    // This block lives at the reaper's tick level, not
                    // inside the `for r in reaped` loop — so without
                    // the gate it would tick once at startup with
                    // active_count=0 and pending_or_blocked=0 and
                    // report "goal met (todo list drained)" before
                    // the operator had a chance to submit a prompt
                    // (user report 2026-04-21).
                    // completed_run_count is bumped in finish_ok only
                    // when a task produced non-empty analysis OR
                    // non-empty code_output, so it's the right signal
                    // for "real work happened".
                    let done_so_far = mgr_for_reaper.completed_run_count().await;
                    if done_so_far == 0 {
                        continue;
                    }
                    let active = mgr_for_reaper.active_count().await;
                    let todo = mgr_for_reaper.todo_snapshot().await;
                    let pending_or_blocked = todo
                        .iter()
                        .filter(|t| {
                            matches!(
                                t.status,
                                kres_core::TodoStatus::Pending | kres_core::TodoStatus::Blocked
                            )
                        })
                        .count();
                    let followups_drained = active == 0 && pending_or_blocked == 0;
                    let no_progress = no_new_findings_streak >= NO_NEW_FINDINGS_STOP;
                    let goal_configured = goal_client_for_reaper.is_some();
                    let no_goal_batch_stop = !goal_configured && !follow_followups && active == 0;
                    let should_stop = if follow_followups {
                        followups_drained || no_progress
                    } else if goal_configured {
                        followups_drained
                    } else {
                        no_goal_batch_stop
                    };
                    // Reset the latch as soon as new work shows up so
                    // the operator sees a fresh "goal met" banner
                    // after submitting another prompt.
                    if pending_or_blocked > 0 {
                        turns0_stop_announced = false;
                    }
                    if should_stop && !turns0_stop_announced {
                        let reason = if no_goal_batch_stop && !followups_drained {
                            format!(
                                "no goal agent; active batch finished ({pending_or_blocked} followup(s) deferred; pass --follow to chase them)"
                            )
                        } else if followups_drained {
                            if goal_configured {
                                "goal met (todo list drained)".to_string()
                            } else {
                                "followup list empty".to_string()
                            }
                        } else {
                            format!(
                                "no new findings for {no_new_findings_streak} consecutive run(s)"
                            )
                        };
                        let suffix = if exit_on_idle {
                            "exiting (stdout is not a terminal)"
                        } else {
                            "REPL staying open; submit another prompt, /summary, or /quit"
                        };
                        kres_core::async_eprintln!("\n=== --turns 0: {reason} — {suffix} ===");
                        // Flip InProgress → Pending before the drain
                        // so the deferred list is complete; an item
                        // mid-run at goal-met time shouldn't silently
                        // disappear.
                        mgr_for_reaper.reset_in_progress_to_pending().await;
                        // Move any leftover pending/blocked items to
                        // /followup's deferred list. Done/Skipped
                        // items stay so the plan step rollup can
                        // still see them. Unlike the --turns N path
                        // we do NOT cancel the root shutdown — the
                        // user wants to keep driving the REPL after
                        // goal met.
                        let drained = mgr_for_reaper.drain_pending_blocked().await;
                        let carry = drained.len();
                        let mut deferred = deferred_for_reaper.lock().await;
                        deferred.extend(drained);
                        drop(deferred);
                        if carry > 0 {
                            kres_core::async_eprintln!(
                                "[{carry} pending item(s) moved to /followup]"
                            );
                        }
                        turns0_stop_announced = true;
                        // Non-tty stdout (or --one): exit on first
                        // stop, same as `--turns N`. Cancel
                        // root_shutdown to break the REPL select on
                        // root_shutdown.cancelled().
                        if exit_on_idle {
                            mgr_for_reaper.root_shutdown().cancel();
                            break;
                        }
                    }
                }
                // Persist session state at the end of every reaper
                // tick. This captures all mutation paths (reaped
                // tasks, followup drains, goal-met / --turns drains)
                // in a single spot rather than sprinkling save calls
                // across every callsite. The content-hash latch in
                // persist_session_state_to makes idle ticks a no-op
                // so the 250ms cadence does not pound the disk.
                if let Some(ref p) = persist_path_for_reaper {
                    let lp = last_prompt_for_reaper.lock().await.clone();
                    persist_session_state_to(
                        p,
                        &mgr_for_reaper,
                        &deferred_for_reaper,
                        lp,
                        Some(&persist_sig_for_reaper),
                    )
                    .await;
                }
            }
        });

        // Install a session-scoped consent store so reads outside
        // --workspace can be auto-granted by mention in the
        // operator's prompt (see consent::grant_paths_from_text in
        // submit_prompt).  install() returns Err when the slot was
        // already set; that's fine — subsequent Sessions in the
        // same process (rare; tests) will see the first one's
        // store, which is acceptable for the unit-test surface.
        let _ = kres_core::consent::install(Arc::new(kres_core::ConsentStore::new()));
        print_banner();
        if !self.lenses.is_empty() {
            kres_core::async_eprintln!(
                "installed {} session-wide slow-agent lens(es):",
                self.lenses.len()
            );
            for l in &self.lenses {
                kres_core::async_eprintln!("  [{}] {}", l.kind, l.name);
            }
        }
        if let Some(ref p) = self.initial_prompt {
            kres_core::async_eprintln!("submitting initial prompt from --prompt");
            self.submit_prompt(p.clone()).await;
        }
        let root_shutdown = self.mgr.root_shutdown().clone();
        let mut auto_continue_idle_since: Option<std::time::Instant> = None;
        loop {
            // rustyline prints its own "> " prompt when attached to
            // a tty; the plain fallback path for piped input doesn't
            // want a prompt at all. So print_prompt() is gone.
            //
            // Also break on root_shutdown cancel so --turns (fired
            // from the reaper) exits the REPL cleanly.
            //
            // §46 auto-continue idle loop: when there are no active
            // tasks but pending todos, print a 5-second countdown
            // banner and auto-launch /continue on timeout so long
            // unattended runs make forward progress without the
            // operator re-typing. Any input during the window
            // cancels the auto-continue.
            // Auto-continue: fire cmd_continue after 5s of
            // continuous idle-with-pending-deps. Previously a single
            // `should_auto_continue()` sample before tokio::select!
            // meant a reaper that added pending items DURING the
            // select wait couldn't trigger the timer — the sleep
            // branch was gated by a stale false. Instead, poll the
            // predicate each second inside the select and maintain
            // an idle-start timestamp; dispatch when it's been true
            // for >= AUTO_CONTINUE_IDLE.
            const AUTO_CONTINUE_IDLE: Duration = Duration::from_secs(5);
            let line = tokio::select! {
                _ = root_shutdown.cancelled() => break,
                l = rx.recv() => {
                    // Input arrived: reset idle clock.
                    auto_continue_idle_since = None;
                    match l {
                        Some(s) => s,
                        None => break,
                    }
                }
                _ = tokio::time::sleep(Duration::from_secs(1)) => {
                    if self.should_auto_continue().await {
                        let since = auto_continue_idle_since.get_or_insert_with(std::time::Instant::now);
                        if since.elapsed() >= AUTO_CONTINUE_IDLE {
                            kres_core::async_eprintln!("[auto-continue: dispatching next batch (hit enter to cancel)]");
                            self.cmd_continue().await;
                            auto_continue_idle_since = None;
                        }
                    } else {
                        auto_continue_idle_since = None;
                    }
                    continue;
                }
            };
            match parse_command(&line) {
                Command::Noop => {}
                Command::Help => print_help(),
                Command::Tasks => self.print_tasks().await,
                Command::Stop => self.cmd_stop().await,
                Command::Clear => self.cmd_clear().await,
                Command::Compact => self.cmd_compact().await,
                Command::Findings => self.print_findings().await,
                Command::Cost => self.print_cost(),
                Command::Todo { clear } => {
                    if clear {
                        self.cmd_todo_clear().await;
                    } else {
                        self.print_todo().await;
                    }
                }
                Command::Plan => self.cmd_plan().await,
                Command::Resume { path } => self.cmd_resume(path).await,
                Command::Followup => self.cmd_followup().await,
                Command::Summary { filename } => self.cmd_summary(filename, false).await,
                Command::SummaryMarkdown { filename } => self.cmd_summary(filename, true).await,
                Command::Review { target } => self.cmd_review(target).await,
                Command::Fix { target } => self.cmd_fix(target).await,
                Command::Extract {
                    dir,
                    report,
                    todo,
                    findings,
                } => self.cmd_extract(dir, report, todo, findings).await,
                Command::Done { index } => self.cmd_done(index).await,
                Command::Report { path } => self.cmd_report(path).await,
                Command::Load { path } => self.cmd_load(path).await,
                Command::Edit => self.cmd_edit().await,
                Command::Reply { text } => self.cmd_reply(text).await,
                Command::Next => self.cmd_next().await,
                Command::Continue => self.cmd_continue().await,
                Command::Quit => {
                    kres_core::async_eprintln!("bye");
                    // Fast-path teardown. Cancel root so every reaper /
                    // orchestrator / task future awaiting cancellation
                    // wakes up now (instead of waiting for stop_all to
                    // individually poke each per-task token). Use a
                    // short grace — a stuck task shouldn't hold up the
                    // operator's exit. MCP children die via
                    // kill_on_drop when the final Arc drops, so the
                    // 2s-per-server graceful path below is skipped.
                    self.mgr.root_shutdown().cancel();
                    let out = self
                        .mgr
                        .stop_all(std::time::Duration::from_millis(500))
                        .await;
                    if out.requested > 0 {
                        kres_core::async_eprintln!(
                            "teardown: {}/{} stopped, {} grace-expired",
                            out.stopped,
                            out.requested,
                            out.grace_expired
                        );
                    }
                    ctrlc_handle.abort();
                    reaper_handle.abort();
                    if let Some(h) = status_handle.as_ref() {
                        h.abort();
                    }
                    if let Some(h) = sigwinch_handle.as_ref() {
                        h.abort();
                    }
                    crate::status::restore();
                    return Ok(());
                }
                Command::Unknown(name) => {
                    kres_core::async_eprintln!("unknown command: /{name} (try /help)");
                }
                Command::Prompt(text) => {
                    // submit_prompt awaits define_goal inline before
                    // spawning the task (goal is used by check_goal
                    // later). If define_goal is stuck in retries (e.g.
                    // workspace-wide 429 crunch, up to 20 retries * N
                    // seconds each) the REPL loop is blind to new
                    // input and to ctrl-c until that future returns.
                    // Race it against root_shutdown so ctrl-c actually
                    // interrupts.
                    tokio::select! {
                        _ = self.submit_prompt(text) => {}
                        _ = root_shutdown.cancelled() => {
                            kres_core::async_eprintln!("(prompt submission cancelled)");
                        }
                    }
                }
            }
            // Command done. Tell the stdin reader it may call
            // readline again and paint the next "> " prompt. Skipped
            // on Quit (that branch `return`s above, dropping the
            // reader's channel).
            if let Some(tx) = self.input_ack_tx.lock().await.as_ref() {
                let _ = tx.send(());
            }
        }

        // --turns exit path drops straight into teardown — no
        // auto-summary. Operators who want the artifact run /summary
        // before quitting, or `kres --summary` against the results
        // dir afterwards.
        let out = self.mgr.stop_all(self.cfg.stop_grace).await;
        if out.requested > 0 {
            kres_core::async_eprintln!(
                "teardown: {}/{} stopped, {} grace-expired",
                out.stopped,
                out.requested,
                out.grace_expired
            );
        }
        ctrlc_handle.abort();
        reaper_handle.abort();
        if let Some(h) = status_handle.as_ref() {
            h.abort();
        }
        if let Some(h) = sigwinch_handle.as_ref() {
            h.abort();
        }
        crate::status::restore();

        // §50: walk every registered MCP client and ask for a
        // graceful shutdown with a 2s grace window. Without this
        // the children get SIGKILL'd via kill_on_drop(true) when
        // their `Arc` drops, which loses the last few lines of
        // stderr logs.
        let mut registered = self.mcp_shutdown.lock().await;
        for client in registered.drain(..) {
            if let Ok(c) = Arc::try_unwrap(client) {
                let c = c.into_inner();
                if let Err(e) = c.shutdown(std::time::Duration::from_secs(2)).await {
                    kres_core::async_eprintln!("mcp shutdown: {e}");
                }
            }
            // If try_unwrap fails the fetcher still holds a clone;
            // dropping this Arc is enough — kill_on_drop cleans up.
        }
        Ok(())
    }

    /// Call the goal agent for `text` and announce the resolved
    /// goal+mode under `label` ("fresh", "inherited", "fallback").
    /// Returns (None, default_mode) when the goal client is
    /// unavailable or the agent declines to produce one.
    async fn derive_goal(
        &self,
        text: &str,
        plan: Option<&kres_core::Plan>,
        label: &str,
    ) -> (Option<String>, kres_agents::TaskMode) {
        let Some(gc) = &self.goal_client else {
            return (None, kres_agents::TaskMode::default());
        };
        match kres_agents::define_goal(gc, text, plan).await {
            Some(def) => {
                kres_core::async_eprintln!(
                    "goal ({}, {label}): {}",
                    def.mode.as_str(),
                    truncate(&def.goal, 160)
                );
                (Some(def.goal), def.mode)
            }
            None => (None, kres_agents::TaskMode::default()),
        }
    }

    /// Operator-typed submission (REPL line, `--prompt`, /load,
    /// /edit, /reply, /continue's stashed-interrupted resume).
    /// Prepends the accumulated-analysis ledger as "Recent context"
    /// so a follow-up operator prompt doesn't start cold.
    async fn submit_prompt(&self, text: String) {
        self.submit_prompt_inner(text, true, None).await
    }

    /// Pipeline-driven submission (cmd_next / cmd_continue's
    /// batch-dispatch loop — auto-continue also funnels through
    /// here).
    ///
    /// For audit/generic tasks the todo item already carries a
    /// structured brief and the slow agent sees previous_findings
    /// plus original_prompt via RunContext, so re-injecting the
    /// ledger would double-count — it would widen narrow fetch
    /// tasks, bust the fast-agent's cached prefix, and pay 8k
    /// chars per turn on every follow-up.
    ///
    /// For coding-mode tasks (fix flow) the accumulated analysis
    /// IS the critical state: it carries what was fixed, what the
    /// build said, what the review found. Without it each follow-up
    /// task starts cold and re-does work the prior task already
    /// finished. Session 841f1305 (2026-04-27): the compile-
    /// verification task couldn't compose a commit message because
    /// the finding text and diff were in the prior task's context
    /// only, not the preamble. Subsequent tasks looped back to
    /// compiling instead of progressing to commit.
    ///
    /// `todo_tag` is the dispatching TodoItem's id (or name when id
    /// is empty) — fed into findings provenance via apply_delta so a
    /// stored finding records which todo produced it.
    async fn submit_from_pipeline(&self, text: String, todo_tag: Option<String>) {
        self.submit_prompt_inner(text, false, todo_tag).await
    }

    async fn submit_prompt_inner(
        &self,
        text: String,
        include_recent_context: bool,
        todo_tag: Option<String>,
    ) {
        let Some(orc) = self.orchestrator.clone() else {
            kres_core::async_eprintln!("(no orchestrator configured — prompt dropped)");
            kres_core::async_eprintln!(
                "hint: rerun `kres repl` with agent configs to enable prompt handling"
            );
            return;
        };
        // Operator engaged — clear the /stop latch so auto-continue
        // works again after this task completes.
        self.stop_latched
            .store(false, std::sync::atomic::Ordering::Release);
        // Auto-grant read consent for any file or directory the
        // operator just named in their prompt. Only fires for
        // operator-typed submissions; pipeline-driven submits
        // (cmd_next / cmd_continue) skip this — the model can't
        // talk kres into reading new trees by hallucinating paths
        // in its followups.
        if include_recent_context {
            if let Some(store) = kres_core::consent::get() {
                let added =
                    kres_core::consent::grant_paths_from_text(&store, &self.cfg.workspace, &text);
                if !added.is_empty() {
                    let label: Vec<String> =
                        added.iter().map(|g| g.dir.display().to_string()).collect();
                    kres_core::async_eprintln!(
                        "consent: granted read access to {} dir(s) named in the prompt: {}",
                        added.len(),
                        truncate(&label.join(", "), 200)
                    );
                    // Louder warning when the operator's prompt
                    // grants a top-level system tree (/usr, /etc,
                    // $HOME, …) — usually accidental, e.g. pasting
                    // a stack trace with a libc path.
                    let wide: Vec<String> = added
                        .iter()
                        .filter(|g| g.suspicious)
                        .map(|g| g.dir.display().to_string())
                        .collect();
                    if !wide.is_empty() {
                        kres_core::async_eprintln!(
                            "consent: WARNING wide grant(s) for top-level system dir(s): {} — narrow the path in the prompt or restart kres if accidental",
                            wide.join(", ")
                        );
                    }
                }
            }
        }

        // §44: inline expand any `/load <path>` substring the user
        // wove into the prompt. Matches. Multiple
        // references expand; a missing file leaves the `/load …`
        // text in place and emits an error to the REPL.
        let text = expand_inline_load(&text);

        // Save the first submitted prompt to <results>/prompt.md so
        // later runs (re-invocations of `kres --summary` against the
        // same directory, or this session's own /summary) have the
        // original question in hand. Only when the operator passed
        // --results; defaulted ~/.kres/sessions/<ts>/ directories
        // skip this. Never overwrite an existing prompt.md — /next
        // and /continue both call submit_prompt for follow-up todo
        // items, and those are not the original prompt.
        if let Some(ref dir) = self.cfg.results_dir {
            let p = dir.join("prompt.md");
            if !p.exists() {
                if let Err(e) = std::fs::create_dir_all(dir) {
                    kres_core::async_eprintln!("prompt.md: cannot create {}: {e}", dir.display());
                } else if let Err(e) = std::fs::write(&p, &text) {
                    kres_core::async_eprintln!("prompt.md: write {}: {e}", p.display());
                } else {
                    kres_core::async_eprintln!("prompt.md: saved to {}", p.display());
                }
            }
        }

        // §22: stash the prompt so a ctrl-c during inference leaves
        // enough state for /continue to re-submit. Cleared after
        // spawn — the spawned task owns re-execution from here.
        *self.interrupted_prompt.lock().await = Some(text.clone());
        // Track the latest prompt for session.json persistence.
        *self.last_prompt.lock().await = Some(text.clone());

        // Ask the main agent for a concrete completion goal
        // ( / §4). Failures fall through to a null
        // goal; we still run the task, we just skip goal checks.
        // The goal is parked below against the spawned task's id so
        // the reaper can pull the right goal for the right task —
        // with multiple concurrent prompts the previous single
        // session-wide goal overwrote earlier ones and the reaper
        // checked task-A's analysis against task-B's goal.
        //
        // Pass the manager's current plan so the per-task goal can
        // anchor itself to a named step. Pipeline follow-ups run
        // through this same path, so without the plan they'd produce
        // goals with no step attribution and the todo_update path
        // downstream can't flip the parent step to `done`.
        //
        // include_recent_context is the operator-vs-pipeline
        // discriminator. Operator-typed submissions (true) get a
        // fresh define_goal call AND cache the result as the
        // session-wide goal. Pipeline-driven follow-ups (false)
        // INHERIT that cached goal: re-running define_goal on a
        // single-followup brief like "run `git add ...`" produces
        // narrow per-task goals ("Confirm git add succeeded") that
        // check_goal-met trivially, draining the rest of the todo
        // list to /followup short of commit/compile/review. See
        // session_goal field comment + session 6a58e4fc replay.
        // Defensive fallback: if no session goal is cached (e.g. a
        // /resume followed by /continue, or a pipeline submission
        // before any operator submission), fall back to the live
        // define_goal call so we get something rather than nothing.
        let existing_plan = self.mgr.plan_snapshot().await;
        let (defined_goal, task_mode): (Option<String>, kres_agents::TaskMode) =
            if include_recent_context {
                // Operator-typed submission: derive a fresh goal and
                // cache it for downstream pipeline follow-ups.
                let r = self
                    .derive_goal(&text, existing_plan.as_ref(), "fresh")
                    .await;
                if let Some(g) = r.0.as_ref() {
                    *self.session_goal.lock().await = Some((g.clone(), r.1));
                }
                r
            } else {
                // Pipeline follow-up: inherit the cached session goal
                // so a narrow brief like "git add foo" doesn't get
                // its own one-shot goal that trivially evaluates met
                // and drains the rest of the todo list.
                let cached = self.session_goal.lock().await.clone();
                if let Some((g, m)) = cached {
                    kres_core::async_eprintln!(
                        "goal ({}, inherited): {}",
                        m.as_str(),
                        truncate(&g, 160)
                    );
                    (Some(g), m)
                } else {
                    // No session goal cached (e.g. /resume followed
                    // by /continue, or a pipeline submission before
                    // any operator submission). Fall back to a
                    // fresh derivation and cache it.
                    let r = self
                        .derive_goal(&text, existing_plan.as_ref(), "fallback")
                        .await;
                    if let Some(g) = r.0.as_ref() {
                        *self.session_goal.lock().await = Some((g.clone(), r.1));
                    }
                    r
                }
            };
        // Ask the goal agent for a plan decomposition, but only on
        // operator-typed submissions — pipeline-driven follow-ups
        // already live under the original plan and should not spawn
        // fresh ones. Gated on a goal having been produced: without
        // a goal the planner has nothing to work from. Pass the
        // manager's current plan so the planner can decide whether
        // this prompt is a continuation (preserve ids) or a fresh
        // topic (emit a new plan); set_plan reconciles orphan
        // step_ids on todos when ids change.
        if include_recent_context {
            if let (Some(gc), Some(goal)) = (&self.goal_client, defined_goal.as_ref()) {
                let existing = self.mgr.plan_snapshot().await;
                if let Some(plan) =
                    kres_agents::define_plan(gc, &text, goal, task_mode, existing.as_ref()).await
                {
                    log_plan_change("define_plan", existing.as_ref(), &plan);
                    self.mgr.set_plan(Some(plan)).await;
                }
            }
        }
        let orc_task = orc.clone();
        // Snapshot findings BEFORE spawning so the task's
        // RunContext sees the running list. bugs.md#H1: the read is
        // cheap and doesn't hold any lock across the API call.
        let previous_findings = self.mgr.findings_snapshot().await;
        let task_brief = format!("prompt: {}", truncate(&text, 60));
        let task_brief_clone = task_brief.clone();
        let lenses = self.lenses.clone();
        let consolidator = self.consolidator.clone();
        let original_prompt = text.clone();
        let prompt_for_park = text.clone();
        // Build the prompt that actually reaches the fast agent:
        // for operator-typed submissions (`include_recent_context =
        // true`) we always prepend the accumulated-analysis ledger.
        //
        // For pipeline-driven submits in CODING mode we also include
        // the preamble: the fix flow's accumulated analysis carries
        // what was fixed, what the build said, what the review found.
        // Without it each follow-up task starts cold and re-does work
        // the prior task already finished (session 841f1305: compile-
        // verification task couldn't compose a commit message because
        // finding text and diff were in the prior task's context
        // only). For audit/generic pipeline follow-ups we skip it —
        // the todo brief is self-contained and the preamble would
        // bust the fast-agent's cached prefix for no benefit.
        //
        // /clear wipes the ledger; /compact shrinks it to a single
        // summary entry.
        let include_preamble =
            include_recent_context || matches!(task_mode, kres_agents::TaskMode::Coding);
        let text = if include_preamble {
            let context_preamble = build_recent_context_preamble(
                &self.accumulated.lock().await,
                RECENT_CONTEXT_CAP_CHARS,
            );
            if context_preamble.is_empty() {
                text
            } else {
                format!("{context_preamble}\n\n---\n\n{text}")
            }
        } else {
            text
        };
        // Snapshot the plan BEFORE spawning so the task's RunContext
        // sees the plan that was current when the task was
        // submitted. A later operator prompt may replace the plan
        // (set_plan(Some(new))) while this task is still mid-run;
        // the cloned snapshot keeps each task pinned to its own
        // plan for the duration.
        let plan_for_ctx = self.mgr.plan_snapshot().await;
        // Only the initial task spawned from an operator-typed
        // prompt gets to rewrite the plan via the slow agent. A
        // pipeline follow-up (/next, /continue, auto-continue) has
        // include_recent_context=false and keeps this flag off so
        // later-turn slow calls can't reshape the plan mid-sweep;
        // incremental plan edits flow through the todo agent for
        // those.
        let allow_plan_rewrite = include_recent_context;
        let task_id = self
            .mgr
            .spawn(task_brief, todo_tag, move |handle| async move {
                let ctx = RunContext {
                    previous_findings,
                    task_brief: task_brief_clone,
                    original_prompt,
                    mode: task_mode,
                    plan: plan_for_ctx,
                    allow_plan_rewrite,
                };
                // Dispatch by mode:
                //   Coding  → single slow call with slow_coding_system;
                //             reaper persists code_output, skips merge.
                //   Audit   → DEFECT-REVIEW flow. Lens fan-out +
                //             consolidator when lenses are installed;
                //             otherwise degrades to a single call (the
                //             old no-lens audit path).
                //   Generic → one-shot main/fast/slow/goal loop. Single
                //             slow call with slow_system, findings
                //             merger still runs in the reaper. Lens
                //             fan-out is bypassed even when the session
                //             has lenses installed — the classifier
                //             picked Generic precisely because the
                //             multi-angle spread would be overkill for
                //             this prompt.
                let res = match task_mode {
                    kres_agents::TaskMode::Coding | kres_agents::TaskMode::Generic => {
                        orc_task
                            .run_once_with_ctx(&text, &ctx, &handle.shutdown)
                            .await
                    }
                    kres_agents::TaskMode::Audit => {
                        if lenses.is_empty() {
                            orc_task
                                .run_once_with_ctx(&text, &ctx, &handle.shutdown)
                                .await
                        } else if let Some(c) = consolidator {
                            orc_task
                                .run_with_lenses(&text, &lenses, &c, &ctx, &handle.shutdown)
                                .await
                        } else {
                            orc_task
                                .run_once_with_ctx(&text, &ctx, &handle.shutdown)
                                .await
                        }
                    }
                };
                match res {
                    Ok(summary) => {
                        // Slow-agent plan rewrite: when the first
                        // slow call came back with a rewritten plan
                        // (ctx.allow_plan_rewrite=true and the agent
                        // decided to), apply it BEFORE returning the
                        // TaskOutcome so the reaper-tick persist and
                        // the post-task todo-agent update both see
                        // the new plan.
                        if let Some(rewrite) = summary.plan {
                            if let Some(mgr) = handle.manager() {
                                let prior = mgr.plan_snapshot().await;
                                // Merge rewrite's steps with the
                                // prior plan's metadata so a
                                // forgotten prompt / goal / mode /
                                // created_at in the LLM reply
                                // cannot silently clobber
                                // identifying fields.
                                let new_plan = rewrite.apply_to(prior.as_ref());
                                log_plan_change("slow: plan rewrite", prior.as_ref(), &new_plan);
                                mgr.set_plan(Some(new_plan)).await;
                            }
                        }
                        // findings.json is maintained by the reaper
                        // through `FindingsStore::apply_delta` (see
                        // session.rs run()). The per-task delta here
                        // rides in TaskOutcome.findings.
                        Ok(kres_core::task::TaskOutcome {
                            analysis: summary.analysis,
                            findings: summary.findings,
                            followups: summary
                                .followups
                                .iter()
                                .filter_map(|f| serde_json::to_value(f).ok())
                                .collect(),
                            mode: summary.mode,
                            code_output: summary.code_output,
                            code_edits: summary.code_edits,
                        })
                    }
                    Err(e) => Err(e.to_string()),
                }
            })
            .await;
        // Park the goal under the spawned task's id so the reaper
        // can pull it when this specific task finishes.
        if let Some(g) = defined_goal {
            self.task_goals.lock().await.insert(task_id, g);
        }
        // Park the original prompt too — check_goal reads both so
        // it can weigh the operator's literal intent against the
        // derived goal string.
        self.task_prompts
            .lock()
            .await
            .insert(task_id, prompt_for_park);
        kres_core::async_eprintln!("submitted task #{task_id}");
    }

    async fn print_tasks(&self) {
        let snap = self.mgr.snapshot().await;
        // Always emit a header so /tasks is visibly acknowledged
        // even on an empty list. Previously the empty case printed
        // a bare "(no tasks)" which was easy to miss in a busy
        // scrollback; the /tasks: prefix makes it obvious this is
        // the command's response.
        kres_core::async_eprintln!("/tasks: {} active", snap.len());
        for t in &snap {
            let badge = match t.state {
                TaskState::Pending => "pending",
                TaskState::Running => "running",
                TaskState::Cancelling => "cancelling",
                TaskState::Done => "done",
                TaskState::Errored => "errored",
            };
            kres_core::async_eprintln!("  [{:>10}] #{}  {}", badge, t.id, t.name);
        }
    }

    async fn print_findings(&self) {
        let findings = self.mgr.findings_snapshot().await;
        if findings.is_empty() {
            kres_core::async_eprintln!("(no findings yet)");
            return;
        }
        let (hi, med, lo) = findings.iter().fold((0, 0, 0), |(h, m, l), f| {
            use kres_core::findings::Severity::*;
            match f.severity {
                High => (h + 1, m, l),
                Medium => (h, m + 1, l),
                Low => (h, m, l + 1),
            }
        });
        kres_core::async_eprintln!(
            "{} findings: {} high, {} medium, {} low",
            findings.len(),
            hi,
            med,
            lo
        );
        for f in findings.iter().take(20) {
            kres_core::async_eprintln!(
                "  [{:>8?}] {} — {}",
                f.severity,
                f.id,
                truncate(&f.title, 70)
            );
        }
        if findings.len() > 20 {
            kres_core::async_eprintln!("  … {} more", findings.len() - 20);
        }
    }

    async fn cmd_stop(&self) {
        let out = self.mgr.stop_all(self.cfg.stop_grace).await;
        // Latch auto-continue off until the operator explicitly
        // resumes with /continue or submits a new prompt.
        self.stop_latched
            .store(true, std::sync::atomic::Ordering::Release);
        // Wake any reaper-side inference call that's select!'ing on
        // stop_notify so it can abandon mid-flight. No-op when no
        // call is in progress — the latched atomic above catches
        // the next iteration either way.
        self.stop_notify.notify_waiters();
        // Move pending / blocked / in-progress todo items to the
        // deferred list. Done/Skipped items stay on the active
        // queue so the plan step rollup in sync_plan_from_todo can
        // still see their step_id linkage. Flip InProgress to
        // Pending first so `drain_pending_blocked` carries them
        // with the rest. Otherwise /stop leaves the queue full and
        // the next /continue (or the reaper's goal-not-met
        // injection after the next task completes) immediately
        // redispatches what the operator just stopped. Operator
        // can get them back with /followup.
        self.mgr.reset_in_progress_to_pending().await;
        let drained = self.mgr.drain_pending_blocked().await;
        let carry = drained.len();
        let mut deferred = self.deferred.lock().await;
        deferred.extend(drained);
        drop(deferred);
        kres_core::async_eprintln!(
            "/stop: requested={} stopped={} grace_expired={} (auto-continue paused; {} pending item(s) moved to /followup; /continue or a new prompt resumes)",
            out.requested, out.stopped, out.grace_expired, carry
        );
    }

    async fn cmd_continue(&self) {
        use kres_core::TodoStatus;
        // Operator opted back in — clear the /stop auto-continue latch.
        self.stop_latched
            .store(false, std::sync::atomic::Ordering::Release);
        // §22: an interrupted inference wins over batch dispatch.
        // Re-submit the stashed prompt and return — the operator
        // gets their work back before new items start.
        let stashed = self.interrupted_prompt.lock().await.take();
        if let Some(prompt) = stashed {
            kres_core::async_eprintln!(
                "/continue: resuming interrupted prompt: {}",
                truncate(&prompt, 80)
            );
            self.submit_prompt(prompt).await;
            return;
        }
        // Pull any deferred items (from goal-met or --turns drains)
        // back into the active todo list as Pending so they get
        // dispatched here. The "/continue to pursue" message we
        // print on goal-met implies this will happen; without it
        // the operator has to re-type every deferred item by hand.
        {
            let mut deferred = self.deferred.lock().await;
            if !deferred.is_empty() {
                let carry = deferred.len();
                let mut items = self.mgr.todo_snapshot().await;
                let existing: std::collections::BTreeSet<String> =
                    items.iter().map(|i| i.name.clone()).collect();
                for mut d in deferred.drain(..) {
                    if existing.contains(&d.name) {
                        continue;
                    }
                    d.status = TodoStatus::Pending;
                    items.push(d);
                }
                self.mgr.replace_todo(items).await;
                kres_core::async_eprintln!(
                    "/continue: pulled {carry} deferred item(s) into todo list"
                );
            }
        }
        // §15: cap the batch at 10 items per `/continue` to match
        //Items beyond the cap stay pending so the
        // operator can re-issue /continue or let the auto-continue
        // idle loop pick them up.
        const BATCH_CAP: usize = 10;
        let items = self.mgr.todo_snapshot().await;
        let done = done_id_set(&items);
        let mut dispatched = 0usize;
        let mut blocked = 0usize;
        let mut remaining = 0usize;
        for item in &items {
            if item.status != TodoStatus::Pending {
                continue;
            }
            if !item.depends_on.iter().all(|d| done.contains(d)) {
                blocked += 1;
                continue;
            }
            if dispatched >= BATCH_CAP {
                remaining += 1;
                continue;
            }
            let prompt = if item.reason.is_empty() {
                format!("[{}] {}", item.kind, item.name)
            } else {
                format!("[{}] {}: {}", item.kind, item.name, item.reason)
            };
            self.mgr
                .mark_todo_status(&item.name, TodoStatus::InProgress)
                .await;
            let tag = if !item.id.is_empty() {
                item.id.clone()
            } else {
                item.name.clone()
            };
            self.submit_from_pipeline(prompt, Some(tag)).await;
            dispatched += 1;
        }
        let mut msg = format!("/continue: dispatched {dispatched} item(s)");
        if blocked > 0 {
            msg.push_str(&format!(", {blocked} blocked on unfinished deps"));
        }
        if remaining > 0 {
            msg.push_str(&format!(
                ", {remaining} left — /continue again to process next batch"
            ));
        }
        kres_core::async_eprintln!("{msg}");
    }

    async fn cmd_next(&self) {
        use kres_core::TodoStatus;
        let items = self.mgr.todo_snapshot().await;
        // Pick the first item whose dependencies are all done.
        // depends_on contains ids; resolve via done_id_set so the
        // match works for items that came in with explicit agent
        // ids (matches /continue and should_auto_continue).
        let done = done_id_set(&items);
        let next = items.iter().find(|i| {
            i.status == TodoStatus::Pending && i.depends_on.iter().all(|d| done.contains(d))
        });
        let Some(item) = next else {
            let pending = items
                .iter()
                .filter(|i| i.status == TodoStatus::Pending)
                .count();
            if pending == 0 {
                kres_core::async_eprintln!("/next: nothing pending");
            } else {
                kres_core::async_eprintln!(
                    "/next: {} pending item(s) but all are blocked on unfinished deps",
                    pending
                );
            }
            return;
        };
        let prompt = if item.reason.is_empty() {
            format!("[{}] {}", item.kind, item.name)
        } else {
            format!("[{}] {}: {}", item.kind, item.name, item.reason)
        };
        // Mark as in-progress so a second /next doesn't re-dispatch
        // the same item while this one is still running.
        self.mgr
            .mark_todo_status(&item.name, TodoStatus::InProgress)
            .await;
        kres_core::async_eprintln!("/next: dispatching {}", truncate(&item.name, 80));
        let tag = if !item.id.is_empty() {
            item.id.clone()
        } else {
            item.name.clone()
        };
        self.submit_from_pipeline(prompt, Some(tag)).await;
    }

    async fn cmd_edit(&self) {
        let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
        let tmp = std::env::temp_dir().join(format!(
            "kres-edit-{}-{}.md",
            std::process::id(),
            chrono::Utc::now().timestamp_millis()
        ));
        if let Err(e) = std::fs::write(&tmp, "") {
            kres_core::async_eprintln!("/edit: create tempfile: {e}");
            return;
        }
        // Tear down kres's DECSTBM scroll region (status.rs:50) and
        // clear the status row BEFORE handing the terminal to the
        // editor. Without this, vim/nvim paint into a terminal
        // whose bottom two rows sit outside the scroll region: the
        // editor's cursor math and input decoding drift, and key
        // sequences (notably Esc) echo as on-screen garbage
        // instead of reaching the editor. User report 2026-04-21:
        // "Escape key doesn't work".  Reinstalled on return.
        //
        // Also pause the 200ms status-row repainter (see the paint
        // task in Self::run). Without this, the painter continues
        // to absolute-position to row H-1 and write to stderr
        // every tick, scribbling across vim's frame and dragging
        // the visible cursor around. Cleared on return.
        self.status_paused
            .store(true, std::sync::atomic::Ordering::Release);
        crate::status::restore();
        // Handing the terminal to the editor requires blocking on
        // its status. spawn_blocking keeps the runtime alive.
        let editor_path = tmp.clone();
        let editor_cmd = editor.clone();
        let status = tokio::task::spawn_blocking(move || {
            std::process::Command::new(editor_cmd)
                .arg(&editor_path)
                .status()
        })
        .await;
        // Reinstall the scroll region so the status row and REPL
        // prompt re-appear, then un-pause the status painter so it
        // repaints the row on its next tick.
        let _ = crate::status::install();
        self.status_paused
            .store(false, std::sync::atomic::Ordering::Release);
        // Trust the tempfile contents regardless of editor exit code.
        // A `:wq!` forced-quit or the
        // user saving and escaping without a clean exit shouldn't
        // discard the typed prompt. Match that here; only a spawn
        // failure (editor binary missing) drops the content.
        let content = match status {
            Ok(Ok(_)) => std::fs::read_to_string(&tmp).ok(),
            Ok(Err(e)) => {
                kres_core::async_eprintln!("/edit: editor spawn failed: {e}");
                None
            }
            Err(e) => {
                kres_core::async_eprintln!("/edit: join error: {e}");
                None
            }
        };
        // bugs.md#L6: always clean up the tempfile, even on editor
        // failure, to avoid /tmp accretion.
        let _ = std::fs::remove_file(&tmp);
        let Some(text) = content else { return };
        let trimmed = text.trim();
        if trimmed.is_empty() {
            kres_core::async_eprintln!("/edit: empty, nothing submitted");
            return;
        }
        self.submit_prompt(trimmed.to_string()).await;
    }

    async fn cmd_reply(&self, text: String) {
        let prior = {
            let g = self.last_analysis.lock().await;
            g.clone()
        };
        let combined = match (prior, text.trim().is_empty()) {
            (Some(p), false) => format!("{}\n\n{}", p, text),
            (Some(p), true) => p,
            (None, false) => {
                kres_core::async_eprintln!("/reply: no prior analysis — submitting plain text");
                text
            }
            (None, true) => {
                kres_core::async_eprintln!(
                    "/reply: no prior analysis and no new text — nothing to do"
                );
                return;
            }
        };
        self.submit_prompt(combined).await;
    }

    async fn cmd_load(&self, path: String) {
        if path.is_empty() {
            kres_core::async_eprintln!("usage: /load <path>");
            return;
        }
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    kres_core::async_eprintln!("/load: {} is empty", path);
                    return;
                }
                kres_core::async_eprintln!(
                    "/load: submitting {} chars from {}",
                    trimmed.len(),
                    path
                );
                self.submit_prompt(trimmed.to_string()).await;
            }
            Err(e) => kres_core::async_eprintln!("/load: {}: {e}", path),
        }
    }

    async fn cmd_report(&self, path: String) {
        if path.is_empty() {
            kres_core::async_eprintln!("usage: /report <path>.md");
            return;
        }
        let findings = self.mgr.findings_snapshot().await;
        match crate::report::write_findings_to_file(&findings, std::path::Path::new(&path)) {
            Ok(()) => kres_core::async_eprintln!(
                "/report: wrote {} finding(s) to {}",
                findings.len(),
                path
            ),
            Err(e) => kres_core::async_eprintln!("/report: {}: {e}", path),
        }
    }

    /// `/resume [PATH]` — load a persisted snapshot from disk.
    /// Selection order:
    ///   1. Explicit `PATH` argument when given.
    ///   2. `<results>/session.json.prev` — the backup kres moves
    ///      aside on startup when `--resume` was not passed.
    ///   3. `<results>/session.json` — the live file. Useful only
    ///      before any state-mutating command in this session,
    ///      since after that point it reflects the current run.
    ///
    /// Overwrites the current in-memory plan / todo / deferred /
    /// counter. Operators who have already submitted prompts in
    /// this session should expect to lose that work; no merge.
    async fn cmd_resume(&self, path: Option<String>) {
        let chosen: std::path::PathBuf = match path.as_deref() {
            Some(p) => std::path::PathBuf::from(p),
            None => {
                // Derive the backup + live paths from cfg.persist_path.
                let Some(live) = self.cfg.persist_path.as_ref() else {
                    kres_core::async_eprintln!(
                        "/resume: no persist path configured (kres was started \
                         without a results dir)"
                    );
                    return;
                };
                // Same-dir, same-stem, extra ".prev" extension.
                let mut prev = live.clone();
                let prev_name = match live.file_name() {
                    Some(n) => format!("{}.prev", n.to_string_lossy()),
                    None => {
                        kres_core::async_eprintln!("/resume: persist path has no filename");
                        return;
                    }
                };
                prev.set_file_name(prev_name);
                if prev.exists() {
                    prev
                } else if live.exists() {
                    live.clone()
                } else {
                    kres_core::async_eprintln!(
                        "/resume: neither {} nor {} exists — nothing to load",
                        prev.display(),
                        live.display()
                    );
                    return;
                }
            }
        };
        match self.resume_state_from(Some(&chosen)).await {
            Ok(Some(state)) => {
                kres_core::async_eprintln!(
                    "/resume: loaded {} ({} todo, {} deferred, turns done={})",
                    chosen.display(),
                    state.todo.len(),
                    state.deferred.len(),
                    state.completed_run_count,
                );
                if let Some(ref p) = state.last_prompt {
                    kres_core::async_eprintln!("/resume: last prompt: {}", truncate(p, 80));
                }
            }
            Ok(None) => {
                kres_core::async_eprintln!("/resume: {} is missing or empty", chosen.display());
            }
            Err(e) => {
                kres_core::async_eprintln!("/resume: {e}");
            }
        }
    }

    /// `/plan` — show the current plan, produced by `define_plan`
    /// when the operator's last top-level prompt was submitted.
    /// Prints each step with its id + live status; the status
    /// reflects `sync_plan_from_todo`, which the reaper tick runs
    /// before every persist. When no plan exists (goal agent not
    /// configured, or the planner call failed) prints a hint.
    async fn cmd_plan(&self) {
        // Sync once so the status we print matches the linked todo
        // statuses right now, not whatever the planner last wrote.
        self.mgr.sync_plan_from_todo().await;
        let Some(plan) = self.mgr.plan_snapshot().await else {
            kres_core::async_eprintln!(
                "(no plan — either no goal agent configured or define_plan failed on the last prompt)"
            );
            return;
        };
        // Pull the current todo list so we can render links in BOTH
        // directions (step.todo_ids → todos, and todos with
        // matching step_id → step). sync_plan_from_todo above only
        // rolls up status; it does not backfill step.todo_ids, so
        // the step-side list is often empty while todos actually
        // point at the step via their own step_id field.
        let todo = self.mgr.todo_snapshot().await;
        kres_core::async_eprintln!(
            "plan — mode={}, {} step(s)",
            plan.mode.as_str(),
            plan.steps.len()
        );
        kres_core::async_eprintln!("goal: {}", truncate(&plan.goal, 120));
        for s in &plan.steps {
            let status = match s.status {
                kres_core::PlanStepStatus::Pending => "pending",
                kres_core::PlanStepStatus::InProgress => "in-progress",
                kres_core::PlanStepStatus::Done => "done",
                kres_core::PlanStepStatus::Skipped => "skipped",
            };
            kres_core::async_eprintln!("  [{}] {:<11}  {}", s.id, status, truncate(&s.title, 80));
            if !s.description.is_empty() {
                kres_core::async_eprintln!("         — {}", truncate(&s.description, 120));
            }
            // Union of step.todo_ids (down-link) and todos whose
            // step_id matches s.id (up-link). Dedup by the todo's
            // `id` when set, else by `name`. Skip when nothing
            // links either way.
            let mut linked: Vec<&kres_core::TodoItem> = Vec::new();
            for tid in &s.todo_ids {
                if let Some(t) = todo
                    .iter()
                    .find(|i| (!i.id.is_empty() && i.id == *tid) || i.name == *tid)
                {
                    if !linked.iter().any(|lt| std::ptr::eq(*lt, t)) {
                        linked.push(t);
                    }
                }
            }
            for t in &todo {
                if !t.step_id.is_empty()
                    && t.step_id == s.id
                    && !linked.iter().any(|lt| std::ptr::eq(*lt, t))
                {
                    linked.push(t);
                }
            }
            if !linked.is_empty() {
                let labels: Vec<String> = linked
                    .iter()
                    .map(|t| {
                        if !t.id.is_empty() {
                            t.id.clone()
                        } else {
                            t.name.clone()
                        }
                    })
                    .collect();
                kres_core::async_eprintln!("         linked: {}", labels.join(", "));
            }
        }
    }

    /// `/followup` — list items deferred by a goal-met or --turns
    /// cap. Matches command.
    async fn cmd_followup(&self) {
        let def = self.deferred.lock().await;
        // Always emit the banner so /followup is visibly acknowledged
        // even on an empty list — operators otherwise can't tell
        // whether the command ran or the main loop was busy.
        kres_core::async_eprintln!("/followup: {} deferred item(s)", def.len());
        if def.is_empty() {
            return;
        }
        for (i, item) in def.iter().enumerate() {
            kres_core::async_eprintln!(
                "  {:3}. [{}] {}  ({})",
                i + 1,
                item.kind,
                truncate(&item.name, 80),
                match item.status {
                    kres_core::TodoStatus::Pending => "pending",
                    kres_core::TodoStatus::InProgress => "in-progress",
                    kres_core::TodoStatus::Blocked => "blocked",
                    kres_core::TodoStatus::Done => "done",
                    kres_core::TodoStatus::Skipped => "skipped",
                }
            );
            if !item.reason.is_empty() {
                kres_core::async_eprintln!("       — {}", truncate(&item.reason, 120));
            }
        }
    }

    /// `/summary` — render the run's findings.json into a plain-text
    /// bug report via the fast agent using the `summary`
    /// slash-command template. Pass `markdown=true` (via
    /// `/summary-markdown`) to select the markdown-variant template
    /// and default the output filename to `summary.md` instead of
    /// `summary.txt`.
    ///
    /// report.md is NOT consulted. The summariser reads findings.json
    /// via jsondb, runs a per-task condense pass, then renders the
    /// result through the bug-summary template in batches that fit
    /// the fast agent's context window.
    async fn cmd_summary(&self, filename: Option<String>, markdown: bool) {
        let Some(orc) = self.orchestrator.as_ref() else {
            async_println(
                "/summary: orchestrator not configured (need --fast-agent and --slow-agent)",
            );
            return;
        };
        let Some(findings_path) = self.cfg.findings_base.clone() else {
            async_println("/summary: no findings path configured");
            return;
        };
        if !findings_path.exists() {
            async_println(format!(
                "/summary: {} does not exist yet — run at least one task",
                findings_path.display()
            ));
            return;
        }
        // Output goes to the explicit --results dir when the operator
        // set one (so prompt.md, findings.json, and summary.txt all
        // live together). Without --results, fall back to the
        // findings.json's parent — that's still inside the defaulted
        // ~/.kres/sessions/<ts>/ tree, just not flagged as operator-
        // chosen.
        let output_dir = self
            .cfg
            .results_dir
            .clone()
            .or_else(|| findings_path.parent().map(std::path::Path::to_path_buf));
        // /summary-markdown defaults the filename to summary.md
        // instead of summary.txt; --summary-markdown at the CLI
        // behaves the same way.
        let default_name: Option<&str> = match filename.as_deref() {
            Some(_) => None,
            None if markdown => Some("summary.md"),
            None => None,
        };
        let effective_name = filename.as_deref().or(default_name);
        let output_path =
            crate::summary::default_output_path(output_dir.as_deref(), effective_name);
        // Original prompt resolution: in-memory initial_prompt wins
        // (it's the literal --prompt FILE or first submission). If
        // that's empty, look for prompt.md in the results dir; the
        // submit_prompt path saves the first prompt there when
        // --results was configured.
        let original_prompt = match self.initial_prompt.clone() {
            Some(s) if !s.trim().is_empty() => Some(s),
            _ => self.cfg.results_dir.as_ref().and_then(|d| {
                let p = d.join("prompt.md");
                std::fs::read_to_string(&p).ok().and_then(|s| {
                    if s.trim().is_empty() {
                        None
                    } else {
                        Some(s)
                    }
                })
            }),
        };
        let inputs = crate::summary::SummaryInputs {
            findings_path,
            output_path: output_path.clone(),
            template_path: self.cfg.template_path.clone(),
            // `/summary` uses the plain-text template,
            // `/summary-markdown` flips this flag so the summariser
            // reads `summary-markdown` from the user_commands table
            // (with the operator's
            // ~/.kres/commands/summary-markdown.md as an override).
            markdown,
            original_prompt,
            client: orc.fast_client.clone(),
            model: orc.fast_model.clone(),
            max_tokens: orc.fast_max_tokens,
            max_input_tokens: orc.fast_max_input_tokens,
        };
        let label = if markdown {
            "/summary-markdown"
        } else {
            "/summary"
        };
        async_println(format!(
            "{label}: rendering summary to {}",
            output_path.display()
        ));
        if let Err(e) = crate::summary::run_summary(inputs).await {
            async_println(format!("{label}: {e}"));
        }
    }

    /// `/fix <target>` — compose the embedded `fix` slash-command
    /// template with the operator's target string and submit the
    /// result as a new prompt. Uses the same user_commands::compose
    /// path as `--prompt "fix: ..."` so the CLI and REPL share one
    /// code path for the fix flow.
    async fn cmd_fix(&self, target: String) {
        let target = target.trim();
        if target.is_empty() {
            async_println(
                "/fix: expected a target, e.g. /fix ~/local/kernel-bugs/findings/<id> or \
                 /fix race in net/sched/cls_bpf.c free path",
            );
            return;
        }
        let Some((src, body)) = kres_agents::user_commands::compose("fix", target) else {
            async_println(
                "/fix: `fix` template missing from the embedded table — this is a build bug",
            );
            return;
        };
        async_println(format!(
            "/fix: composed prompt from {src} ({} chars)",
            body.len()
        ));
        self.submit_prompt(body).await;
    }

    /// `/review <target>` — compose the embedded `review`
    /// slash-command template with the operator's target string
    /// and submit the result as a new prompt. Uses the same
    /// user_commands::compose path as `--prompt "review: ..."` so
    /// the CLI and REPL share exactly one code path for the
    /// review flow.
    async fn cmd_review(&self, target: String) {
        let target = target.trim();
        if target.is_empty() {
            async_println("/review: expected a target, e.g. /review fs/btrfs/ctree.c");
            return;
        }
        let Some((src, body)) = kres_agents::user_commands::compose("review", target) else {
            async_println(
                "/review: `review` template missing from the embedded table — this is a build bug",
            );
            return;
        };
        async_println(format!(
            "/review: composed prompt from {src} ({} chars)",
            body.len()
        ));
        self.submit_prompt(body).await;
    }

    /// `/extract [--dir D] [--report F] [--todo F] [--findings F]` —
    /// copy session artifacts to operator-chosen destinations. Matches
    async fn cmd_extract(
        &self,
        dir: Option<String>,
        report: Option<String>,
        todo: Option<String>,
        findings: Option<String>,
    ) {
        // Decide destination for each artifact. --dir sets a
        // baseline destination directory; per-file flags override.
        let dir_buf = dir.as_ref().map(std::path::PathBuf::from);
        if let Some(ref d) = dir_buf {
            if let Err(e) = std::fs::create_dir_all(d) {
                kres_core::async_eprintln!("/extract: create {}: {e}", d.display());
                return;
            }
        }
        let resolve = |name: &str, override_: Option<&String>| -> Option<std::path::PathBuf> {
            if let Some(p) = override_ {
                return Some(std::path::PathBuf::from(p));
            }
            dir_buf.as_ref().map(|d| d.join(name))
        };
        // Report: take the findings list and dump it.
        if let Some(p) = resolve("report.md", report.as_ref()) {
            let findings = self.mgr.findings_snapshot().await;
            match crate::report::write_findings_to_file(&findings, &p) {
                Ok(()) => kres_core::async_eprintln!(
                    "/extract: wrote {} finding(s) to {}",
                    findings.len(),
                    p.display()
                ),
                Err(e) => kres_core::async_eprintln!("/extract: report {}: {e}", p.display()),
            }
        }
        // Todo: write current todo list (pending+done) as markdown.
        if let Some(p) = resolve("todo.md", todo.as_ref()) {
            let items = self.mgr.todo_snapshot().await;
            let mut md = String::from("# Todo\n\n");
            for item in &items {
                let check = if item.status == kres_core::TodoStatus::Done {
                    "x"
                } else {
                    " "
                };
                md.push_str(&format!("- [{check}] **[{}]** {}", item.kind, item.name));
                if !item.reason.is_empty() {
                    md.push_str(&format!(" — {}", item.reason));
                }
                md.push('\n');
            }
            match std::fs::write(&p, md) {
                Ok(()) => kres_core::async_eprintln!(
                    "/extract: wrote {} todo(s) to {}",
                    items.len(),
                    p.display()
                ),
                Err(e) => kres_core::async_eprintln!("/extract: todo {}: {e}", p.display()),
            }
        }
        // Findings: dump the structured JSON.
        if let Some(p) = resolve("findings.json", findings.as_ref()) {
            let list = self.mgr.findings_snapshot().await;
            match serde_json::to_string_pretty(&list) {
                Ok(s) => match std::fs::write(&p, s) {
                    Ok(()) => kres_core::async_eprintln!(
                        "/extract: wrote {} finding(s) to {}",
                        list.len(),
                        p.display()
                    ),
                    Err(e) => kres_core::async_eprintln!("/extract: findings {}: {e}", p.display()),
                },
                Err(e) => kres_core::async_eprintln!("/extract: findings serialise: {e}"),
            }
        }
    }

    /// `/done N` — remove the N'th (1-based) pending todo item.
    async fn cmd_done(&self, index: usize) {
        if index == 0 {
            kres_core::async_eprintln!("/done: 1-based index expected");
            return;
        }
        let items = self.mgr.todo_snapshot().await;
        let pending: Vec<&kres_core::TodoItem> = items
            .iter()
            .filter(|t| {
                matches!(
                    t.status,
                    kres_core::TodoStatus::Pending | kres_core::TodoStatus::Blocked
                )
            })
            .collect();
        if index > pending.len() {
            kres_core::async_eprintln!(
                "/done: index {} out of range ({} pending)",
                index,
                pending.len()
            );
            return;
        }
        let target_name = pending[index - 1].name.clone();
        let new_list: Vec<kres_core::TodoItem> = items
            .into_iter()
            .filter(|t| t.name != target_name)
            .collect();
        self.mgr.replace_todo(new_list).await;
        kres_core::async_eprintln!("/done: removed {}", truncate(&target_name, 80));
    }

    /// §46: decide whether the idle loop should auto-launch a
    /// `/continue` on timeout. Conditions (mirroring):
    /// no active tasks, at least one pending todo, and at least one
    /// pending item whose deps are satisfied.
    async fn should_auto_continue(&self) -> bool {
        use kres_core::TodoStatus;
        if self.stop_latched.load(std::sync::atomic::Ordering::Acquire) {
            return false;
        }
        let running = self.mgr.active_count().await;
        if running > 0 {
            return false;
        }
        let items = self.mgr.todo_snapshot().await;
        let done = done_id_set(&items);
        items.iter().any(|i| {
            i.status == TodoStatus::Pending && i.depends_on.iter().all(|d| done.contains(d))
        })
    }

    /// `/todo --clear` — drop every todo item.
    async fn cmd_todo_clear(&self) {
        self.mgr.replace_todo(Vec::new()).await;
        kres_core::async_eprintln!("/todo: cleared");
    }

    async fn print_todo(&self) {
        use kres_core::TodoStatus;
        let items = self.mgr.todo_snapshot().await;
        let pending = items
            .iter()
            .filter(|i| i.status == TodoStatus::Pending)
            .count();
        let running = items
            .iter()
            .filter(|i| i.status == TodoStatus::InProgress)
            .count();
        let done = items
            .iter()
            .filter(|i| i.status == TodoStatus::Done)
            .count();
        // Always emit the banner so /todo is visibly acknowledged
        // even on an empty list — the "/todo:" prefix also makes
        // the response identifiable in a busy scrollback full of
        // agent output.
        kres_core::async_eprintln!(
            "/todo: {} item(s) ({} pending, {} running, {} done)",
            items.len(),
            pending,
            running,
            done
        );
        for i in items.iter().take(30) {
            let badge = match i.status {
                TodoStatus::Pending => "pending",
                TodoStatus::InProgress => "running",
                TodoStatus::Done => "done",
                TodoStatus::Blocked => "blocked",
                TodoStatus::Skipped => "skipped",
            };
            kres_core::async_eprintln!("  [{:>7}] [{}] {}", badge, i.kind, i.name);
        }
        if items.len() > 30 {
            kres_core::async_eprintln!("  … {} more", items.len() - 30);
        }
    }

    fn print_cost(&self) {
        let snap = self.usage.snapshot();
        if snap.is_empty() {
            kres_core::async_eprintln!("(no API usage recorded yet)");
            return;
        }
        let total = self.usage.totals();
        // Show per-row input/output and cache-create/cache-read,
        // plus a total line.
        kres_core::async_eprintln!("usage ({} call(s) total):", total.calls);
        for (k, e) in &snap {
            kres_core::async_eprintln!(
                "  {:>4}/{:<24}  {:>4}×  in={:>9}  out={:>9}  cache_create={:>9}  cache_read={:>9}",
                k.role,
                k.model,
                e.calls,
                fmt_k(e.input_tokens),
                fmt_k(e.output_tokens),
                fmt_k(e.cache_creation_input_tokens),
                fmt_k(e.cache_read_input_tokens),
            );
        }
        kres_core::async_eprintln!(
            "  total         {:>4}×  in={:>9}  out={:>9}  cache_create={:>9}  cache_read={:>9}",
            total.calls,
            fmt_k(total.input_tokens),
            fmt_k(total.output_tokens),
            fmt_k(total.cache_creation_input_tokens),
            fmt_k(total.cache_read_input_tokens),
        );
    }

    async fn cmd_clear(&self) {
        // bugs.md#C2: cancel first, reset state after.
        let out = self.mgr.stop_all(self.cfg.stop_grace).await;
        self.mgr.replace_findings(vec![]).await;
        self.mgr.replace_todo(vec![]).await;
        // Also wipe the accumulated-analysis ledger so the next
        // prompt starts with a clean slate. Without this, the
        // "recent context" preamble submit_prompt injects would
        // keep referencing work the operator just said to forget.
        self.accumulated.lock().await.clear();
        *self.last_analysis.lock().await = None;
        self.deferred.lock().await.clear();
        // Drop the cached session goal too. Without this, the
        // first pipeline-driven follow-up after /clear would
        // inherit the prior topic's goal — exactly the
        // cross-topic bleed /clear exists to prevent.
        *self.session_goal.lock().await = None;
        // Drop every outside-workspace read consent. The store is
        // global (OnceLock); without this a /clear would leave
        // grants from the prior topic in place and a follow-up
        // prompt on a different topic could quietly read paths the
        // operator forgot they'd allowed.
        let dropped_grants = kres_core::consent::get().map(|s| s.clear()).unwrap_or(0);
        kres_core::async_eprintln!(
            "/clear: stopped {} task(s), reset findings + todo + accumulated context, dropped {} consent grant(s)",
            out.stopped + out.grace_expired,
            dropped_grants
        );
    }

    /// `/compact` — run a single fast-agent call that compresses the
    /// accumulated-analysis ledger into one short summary entry.
    /// Subsequent prompts still see continuity ("we did X earlier")
    /// but with a fraction of the tokens. Non-fatal: on failure we
    /// leave the ledger untouched.
    async fn cmd_compact(&self) {
        let entries = self.accumulated.lock().await.clone();
        if entries.len() <= 1 {
            kres_core::async_eprintln!(
                "/compact: nothing to compact (ledger has {} entry)",
                entries.len()
            );
            return;
        }
        let Some(orc) = self.orchestrator.as_ref() else {
            kres_core::async_eprintln!("/compact: no orchestrator configured");
            return;
        };
        // Build the inference request: feed every accumulated entry
        // to the fast agent and ask for a terse single-paragraph
        // summary. Reuse the fast client the orchestrator already
        // holds — cheapest call in the pipeline.
        let mut joined = String::new();
        for (i, e) in entries.iter().enumerate() {
            if i > 0 {
                joined.push_str("\n\n---\n\n");
            }
            joined.push_str(&format!("## {}\n\n{}", e.task, e.analysis));
        }
        let request = serde_json::json!({
            "task": "compact_accumulated",
            "ledger": joined,
            "instructions": "Compress the preceding task-by-task analysis ledger into a single TERSE summary — 2 to 6 sentences total — that preserves: (a) what code was examined, (b) what files were written, if any, (c) key findings or decisions, (d) open questions still worth pulling on. Omit per-task boilerplate and restated code. Return JSON only: {\"summary\": \"the compressed text\"}"
        });
        let body = match serde_json::to_string_pretty(&request) {
            Ok(s) => s,
            Err(e) => {
                kres_core::async_eprintln!("/compact: serialise failed: {e}");
                return;
            }
        };
        let mut cfg = kres_llm::config::CallConfig::defaults_for(orc.fast_model.clone())
            .with_max_tokens(4_000)
            .with_stream_label("compact");
        if let Some(s) = &orc.fast_system {
            cfg = cfg.with_system(s.clone());
        }
        if let Some(n) = orc.fast_max_input_tokens {
            cfg = cfg.with_max_input_tokens(n);
        }
        let messages = vec![kres_llm::request::Message {
            role: "user".into(),
            content: body.clone(),
            cache: false,
            cached_prefix: None,
        }];
        if let Some(lg) = &self.logger {
            lg.log_main("user", &body, None, None);
        }
        let resp = match orc.fast_client.messages_streaming(&cfg, &messages).await {
            Ok(r) => r,
            Err(e) => {
                kres_core::async_eprintln!(
                    "/compact: fast-agent call failed: {e}; ledger unchanged"
                );
                return;
            }
        };
        let text = {
            let mut out = String::new();
            for block in &resp.content {
                if let kres_llm::request::ContentBlock::Text { text } = block {
                    out.push_str(text);
                }
            }
            out
        };
        if let Some(lg) = &self.logger {
            lg.log_main(
                "assistant",
                &text,
                Some(kres_core::LoggedUsage {
                    input: resp.usage.input_tokens,
                    output: resp.usage.output_tokens,
                    cache_creation: resp.usage.cache_creation_input_tokens,
                    cache_read: resp.usage.cache_read_input_tokens,
                }),
                None,
            );
        }
        // The fast agent is expected to reply with
        // {"summary": "..."}. Tolerate prose-wrapped JSON.
        let summary: Option<String> = (|| {
            #[derive(serde::Deserialize)]
            struct CompactResp {
                #[serde(default)]
                summary: String,
            }
            if let Ok(r) = serde_json::from_str::<CompactResp>(text.trim()) {
                return (!r.summary.is_empty()).then_some(r.summary);
            }
            // Brace-match fallback.
            let (start, end) = (text.find('{'), text.rfind('}'));
            if let (Some(s), Some(e)) = (start, end) {
                if let Ok(r) = serde_json::from_str::<CompactResp>(&text[s..=e]) {
                    return (!r.summary.is_empty()).then_some(r.summary);
                }
            }
            None
        })();
        let summary = match summary {
            Some(s) => s,
            None => {
                kres_core::async_eprintln!(
                    "/compact: could not parse a summary from the fast agent; ledger unchanged"
                );
                return;
            }
        };
        let before = entries.len();
        let replaced = AccumulatedEntry {
            task: format!("compacted ({} prior task(s))", before),
            analysis: summary.clone(),
        };
        let mut guard = self.accumulated.lock().await;
        *guard = vec![replaced];
        kres_core::async_eprintln!(
            "/compact: replaced {before} entry(s) with a {}-char summary",
            summary.len()
        );
    }
}

/// Max total size of the "recent context" preamble
/// `submit_prompt` injects ahead of a new operator prompt. The
/// accumulated ledger can grow without bound across a long session;
/// capping here keeps the attached-context cost bounded. Use
/// /compact to trim the ledger itself; this cap only limits what
/// leaks into each new task's prompt.
const RECENT_CONTEXT_CAP_CHARS: usize = 8_000;

/// Render the most recent accumulated-analysis entries into a
/// short preamble, newest-first, capped at `cap` characters.
/// Returns an empty string when the ledger is empty.
fn build_recent_context_preamble(entries: &[AccumulatedEntry], cap: usize) -> String {
    if entries.is_empty() || cap == 0 {
        return String::new();
    }
    let mut out = String::from("Recent context from this session (most recent first):\n\n");
    for e in entries.iter().rev() {
        if out.len() >= cap {
            break;
        }
        let remaining = cap.saturating_sub(out.len());
        // Budget each entry: at most half the remaining cap, so an
        // early giant entry can't starve the rest. Cap at 2k chars
        // per entry regardless.
        let entry_budget = (remaining / 2).clamp(400, 2_000);
        let head: String = e.analysis.chars().take(entry_budget).collect();
        out.push_str(&format!("### {}\n{}", e.task, head));
        if e.analysis.chars().count() > entry_budget {
            out.push_str("\n… (entry truncated)\n");
        } else if !head.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }
    if out.len() > cap {
        out.truncate(cap);
        out.push_str("\n… (preamble truncated at cap)\n");
    }
    out
}

/// Load a `.system.md` prompt from disk-then-embedded, matching the
/// same two-step resolution `AgentConfig::load` uses for
/// `system_file`: an operator's `~/.kres/system-prompts/<basename>`
/// copy wins, otherwise the compiled-in entry from
/// `kres_agents::embedded_prompts` is used. Returns None only when
/// no embedded entry is bundled under this basename (in which case
/// the caller should surface a warning and fall back to its own
/// default — for coding/generic mode this means "use the analysis
/// prompt"; see `pipeline::run_once_with_ctx`).
///
/// The override directory name is `system-prompts/` (not
/// `prompts/`) on purpose: before agent prompts were embedded in
/// the binary, setup.sh populated `~/.kres/prompts/*.system.md`
/// directly, and those leftover files would otherwise be read
/// ahead of the embedded defaults, producing stale behaviour
/// after an upgrade. Moving the override to a new directory name
/// means a fresh kres reads only the embedded prompts until the
/// operator deliberately drops a file under the new path.
fn load_prompt_disk_then_embedded(basename: &str) -> Option<String> {
    if let Some(home) = dirs::home_dir() {
        let p = home.join(".kres").join("system-prompts").join(basename);
        if let Ok(s) = std::fs::read_to_string(&p) {
            if !s.trim().is_empty() {
                return Some(s);
            }
        }
    }
    kres_agents::embedded_prompts::lookup(basename).map(|s| s.to_string())
}

fn load_slow_coding_system() -> Option<String> {
    load_prompt_disk_then_embedded("slow-code-agent-coding.system.md")
}

fn load_slow_generic_system() -> Option<String> {
    load_prompt_disk_then_embedded("slow-code-agent-generic.system.md")
}

/// Convenience: build an Orchestrator from paths to agent configs and
/// a workspace directory. The DataFetcher is a WorkspaceFetcher over
/// the given workspace; MCP integration is a Phase 8 add-on.
/// Built components from a pair of agent configs.
///
/// The Orchestrator is the task runner; the ConsolidatorClient is the
/// fast-agent-flavoured LLM handle used by `run_with_lenses` to merge
/// N parallel lens outputs into a unified analysis + deduplicated
/// findings list.
pub struct BuiltAgents {
    pub orchestrator: Arc<Orchestrator>,
    pub consolidator: Arc<kres_agents::ConsolidatorClient>,
}

#[allow(clippy::too_many_arguments)]
pub async fn build_orchestrator(
    fast_cfg_path: &Path,
    slow_cfg_path: &Path,
    workspace: impl Into<PathBuf>,
    fetcher: Arc<dyn DataFetcher>,
    skills: Option<serde_json::Value>,
    usage: Option<Arc<UsageTracker>>,
    gather_turns: u8,
    logger: Option<Arc<TurnLogger>>,
    settings: &crate::settings::Settings,
) -> Result<BuiltAgents> {
    let fast_cfg = AgentConfig::load(fast_cfg_path)
        .with_context(|| format!("loading fast agent config {}", fast_cfg_path.display()))?;
    let slow_cfg = AgentConfig::load(slow_cfg_path)
        .with_context(|| format!("loading slow agent config {}", slow_cfg_path.display()))?;

    let fast_key = fast_cfg.key.clone();
    let slow_key = slow_cfg.key.clone();

    let fast_model = crate::settings::pick_model(
        fast_cfg.model.as_deref(),
        crate::settings::ModelRole::Fast,
        settings,
    );
    let slow_model = crate::settings::pick_model(
        slow_cfg.model.as_deref(),
        crate::settings::ModelRole::Slow,
        settings,
    );

    // Shared rate limiter keyed by API-key string: agents using the
    // same key share a bucket so they can't collectively burst past
    // the per-key server limit. Capacity comes from whichever config
    // was read first for that key. (Previously keyed on key_file
    // path; now that keys are inline in the config we key on the key
    // string itself, which is equivalent when two configs share a
    // key and correctly separate when they don't.)
    let mut limiters: std::collections::HashMap<String, Arc<RateLimiter>> =
        std::collections::HashMap::new();
    let fast_limiter = fast_cfg
        .rate_limit
        .and_then(|c| RateLimiter::new(c as u64))
        .inspect(|r| {
            limiters.insert(fast_key.clone(), r.clone());
        });
    let slow_limiter = if fast_key == slow_key {
        fast_limiter.clone()
    } else {
        slow_cfg
            .rate_limit
            .and_then(|c| RateLimiter::new(c as u64))
            .inspect(|r| {
                limiters.insert(slow_key.clone(), r.clone());
            })
    };
    let _ = limiters;

    let fast_client = Arc::new(
        Client::builder(fast_key)
            .rate_limiter(fast_limiter)
            .build()?,
    );
    let slow_client = Arc::new(
        Client::builder(slow_key)
            .rate_limiter(slow_limiter)
            .build()?,
    );

    let _workspace = workspace.into(); // retained by caller; fetcher already knows.

    let consolidator = Arc::new(kres_agents::ConsolidatorClient {
        client: fast_client.clone(),
        model: fast_model.clone(),
        system: fast_cfg.system.clone(),
        max_tokens: fast_cfg
            .max_tokens
            .unwrap_or(fast_model.max_output_tokens)
            .min(32_000),
        max_input_tokens: fast_cfg.max_input_tokens,
    });

    let slow_coding_system = load_slow_coding_system();
    let slow_generic_system = load_slow_generic_system();
    let orchestrator = Arc::new(Orchestrator {
        fast_client,
        fast_model: fast_model.clone(),
        fast_system: fast_cfg.system,
        fast_max_tokens: fast_cfg.max_tokens.unwrap_or(fast_model.max_output_tokens),
        fast_max_input_tokens: fast_cfg.max_input_tokens,
        slow_client,
        slow_model: slow_model.clone(),
        slow_system: slow_cfg.system,
        slow_max_tokens: slow_cfg.max_tokens.unwrap_or(slow_model.max_output_tokens),
        slow_max_input_tokens: slow_cfg.max_input_tokens,
        slow_coding_system,
        slow_generic_system,
        fetcher,
        max_fast_rounds: gather_turns,
        skills,
        usage,
        logger,
    });

    Ok(BuiltAgents {
        orchestrator,
        consolidator,
    })
}

/// Print a one-line summary of a reaped task.
/// Write code_output files emitted by a Coding-mode task.
///
/// Path handling, mirroring the rule `edit_file` already uses for
/// outside-workspace edits (kres-agents/src/tools.rs:resolve_workspace):
///
///   * Relative paths land at `<workspace>/<path>` — same default
///     `<workspace>` rooting that's served the in-tree coding flow.
///   * Absolute paths are accepted ONLY when they resolve under the
///     workspace OR under a directory the operator named in a prompt
///     this session (granted via `consent::grant_paths_from_text`).
///     This is what lets a triage prompt that names an absolute bug
///     folder receive `summary.md` writes there directly, without
///     dropping write-anywhere across the FS.
///   * `..` traversal segments are always rejected — they don't make
///     sense in either rooting and are how a malformed reply would
///     try to escape both the workspace and the consent gate.
///
/// Each file is written with a tmp + rename so a crash doesn't leave
/// a partial artifact.
/// One applied (or attempted) CodeEdit. The reaper folds these
/// back into the task's analysis trailer so a failure ("old_string
/// not found", "ambiguous match") is visible to the NEXT slow-agent
/// turn instead of dying on stderr.
pub(crate) struct AppliedEdit {
    pub file_path: String,
    /// `Ok(msg)` carries the per-edit success preview from
    /// `edit_file` (replacement count + before/after sizes +
    /// 5-line context snippet). `Err(msg)` carries the error text
    /// the slow agent needs to see to correct its next emission.
    pub result: Result<String, String>,
}

/// Apply each CodeEdit emitted by a coding-mode task to its target
/// file on disk via kres_agents::tools::edit_file. Returns a vector
/// of `AppliedEdit`s so the reaper can fold outcomes into the
/// task's analysis trailer; also logs one line per edit to stderr
/// for the operator. Edits apply in emission order — a later edit
/// whose `old_string` was invalidated by an earlier one in the same
/// batch will fail with a normal "not found" error; the caller
/// (slow agent) sees that in the trailer and can re-emit.
async fn apply_code_edits(
    workspace: &Path,
    task_name: &str,
    edits: &[kres_core::CodeEdit],
) -> Vec<AppliedEdit> {
    let mut results: Vec<AppliedEdit> = Vec::with_capacity(edits.len());
    let mut applied = 0usize;
    let mut failed = 0usize;
    for e in edits {
        let args = kres_agents::tools::EditArgs {
            file_path: e.file_path.clone(),
            old_string: e.old_string.clone(),
            new_string: e.new_string.clone(),
            replace_all: e.replace_all,
        };
        match kres_agents::tools::edit_file(workspace, &args).await {
            Ok(msg) => {
                applied += 1;
                kres_core::async_eprintln!("[coding-edit] {msg}");
                results.push(AppliedEdit {
                    file_path: e.file_path.clone(),
                    result: Ok(msg),
                });
            }
            Err(err) => {
                failed += 1;
                let text = err.to_string();
                kres_core::async_eprintln!("[coding-edit] {}: {text}", e.file_path);
                results.push(AppliedEdit {
                    file_path: e.file_path.clone(),
                    result: Err(text),
                });
            }
        }
    }
    kres_core::async_eprintln!(
        "[coding-edit] {task_name}: applied {applied}/{} edit(s) ({failed} failed)",
        edits.len()
    );
    results
}

/// Render the list of AppliedEdit into a trailer section for the
/// task's analysis text. Failed edits are called out with
/// "[FAILED]" so the next slow-agent turn can grep for them; the
/// full error message is included verbatim so the model has the
/// exact anchor text it needs to re-emit a corrected edit.
pub(crate) fn format_applied_edits_trailer(edits: &[AppliedEdit]) -> String {
    if edits.is_empty() {
        return String::new();
    }
    let applied = edits.iter().filter(|e| e.result.is_ok()).count();
    let failed = edits.len() - applied;
    let mut s = String::new();
    s.push_str("\n---\nEdits applied (");
    s.push_str(&applied.to_string());
    s.push('/');
    s.push_str(&edits.len().to_string());
    if failed > 0 {
        s.push_str(", ");
        s.push_str(&failed.to_string());
        s.push_str(" FAILED");
    }
    s.push_str("):\n");
    for e in edits {
        match &e.result {
            Ok(msg) => {
                s.push_str("- ");
                s.push_str(&e.file_path);
                // msg starts with "[edit <abs>] N replacement(s) (..."
                // — drop the `[edit <abs>] ` prefix to keep the trailer
                // tight; the path is already on the line.
                let tail = msg.split_once("] ").map(|x| x.1).unwrap_or(msg);
                s.push_str(": ");
                // Only keep the first line of the preview block — the
                // full 5-line context lives in the stderr log.
                let first = tail.split('\n').next().unwrap_or(tail);
                s.push_str(first);
                s.push('\n');
            }
            Err(err) => {
                s.push_str("- [FAILED] ");
                s.push_str(&e.file_path);
                s.push_str(": ");
                s.push_str(err);
                s.push('\n');
            }
        }
    }
    s
}

async fn persist_code_output(workspace: &Path, task_name: &str, files: &[kres_core::CodeFile]) {
    let base = workspace.to_path_buf();
    if let Err(e) = tokio::fs::create_dir_all(&base).await {
        kres_core::async_eprintln!("[coding] create {} failed: {e}", base.display());
        return;
    }
    let ws_canon = base.canonicalize().unwrap_or_else(|_| base.clone());
    let mut wrote = 0usize;
    for f in files {
        let rel = std::path::Path::new(&f.path);
        if rel
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            kres_core::async_eprintln!(
                "[coding] rejecting suspicious path '{}' (contains '..')",
                f.path
            );
            continue;
        }
        let out = if rel.is_absolute() {
            let allowed = rel.starts_with(&ws_canon)
                || kres_core::consent::get()
                    .map(|s| s.is_allowed(rel))
                    .unwrap_or(false);
            if !allowed {
                kres_core::async_eprintln!(
                    "[coding] rejecting absolute path '{}' (outside workspace and no consent on file — mention the containing directory in a prompt to grant write access)",
                    f.path
                );
                continue;
            }
            rel.to_path_buf()
        } else {
            base.join(rel)
        };
        if let Some(parent) = out.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                kres_core::async_eprintln!("[coding] mkdir {} failed: {e}", parent.display());
                continue;
            }
        }
        // tmp + rename so a crash leaves either the old content or
        // the new content, never a truncated partial.
        let tmp = out.with_extension(format!(
            "{}.tmp",
            out.extension().and_then(|e| e.to_str()).unwrap_or("")
        ));
        if let Err(e) = tokio::fs::write(&tmp, f.content.as_bytes()).await {
            kres_core::async_eprintln!("[coding] write {} failed: {e}", tmp.display());
            continue;
        }
        if let Err(e) = tokio::fs::rename(&tmp, &out).await {
            kres_core::async_eprintln!(
                "[coding] rename {} -> {} failed: {e}",
                tmp.display(),
                out.display()
            );
            continue;
        }
        wrote += 1;
        kres_core::async_eprintln!(
            "[coding] wrote {} ({})",
            out.display(),
            if f.purpose.is_empty() {
                "no purpose given".to_string()
            } else {
                f.purpose.clone()
            }
        );
    }
    kres_core::async_eprintln!(
        "[coding] {}: persisted {}/{} file(s) under {}",
        task_name,
        wrote,
        files.len(),
        base.display()
    );
}

fn report_reaped(r: &kres_core::ReapedTask) {
    match r.state {
        kres_core::TaskState::Done => {
            kres_core::async_eprintln!(
                "== done #{} {} ({} findings, {} char analysis)",
                r.id,
                truncate(&r.name, 60),
                r.findings_delta.len(),
                r.analysis.len(),
            );
            // Print the analysis body. Previously only a one-line
            // summary reached the screen, so an operator who didn't
            // know about /summary would see agent-traffic lines fly
            // past and then ... nothing. Full body on stdout matches
            // the 's behaviour.
            //
            // Route the body through the markdown sink so the TUI
            // render path can style fenced code / inline backticks
            // via tui_markdown. The sink is only installed by
            // `install_tui_printer`; --stdio and rustyline paths
            // leave it empty and fold straight back to
            // `async_println`, so their output is unchanged.
            if !r.analysis.is_empty() {
                kres_core::async_eprintln!("");
                kres_core::io::async_println_markdown(&r.analysis);
                kres_core::async_eprintln!("");
            }
        }
        kres_core::TaskState::Errored => {
            kres_core::async_eprintln!(
                "== error #{} {} — {}",
                r.id,
                truncate(&r.name, 60),
                r.error.as_deref().unwrap_or("(no error text)")
            );
        }
        _ => {}
    }
}

fn read_stdin(tx: mpsc::UnboundedSender<String>, mut ack_rx: mpsc::UnboundedReceiver<()>) {
    // rustyline: line-editing + ^R history search + arrow-key recall.
    // History persists to $HOME/.kres/history. Falls back to plain
    // stdin on any rustyline init failure so a weird terminal doesn't
    // brick the REPL.
    use rustyline::{Cmd, KeyCode, KeyEvent, Modifiers};

    let history_path = dirs::home_dir().map(|h| h.join(".kres").join("history"));
    let mut editor = match rustyline::DefaultEditor::new() {
        Ok(e) => e,
        Err(err) => {
            tracing::warn!(target: "kres_repl", "rustyline init failed: {err}; falling back");
            return read_stdin_plain(tx);
        }
    };
    // §21: install a global printer channel so async sites can push
    // lines through rustyline's ExternalPrinter without redrawing
    // over the in-progress buffer. The handler is registered into
    // kres_core::io so agents/llm crates can reach it via
    // async_println without a kres-repl dep.
    if let Ok(mut printer) = editor.create_external_printer() {
        let (ptx, mut prx) = tokio::sync::mpsc::unbounded_channel::<String>();
        // `replace_printer` rather than `install_printer`: the
        // caller in Session::run already installed a stdout-
        // bootstrap printer so `print_banner` and friends had a
        // sink. Now that the ExternalPrinter is ready, take over so
        // subsequent lines arrive through the prompt-aware channel.
        kres_core::io::replace_printer(Box::new(move |s| {
            let _ = ptx.send(s);
        }));
        std::thread::spawn(move || {
            use tokio::runtime::Handle;
            let handle = Handle::try_current().ok();
            let drain = async move {
                while let Some(line) = prx.recv().await {
                    use rustyline::ExternalPrinter as _;
                    if let Err(e) = printer.print(format!("{line}\n")) {
                        kres_core::async_eprintln!("external printer: {e}\n{line}");
                    }
                }
            };
            if let Some(h) = handle {
                h.block_on(drain);
            } else {
                // Best-effort fallback when no tokio runtime is
                // reachable from this thread.
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                if let Ok(rt) = rt {
                    rt.block_on(drain);
                }
            }
        });
    }
    // §43: Ctrl-G submits `/edit` so the operator can open $EDITOR
    // on a scratch file. Matches \C-a\C-k/edit\C-m` binding
    // at. rustyline lets us bind a single
    // key-event to either a Cmd::Insert-then-AcceptLine sequence or
    // a dedicated command — we approximate by binding Ctrl-G to
    // "kill line, insert /edit, accept". The sequence is expressed
    // as a chain by calling bind_sequence repeatedly.
    editor.bind_sequence(
        KeyEvent::new('g', Modifiers::CTRL),
        Cmd::Insert(1, "/edit".to_string()),
    );
    // §43: also honour Shift-Enter / Alt-Enter / CSI-u forms as
    // literal-newline inputs so multi-line prompts work without
    // submit. rustyline binds to Cmd::Newline.
    for key in [
        KeyEvent(KeyCode::Enter, Modifiers::SHIFT),
        KeyEvent(KeyCode::Enter, Modifiers::ALT),
    ] {
        editor.bind_sequence(key, Cmd::Newline);
    }
    if let Some(ref p) = history_path {
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = editor.load_history(p);
    }
    let mut first_prompt = true;
    loop {
        // After the first line, wait for the main loop to
        // ack-complete the previous command before printing the
        // next "> " prompt. Without this, readline() fires again
        // the moment tx.send returns, and rustyline paints the
        // prompt on top of vim's frame as soon as "/edit" is
        // sent — well before cmd_edit has had a chance to take
        // over the terminal. On None (channel closed) we break
        // out; the REPL is tearing down.
        if !first_prompt && ack_rx.blocking_recv().is_none() {
            break;
        }
        first_prompt = false;
        match editor.readline("> ") {
            Ok(line) => {
                if !line.trim().is_empty() {
                    let _ = editor.add_history_entry(line.as_str());
                }
                if tx.send(line).is_err() {
                    break;
                }
            }
            Err(rustyline::error::ReadlineError::Interrupted) => {
                // Ctrl-C at the prompt: send empty line; the outer
                // Ctrl-C handler in run() already handles cancel.
                let _ = tx.send(String::new());
            }
            Err(rustyline::error::ReadlineError::Eof) => break,
            Err(_) => break,
        }
    }
    if let Some(ref p) = history_path {
        let _ = editor.save_history(p);
    }
}

/// Fallback reader when rustyline can't initialise (non-tty stdin
/// under `echo ... | kres repl`, or exotic terminals).
fn read_stdin_plain(tx: mpsc::UnboundedSender<String>) {
    use std::io::BufRead as _;
    let stdin = std::io::stdin();
    let mut lock = stdin.lock();
    let mut line = String::new();
    loop {
        line.clear();
        match lock.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                let s = line.trim_end_matches(['\r', '\n']).to_string();
                if tx.send(s).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

fn print_banner() {
    // §34: banner parity with. Session/logs/agent
    // lines are printed to stderr by the caller before run() starts
    // (see main.rs). Here we emit the header + the quick-command
    // hint — the per-run context (skills, artifacts dir, etc.) is
    // already on stderr by the time the REPL loop starts.
    kres_core::async_eprintln!("kres — kernel code research agent");
    kres_core::async_eprintln!("type /help for commands, /quit to exit");
    kres_core::async_eprintln!("ctrl-g: editor  |  /clear: reset  |  /quit: exit");
}

fn print_help() {
    kres_core::async_eprintln!("commands:");
    kres_core::async_eprintln!("  /help, /?              show this help");
    kres_core::async_eprintln!("  /tasks, /task          list running tasks");
    kres_core::async_eprintln!("  /findings              summarise findings");
    kres_core::async_eprintln!("  /stop                  cancel running tasks");
    kres_core::async_eprintln!(
        "  /clear                 stop tasks, reset findings + todo + accumulated context"
    );
    kres_core::async_eprintln!(
        "  /compact               summarise accumulated context into one short entry"
    );
    kres_core::async_eprintln!("  /cost                  show API token usage");
    kres_core::async_eprintln!("  /todo                  show the todo list");
    kres_core::async_eprintln!(
        "  /plan                  show the current plan (produced by define_plan)"
    );
    kres_core::async_eprintln!(
        "  /resume [PATH]         load a persisted session.json (backup, live, or PATH)"
    );
    kres_core::async_eprintln!("  /report <path>         write findings report (markdown)");
    kres_core::async_eprintln!(
        "  /load <path>           submit a file's contents as the next prompt"
    );
    kres_core::async_eprintln!(
        "  /edit                  open $EDITOR on a scratch file, submit on save"
    );
    kres_core::async_eprintln!("  /followup              list items deferred by goal/--turns");
    kres_core::async_eprintln!(
        "  /review <target>       compose the embedded `review` template with <target> and submit"
    );
    kres_core::async_eprintln!(
        "  /fix <target>          compose the embedded `fix` template (finding dir or prose) and submit"
    );
    kres_core::async_eprintln!("  /summary [FILE]        render report.md+findings.json into a plain-text summary (default summary.txt)");
    kres_core::async_eprintln!(
        "  /summary-markdown [FILE]  render the markdown variant (default summary.md)"
    );
    kres_core::async_eprintln!(
        "  /extract ...           copy artifacts (--dir, --report, --todo, --findings)"
    );
    kres_core::async_eprintln!("  /done N                remove the N'th pending todo");
    kres_core::async_eprintln!("  /todo --clear          drop every todo item");
    kres_core::async_eprintln!(
        "  /reply <text>          prepend last analysis to new text, submit"
    );
    kres_core::async_eprintln!(
        "  /next                  dispatch the next pending todo item as a prompt"
    );
    kres_core::async_eprintln!("  /continue              dispatch every unblocked pending todo");
    kres_core::async_eprintln!("  /quit, /exit           leave the REPL");
    kres_core::async_eprintln!("  <anything else>        submit as a prompt");
    kres_core::async_eprintln!("");
    kres_core::async_eprintln!(
        "override slash-command templates by dropping a file at ~/.kres/commands/<name>.md"
    );
}

/// Followup types the reaper executes directly (instead of routing
/// through the main agent and a follow-up task). The list is the
/// single source of truth for both the dispatch loop and the
/// todo-agent input filter, so adding a new reaper-handled type
/// only requires one entry here.
#[derive(Debug, Clone, Copy)]
enum ReaperFollowup {
    Git,
    PublishFix,
}

impl ReaperFollowup {
    fn label(self) -> &'static str {
        match self {
            ReaperFollowup::Git => "git",
            ReaperFollowup::PublishFix => "publish-fix",
        }
    }
}

fn reaper_followup_kind(fu: &serde_json::Value) -> Option<ReaperFollowup> {
    match fu.get("type").and_then(|v| v.as_str()) {
        Some("git") => Some(ReaperFollowup::Git),
        Some("publish-fix") => Some(ReaperFollowup::PublishFix),
        _ => None,
    }
}

/// Build a set of ids (falling back to name when id is empty) for
/// done TodoItems. Used by `cmd_continue` and `should_auto_continue`
/// to resolve `depends_on` — which contains ids, not names.
fn done_id_set(items: &[kres_core::TodoItem]) -> std::collections::BTreeSet<String> {
    items
        .iter()
        .filter(|i| i.status == kres_core::TodoStatus::Done)
        .map(|i| {
            if i.id.is_empty() {
                i.name.clone()
            } else {
                i.id.clone()
            }
        })
        .collect()
}

/// Execute a git followup from the reaper.
///
/// For `commit` followups the slow agent must FIRST write the
/// commit message to a workspace file via a `code_output` entry,
/// THEN reference that file with `-F <path>` in the followup
/// command:
///
/// ```text
/// code_output: [{path: ".kres-commit-msg.tmp", content: "..."}]
/// followup:   {type: "git", name: "commit -s -F .kres-commit-msg.tmp"}
/// ```
///
/// The reaper applies code_output before processing followups so
/// the file is already on disk when this function runs. We read
/// it back to validate line lengths (kernel rule: prose wraps at
/// 75 cols; reject if any non-trailer non-indented line exceeds
/// 100), then hand the command to the existing git tool. `-m` is
/// rejected outright so the agent never reverts to embedded
/// message strings.
///
/// Non-commit git commands (add, diff, log, etc.) pass straight
/// through to `kres_agents::tools::git`.
async fn run_reaper_git(workspace: &Path, command: &str) -> String {
    let trimmed = command.trim();
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    let subcommand = tokens.first().copied().unwrap_or("");
    let label = if subcommand.is_empty() {
        "git"
    } else {
        subcommand
    };

    if subcommand == "commit" {
        if let Err(rejection) = validate_commit_command(workspace, &tokens).await {
            return rejection;
        }
    }

    let args = kres_agents::tools::GitArgs {
        command: command.to_string(),
    };
    match kres_agents::tools::git(workspace, &args).await {
        Ok(out) => format!("[git {label}] {}", out.trim()),
        Err(e) => format!("[git {label} FAILED] {e}"),
    }
}

/// Inspect a tokenised `git commit ...` invocation before sending it
/// to the git tool. Returns Err with the reaper-style rejection text
/// when the slow agent has emitted an unsupported shape:
///
/// - `-m` / `--message` in any form: the reaper requires the message
///   to come from a file via `-F` so the line-wrap validator can run
///   and so multi-paragraph bodies survive without -m juggling.
/// - missing `-F <path>` on initial commits: ditto.
///
/// `git commit --amend` without `-F` is allowed — git reuses the
/// previous message, which is exactly what the FIX flow's step 4
/// (fold compile-warning fixes into the original commit) wants.
async fn validate_commit_command(workspace: &Path, tokens: &[&str]) -> Result<(), String> {
    if let Some(t) = tokens.iter().find(|t| token_is_message_flag(t)) {
        return Err(format!(
            "[git commit REJECTED] do not pass `-m` / `--message` ({t}); \
             write the full commit message to a workspace file via a \
             `code_output` entry (e.g. `.kres-commit-msg.tmp`), then \
             point at it with `-F <path>` in the git command."
        ));
    }
    let has_amend = tokens.contains(&"--amend");
    let mut msg_path: Option<&str> = None;
    for pair in tokens.windows(2) {
        if pair[0] == "-F" || pair[0] == "--file" {
            msg_path = Some(pair[1]);
            break;
        }
    }
    // Trailing -F with no path: git itself would reject too, but we
    // want a kres-shaped error so the slow agent's analysis trailer
    // is actionable.
    if msg_path.is_none() && tokens.last().is_some_and(|t| *t == "-F" || *t == "--file") {
        return Err("[git commit REJECTED] `-F` flag has no path argument".into());
    }
    let Some(msg_path) = msg_path else {
        if has_amend {
            // --amend reuses the existing message; nothing to validate.
            return Ok(());
        }
        return Err(
            "[git commit REJECTED] missing `-F <path>`. Write the commit \
             message to a workspace file via `code_output` first, then \
             commit with `-F <that-path>`."
                .into(),
        );
    };

    let full_path = if std::path::Path::new(msg_path).is_absolute() {
        std::path::PathBuf::from(msg_path)
    } else {
        workspace.join(msg_path)
    };
    let message = tokio::fs::read_to_string(&full_path).await.map_err(|e| {
        format!(
            "[git commit FAILED] cannot read commit message file {}: {e}",
            full_path.display()
        )
    })?;
    if let Some((idx, bad_line)) = find_overlong_commit_line(&message, 100) {
        return Err(format!(
            "[git commit REJECTED] {} line {} is {} chars (>100); \
             wrap prose at 75 cols (kernel rule). Re-emit a corrected \
             `code_output` entry for this file, then retry the commit. \
             Offending line: {}",
            full_path.display(),
            idx + 1,
            bad_line.chars().count(),
            truncate(bad_line, 80),
        ));
    }
    Ok(())
}

/// True when `t` is any spelling of the git `-m` / `--message` flag:
/// `-m`, `-m<msg>` (no space, valid git syntax), `--message`,
/// `--message=<msg>`. Distinct from other `-m*` and `--m*` flags
/// (`-M`, `--metadata`, `--mailto`, `--minimal`) which we want to
/// pass through untouched.
fn token_is_message_flag(t: &str) -> bool {
    if matches!(t, "-m" | "--message") {
        return true;
    }
    if t.starts_with("--message=") {
        return true;
    }
    // `-mFOO` or `-m"foo"` — short-form with no space. Reject the
    // double-dash family (`--metadata`, `--mailto`) which also
    // starts with `-m`.
    t.starts_with("-m") && t.len() > 2 && !t.starts_with("--")
}

async fn git_rev_parse_head(workspace: &Path) -> Option<String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.current_dir(workspace);
    cmd.args(["rev-parse", "HEAD"]);
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let out = tokio::time::timeout(std::time::Duration::from_secs(5), cmd.output())
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (sha.len() == 40 && sha.chars().all(|c| c.is_ascii_hexdigit())).then_some(sha)
}

/// Publish the workspace's HEAD commit as `auto-generated-fix.diff`
/// inside a kres finding directory. Triggered by the slow agent's
/// `publish-fix` followup as the last step of the FIX flow, after
/// build and review have passed. The argument is the absolute path
/// to a finding directory (the kres --export shape with
/// `metadata.yaml`, `FINDING.md`, `summary.md`).
///
/// On success the directory gains:
/// - `auto-generated-fix.diff` — the output of
///   `git format-patch -1 --stdout HEAD` from the workspace.
/// - `metadata.yaml` gains an `auto_generated_fix:` key naming
///   the patch file (idempotent — skipped if already present).
/// - `summary.md`'s cross-link header gains a third link
///   pointing at the patch (idempotent).
///
/// Failures append a `[publish-fix FAILED] ...` line to the
/// returned trailer text but do not abort the run.
async fn run_publish_fix(workspace: &Path, finding_dir: &str) -> String {
    let dir = std::path::PathBuf::from(finding_dir);
    if !dir.is_absolute() {
        return format!("[publish-fix FAILED] finding_dir must be absolute: {finding_dir}");
    }
    let metadata_path = dir.join("metadata.yaml");
    let finding_path = dir.join("FINDING.md");
    if !metadata_path.exists() || !finding_path.exists() {
        return format!(
            "[publish-fix FAILED] {finding_dir} is not a kres finding directory \
             (missing metadata.yaml or FINDING.md)"
        );
    }

    let fix_path = dir.join("auto-generated-fix.diff");

    // Skip when auto-generated-fix.diff already records the current
    // HEAD. The FIX flow has two paths that emit `publish-fix`: the
    // review task's slow agent ends with one, and the dedicated
    // `publish-fix` plan step emits another. Without this guard
    // `git format-patch` runs twice for the same SHA and rewrites
    // an identical file. `git format-patch -1 --stdout HEAD` opens
    // each patch with `From <40-hex-sha> Mon Sep 17 00:00:00 2001`,
    // so comparing that prefix to `git rev-parse HEAD` is enough to
    // detect "already published this commit". A real amend changes
    // HEAD's sha and falls through to the rewrite path.
    if let Some(head_sha) = git_rev_parse_head(workspace).await {
        if let Ok(existing) = tokio::fs::read_to_string(&fix_path).await {
            if existing
                .lines()
                .next()
                .is_some_and(|l| l.starts_with(&format!("From {head_sha} ")))
            {
                return format!(
                    "[publish-fix] {} already up to date for HEAD {}",
                    fix_path.display(),
                    &head_sha[..12.min(head_sha.len())],
                );
            }
        }
    }

    // Run `git format-patch -1 --stdout HEAD` in the workspace.
    let mut cmd = tokio::process::Command::new("git");
    cmd.current_dir(workspace);
    cmd.args(["format-patch", "-1", "--stdout", "HEAD"]);
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let out = match tokio::time::timeout(std::time::Duration::from_secs(30), cmd.output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return format!("[publish-fix FAILED] git format-patch spawn: {e}"),
        Err(_) => return "[publish-fix FAILED] git format-patch timed out".to_string(),
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return format!(
            "[publish-fix FAILED] git format-patch exited {}: {}",
            out.status.code().unwrap_or(-1),
            stderr.trim()
        );
    }
    let patch = String::from_utf8_lossy(&out.stdout).into_owned();
    if patch.is_empty() {
        return "[publish-fix FAILED] git format-patch produced empty output".to_string();
    }

    if let Err(e) = tokio::fs::write(&fix_path, &patch).await {
        return format!("[publish-fix FAILED] write {}: {e}", fix_path.display());
    }

    // Update metadata.yaml: append `auto_generated_fix:
    // auto-generated-fix.diff` if not already present. Avoid YAML
    // libraries here — kres findings ship as flat key:value lines
    // and a textual append is robust.
    match tokio::fs::read_to_string(&metadata_path).await {
        Ok(metadata) => {
            if !metadata
                .lines()
                .any(|l| l.trim_start().starts_with("auto_generated_fix:"))
            {
                let mut updated = metadata.trim_end().to_string();
                updated.push('\n');
                updated.push_str("auto_generated_fix: auto-generated-fix.diff\n");
                if let Err(e) = tokio::fs::write(&metadata_path, updated).await {
                    return format!(
                        "[publish-fix FAILED] write {}: {e}",
                        metadata_path.display()
                    );
                }
            }
        }
        Err(e) => {
            return format!("[publish-fix FAILED] read {}: {e}", metadata_path.display());
        }
    }

    // Update summary.md: extend the cross-link header line with a
    // link to the patch. Idempotent: skip when already present.
    let summary_path = dir.join("summary.md");
    if summary_path.exists() {
        match tokio::fs::read_to_string(&summary_path).await {
            Ok(summary) => {
                let link = "[auto-generated-fix.diff](auto-generated-fix.diff)";
                if !summary.contains(link) {
                    // Look for the cross-link header
                    // ([FINDING.md]...|[metadata.yaml]...) and append
                    // ` | <link>`. If absent, prepend the link as a
                    // standalone first line.
                    let cross_link = "[FINDING.md](FINDING.md) | [metadata.yaml](metadata.yaml)";
                    let updated = if let Some(pos) = summary.find(cross_link) {
                        let end = pos + cross_link.len();
                        format!("{} | {}{}", &summary[..end], link, &summary[end..])
                    } else {
                        format!("{link}\n\n{summary}")
                    };
                    if let Err(e) = tokio::fs::write(&summary_path, updated).await {
                        return format!(
                            "[publish-fix FAILED] write {}: {e}",
                            summary_path.display()
                        );
                    }
                }
            }
            Err(e) => {
                return format!("[publish-fix FAILED] read {}: {e}", summary_path.display());
            }
        }
    }

    format!(
        "[publish-fix] wrote {} ({} bytes), updated metadata.yaml + summary.md",
        fix_path.display(),
        patch.len()
    )
}

/// Walk the commit message line-by-line and return the first line
/// (with its 0-based index) that exceeds `cap` characters and is
/// not exempt from the wrap rule.
///
/// Exempt lines, per submitting-patches.rst:
/// - Trailer tags (`Word(-word)*: value`) — line 148 says tags are
///   "exempt from the wrap-at-75-columns rule in order to simplify
///   parsing scripts".
/// - Lines indented by 4+ spaces or a tab — quoted code per
///   submitting-patches.rst:792-805.
///
/// Returns `None` when every line is within the cap.
fn find_overlong_commit_line(msg: &str, cap: usize) -> Option<(usize, &str)> {
    for (idx, line) in msg.lines().enumerate() {
        if line.chars().count() <= cap {
            continue;
        }
        if is_trailer_line(line) {
            continue;
        }
        if line.starts_with("    ") || line.starts_with('\t') {
            continue;
        }
        return Some((idx, line));
    }
    None
}

/// Detect a kernel-style trailer tag from
/// Documentation/process/submitting-patches.rst. Either:
///
/// - a known single-word tag (`Fixes:`, `Closes:`, `Link:`, `Cc:`,
///   `BugLink:`, `Bug:`), OR
/// - a hyphenated multi-word tag conventionally ending in `-by` or
///   `-on` (`Reported-by:`, `Signed-off-by:`, `Co-developed-by:`,
///   `Tested-by:`, `Reviewed-by:`, `Acked-by:`, `Suggested-by:`,
///   `Assisted-by:`, `Based-on:`).
///
/// Stricter than "any `Word: …` prefix" on purpose: prose lines
/// that happen to start with `Note:`, `Example:`, `Fix:` would
/// otherwise be exempted from the 100-char cap and hide real
/// over-long body content.
fn is_trailer_line(line: &str) -> bool {
    let Some(colon) = line.find(':') else {
        return false;
    };
    let key = &line[..colon];
    if key.is_empty() {
        return false;
    }
    let first = key.chars().next().unwrap();
    if !first.is_ascii_uppercase() {
        return false;
    }
    if !key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return false;
    }
    const SINGLE_WORD: &[&str] = &["Fixes", "Closes", "Link", "Cc", "Bug", "BugLink"];
    if SINGLE_WORD.contains(&key) {
        return true;
    }
    if key.contains('-') {
        return key.ends_with("-by") || key.ends_with("-on");
    }
    false
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let head: String = s.chars().take(n).collect();
    format!("{head}…")
}

/// Log a plan replacement to the REPL, with a change summary
/// against the prior plan (if any). `source` names the writer
/// ("define_plan" / "slow: plan rewrite" / "todo agent: plan
/// rewrite") so the operator can see which agent reshaped it.
///
/// Emits one top-line summary plus, when the prior plan existed,
/// per-step lines for steps that were added, removed, or whose
/// title changed. For a fresh plan (no prior) falls back to the
/// same "title per step" dump the session used before this helper
/// existed.
pub(crate) fn log_plan_change(
    source: &str,
    prior: Option<&kres_core::Plan>,
    new: &kres_core::Plan,
) {
    let prior_count = prior.map(|p| p.steps.len()).unwrap_or(0);
    kres_core::async_eprintln!(
        "[{source}] {} step(s){}",
        new.steps.len(),
        match prior {
            Some(_) => format!(" (was {prior_count})"),
            None => String::new(),
        }
    );
    let Some(prior) = prior else {
        // No prior → list every step inline so the operator sees
        // the initial decomposition without needing /plan.
        for s in &new.steps {
            kres_core::async_eprintln!("  [{}] {}", s.id, truncate(&s.title, 100));
        }
        return;
    };
    let prior_by_id: std::collections::BTreeMap<&str, &kres_core::PlanStep> =
        prior.steps.iter().map(|s| (s.id.as_str(), s)).collect();
    let new_by_id: std::collections::BTreeMap<&str, &kres_core::PlanStep> =
        new.steps.iter().map(|s| (s.id.as_str(), s)).collect();
    // Added: in new but not in prior.
    for s in &new.steps {
        if !prior_by_id.contains_key(s.id.as_str()) {
            kres_core::async_eprintln!("  + [{}] {}", s.id, truncate(&s.title, 100));
        }
    }
    // Removed: in prior but not in new.
    for s in &prior.steps {
        if !new_by_id.contains_key(s.id.as_str()) {
            kres_core::async_eprintln!("  - [{}] {}", s.id, truncate(&s.title, 100));
        }
    }
    // Retitled: id preserved, title changed.
    for s in &new.steps {
        if let Some(old) = prior_by_id.get(s.id.as_str()) {
            if old.title != s.title {
                kres_core::async_eprintln!(
                    "  ~ [{}] {} → {}",
                    s.id,
                    truncate(&old.title, 60),
                    truncate(&s.title, 60)
                );
            }
        }
    }
    // Fully unchanged (same id, same title, possibly status drift
    // which we report separately in sync_plan_from_todo). Counted
    // silently — listing them would bury the signal.
}

/// Log plan-step status transitions caused by `sync_plan_from_todo`.
/// `prior` + `after` come from two plan_snapshot calls bracketing
/// the sync. Emits one line per step whose status changed (e.g.
/// `[plan] s3 pending → done`).
pub(crate) fn log_plan_status_transitions(
    prior: Option<&kres_core::Plan>,
    after: Option<&kres_core::Plan>,
) {
    let (Some(prior), Some(after)) = (prior, after) else {
        return;
    };
    let prior_by_id: std::collections::BTreeMap<&str, kres_core::PlanStepStatus> = prior
        .steps
        .iter()
        .map(|s| (s.id.as_str(), s.status))
        .collect();
    for s in &after.steps {
        if let Some(prior_status) = prior_by_id.get(s.id.as_str()) {
            if *prior_status != s.status {
                kres_core::async_eprintln!(
                    "[plan] {} {} → {}",
                    s.id,
                    plan_status_label(*prior_status),
                    plan_status_label(s.status),
                );
            }
        }
    }
}

fn plan_status_label(s: kres_core::PlanStepStatus) -> &'static str {
    match s {
        kres_core::PlanStepStatus::Pending => "pending",
        kres_core::PlanStepStatus::InProgress => "in-progress",
        kres_core::PlanStepStatus::Done => "done",
        kres_core::PlanStepStatus::Skipped => "skipped",
    }
}

/// Sorted signature tuple per finding — used to detect merge
/// quiescence (§16). Matches
///id, status, summary, reproducer_sketch,
/// plus the LENGTHS of relevant_symbols and relevant_file_sections so
/// that added evidence registers as a change but order-only edits
/// don't.
pub(crate) fn findings_signature(
    findings: &[kres_core::Finding],
) -> Vec<(String, String, String, String, usize, usize)> {
    let mut out: Vec<_> = findings
        .iter()
        .map(|f| {
            (
                f.id.clone(),
                match f.status {
                    kres_core::findings::Status::Active => "active".to_string(),
                    kres_core::findings::Status::Invalidated => "invalidated".to_string(),
                },
                f.summary.clone(),
                f.reproducer_sketch.clone(),
                f.relevant_symbols.len(),
                f.relevant_file_sections.len(),
            )
        })
        .collect();
    out.sort();
    out
}

/// §44: expand every `/load <path>` occurrence in `text` with the
/// contents of `<path>`, wrapped in
/// `\n--- <path> ---\n<content>\n--- end <path> ---\n`. Matches
///On read failure the `/load …` literal survives
/// in the prompt and the error prints to stderr.
pub fn expand_inline_load(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let marker = b"/load ";
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(marker) {
            // Scan to next whitespace for the path token.
            let start = i + marker.len();
            let mut end = start;
            while end < bytes.len() && !(bytes[end] as char).is_whitespace() {
                end += 1;
            }
            let path = &text[start..end];
            if !path.is_empty() {
                match std::fs::read_to_string(path) {
                    Ok(body) => {
                        out.push('\n');
                        out.push_str(&format!("--- {path} ---\n"));
                        out.push_str(&body);
                        if !body.ends_with('\n') {
                            out.push('\n');
                        }
                        out.push_str(&format!("--- end {path} ---\n"));
                        i = end;
                        continue;
                    }
                    Err(e) => {
                        kres_core::async_eprintln!("/load {path}: {e}");
                        // Fall through: leave the `/load PATH`
                        // literal in place so the operator can see
                        // what didn't expand.
                    }
                }
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Human token counts: `12345` → `12.3k`. Matches
/// helper at.
fn fmt_k(n: u64) -> String {
    if n < 1_000 {
        return n.to_string();
    }
    if n < 1_000_000 {
        return format!("{:.1}k", n as f64 / 1_000.0);
    }
    format!("{:.2}M", n as f64 / 1_000_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn session_without_orchestrator_drops_prompt() {
        let mgr = TaskManager::new();
        let s = Session::new(mgr, ReplConfig::default()).await;
        // We can't easily exercise submit_prompt from a unit test
        // without stdin plumbing, but we can assert construction
        // leaves `orchestrator` unset.
        assert!(s.orchestrator.is_none());
    }

    #[test]
    fn truncate_preserves_short() {
        assert_eq!(truncate("abc", 5), "abc");
    }

    #[test]
    fn token_is_message_flag_accepts_all_spellings() {
        // -m / --message in the forms git accepts must trip the gate.
        for t in [
            "-m",
            "--message",
            "--message=foo",
            "-mfoo",
            "-m\"hello world\"",
        ] {
            assert!(token_is_message_flag(t), "should match -m form: {t:?}");
        }
    }

    #[test]
    fn token_is_message_flag_passes_through_lookalikes() {
        // Other -m*/--m* flags must NOT be classified as -m. The
        // false-positive set used to include `--metadata` etc when
        // we were doing a substring scan over the full command.
        for t in [
            "-M",          // git's "detect renames" flag
            "--metadata",  // hypothetical / future
            "--mailto",    // git format-patch
            "--minimal",   // git diff
            "--max-count", // git log
            "-F",
            "-s",
            "--amend",
            "commit",
        ] {
            assert!(!token_is_message_flag(t), "should not match -m form: {t:?}");
        }
    }

    #[test]
    fn find_overlong_commit_line_skips_trailers_and_indented_code() {
        // 120-char trailer is allowed; 90-char prose line trips at
        // cap=80; an indented quoted-code line is exempt.
        let msg = concat!(
            "subject line\n",
            "\n",
            "Short prose paragraph.\n",
            "Some prose line that is moderately long but under cap.\n",
            "    indented quoted code that is intentionally much longer than the cap should be skipped\n",
            "\n",
            "Fixes: 0123456789abcdef0123456789abcdef01234567 (\"a very long subject line that exceeds the cap easily\")\n",
            "Signed-off-by: Name <email@example.com>\n"
        );
        assert!(find_overlong_commit_line(msg, 80).is_none());

        let bad = concat!(
            "subject\n",
            "\n",
            "This is a single very long prose line that should trip the cap because it has way more characters than the limit allows.\n"
        );
        let (idx, line) = find_overlong_commit_line(bad, 80).expect("should detect");
        assert_eq!(idx, 2);
        assert!(line.contains("very long prose"));
    }

    #[test]
    fn find_overlong_commit_line_does_not_exempt_prose_with_colon() {
        // Body prose like "Note: ..." or "Example: ..." starts with
        // an uppercase word + colon but is NOT a kernel trailer.
        // is_trailer_line used to accept any Word: prefix and let
        // long prose through; the cap must still trip.
        let bad = concat!(
            "subject\n",
            "\n",
            "Note: this is a long-ish prose line that exceeds the configured cap and must trip the check.\n"
        );
        let (idx, _) = find_overlong_commit_line(bad, 80).expect("should detect");
        assert_eq!(idx, 2);
    }

    #[test]
    fn applied_edits_trailer_reports_failures() {
        let edits = vec![
            AppliedEdit {
                file_path: "a.c".into(),
                result: Ok(
                    "[edit /tmp/a.c] 1 replacement(s) (before: 100c, after: 98c)\n  ctx1\n  ctx2\n".into(),
                ),
            },
            AppliedEdit {
                file_path: "b.c".into(),
                result: Err(
                    "edit: old_string not found in /tmp/b.c — re-read the file and supply bytes copied verbatim from the current contents".into(),
                ),
            },
        ];
        let t = format_applied_edits_trailer(&edits);
        assert!(t.contains("Edits applied (1/2, 1 FAILED):"), "got {t}");
        assert!(t.contains("- a.c: 1 replacement(s)"), "got {t}");
        assert!(t.contains("[FAILED] b.c"), "got {t}");
        assert!(t.contains("old_string not found"), "got {t}");
        // Success entry should keep first preview line only, not the
        // multi-line context block.
        assert!(!t.contains("ctx2"), "preview context leaked: {t}");
    }

    #[test]
    fn applied_edits_trailer_empty_on_no_edits() {
        assert_eq!(format_applied_edits_trailer(&[]), "");
    }

    #[test]
    fn applied_edits_trailer_all_success_no_failed_marker() {
        let edits = vec![AppliedEdit {
            file_path: "a.c".into(),
            result: Ok("[edit /tmp/a.c] 2 replacement(s) (...)\n".into()),
        }];
        let t = format_applied_edits_trailer(&edits);
        assert!(t.contains("Edits applied (1/1):"), "got {t}");
        assert!(!t.contains("FAILED"), "got {t}");
    }

    #[test]
    fn truncate_ellipsises_long() {
        assert_eq!(truncate("abcdef", 3), "abc…");
    }

    fn code_output_tmp_dir(nonce: &str) -> std::path::PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "kres-code-output-{}-{}-{:x}",
            nonce,
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[tokio::test]
    async fn code_output_relative_lands_under_workspace() {
        let ws = code_output_tmp_dir("rel");
        let files = vec![kres_core::CodeFile {
            path: "summary.md".into(),
            content: "hello".into(),
            purpose: String::new(),
        }];
        persist_code_output(&ws, "task1", &files).await;
        let written = std::fs::read_to_string(ws.join("summary.md")).unwrap();
        assert_eq!(written, "hello");
        std::fs::remove_dir_all(&ws).ok();
    }

    #[tokio::test]
    async fn code_output_absolute_outside_workspace_without_consent_is_rejected() {
        // Fresh consent store with NO grants.
        let _ = kres_core::consent::install(Arc::new(kres_core::ConsentStore::new()));
        if let Some(s) = kres_core::consent::get() {
            s.clear();
        }
        let ws = code_output_tmp_dir("abs-rejected-ws");
        let outside = code_output_tmp_dir("abs-rejected-out");
        let target = outside.join("summary.md");
        let files = vec![kres_core::CodeFile {
            path: target.display().to_string(),
            content: "nope".into(),
            purpose: String::new(),
        }];
        persist_code_output(&ws, "task1", &files).await;
        assert!(
            !target.exists(),
            "consent gate should have blocked the absolute write"
        );
        std::fs::remove_dir_all(&ws).ok();
        std::fs::remove_dir_all(&outside).ok();
    }

    #[tokio::test]
    async fn code_output_absolute_with_consent_writes_through() {
        let _ = kres_core::consent::install(Arc::new(kres_core::ConsentStore::new()));
        let store = kres_core::consent::get().expect("consent installed");
        store.clear();
        let ws = code_output_tmp_dir("abs-allowed-ws");
        let bug_dir = code_output_tmp_dir("abs-allowed-bug");
        // Operator-mention equivalent: grant the bug dir.
        store
            .grant_from_mention(&bug_dir)
            .expect("grant existing dir");
        let target = bug_dir.join("summary.md");
        let files = vec![kres_core::CodeFile {
            path: target.display().to_string(),
            content: "triage body".into(),
            purpose: "triage summary".into(),
        }];
        persist_code_output(&ws, "task1", &files).await;
        let written = std::fs::read_to_string(&target).expect("file written");
        assert_eq!(written, "triage body");
        // Make sure we did NOT also write a copy under the workspace.
        let basename = target.file_name().unwrap();
        assert!(!ws.join(basename).exists());
        store.clear();
        std::fs::remove_dir_all(&ws).ok();
        std::fs::remove_dir_all(&bug_dir).ok();
    }

    #[tokio::test]
    async fn code_output_parentdir_traversal_is_rejected() {
        let ws = code_output_tmp_dir("parent");
        let files = vec![kres_core::CodeFile {
            path: "../escape.md".into(),
            content: "no".into(),
            purpose: String::new(),
        }];
        persist_code_output(&ws, "task1", &files).await;
        let parent = ws.parent().unwrap();
        assert!(
            !parent.join("escape.md").exists(),
            ".. traversal must be blocked"
        );
        std::fs::remove_dir_all(&ws).ok();
    }
}
