//! REPL session loop.

#[cfg(test)]
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::{stream, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use kres_agents::{AgentConfig, AgentKind, AgentRunner, DataFetcher, Followup, RunContext};
use kres_core::log::TurnLogger;
use kres_core::{format_usage_summary, FindingsStore, TaskManager, TaskState, UsageTracker};
use kres_llm::RateLimiter;

use crate::change_survey::{
    change_survey_chunk_prompt, change_survey_prompt, parse_inference_risks,
    split_diff_for_inference, split_source_for_inference, ChangeSurveyDiffChunk,
    ChangeSurveyReport, ChangeSurveySourceChunk,
};
use crate::commands::{parse_command, Command};

#[derive(Debug, Clone)]
pub struct ReplConfig {
    pub stop_grace: Duration,
    /// Lines emitted after the REPL output sink is installed. Startup
    /// context must go through this path because TUI mode owns an
    /// ratatui scrollback.
    pub startup_lines: Vec<String>,
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
    /// The directory this run owns outright — the explicit `--results`
    /// when there is one, otherwise the defaulted
    /// `~/.kres/sessions/<ts>-<pid>/`. Unlike `results_dir` this is
    /// always set, because it answers a different question: not "did
    /// the operator ask for a persistent run" but "where may this run
    /// write state that no other run may touch".
    ///
    /// Conflating the two is how workflow snapshots ended up in a path
    /// shared by every concurrent process.
    pub session_dir: PathBuf,
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
    /// MCP registry used to wire workflow-local source fetchers.
    /// Defaults to `~/.kres/mcp.json` when unset.
    pub mcp_config: Option<PathBuf>,
    /// Path to `<results>/session.json`. When set, the reaper and
    /// drain paths persist a [`kres_core::SessionState`] snapshot
    /// here on every mutation so an interrupted session can be
    /// resumed by re-invoking kres with the same `--results DIR`.
    /// None disables persistence (no-op writes).
    pub persist_path: Option<PathBuf>,
    /// Reuse a matching `change-survey.json` checkpoint. This follows the
    /// same explicit-resume contract as `session.json`; fresh sessions still
    /// write checkpoints but never inherit one from an earlier run.
    pub resume_change_survey: bool,
    /// When true, exit the REPL once the work-stop condition fires
    /// (`--turns 0` goal-met / no-progress / no-goal-batch-finished),
    /// instead of staying open waiting for further operator input.
    /// Defaulted to `!stdout.is_terminal()` from main.rs so a piped
    /// invocation (`kres ... > out.txt`) terminates after the
    /// turns stop, matching the existing `--turns N` exit path.
    pub exit_on_idle: bool,
    /// Exact value to use after `Assisted-by:` for fix-workflow
    /// commit messages. Defaults to `kres:<slow-model-id>`; the
    /// CLI can override it with `--assisted-by`.
    pub assisted_by: String,
}

impl Default for ReplConfig {
    fn default() -> Self {
        Self {
            stop_grace: Duration::from_secs(5),
            startup_lines: Vec::new(),
            findings_base: None,
            turns_limit: 0,
            follow_followups: false,
            report_path: None,
            results_dir: None,
            session_dir: std::env::temp_dir().join(format!("kres-session-{}", std::process::id())),
            template_path: None,
            stdio: false,
            tui: false,
            workspace: PathBuf::from("."),
            mcp_config: None,
            persist_path: None,
            resume_change_survey: false,
            exit_on_idle: false,
            assisted_by: "kres:claude-sonnet-5".to_string(),
        }
    }
}

/// How deep to rank when concurrency is unbounded. With a cap, the cap
/// IS the depth: ranking further ahead is output the next refresh
/// overwrites before anything reads it.
const UNBOUNDED_RANKING_DEPTH: usize = 10;

/// Who asked for work to continue. `/continue` carries operator
/// intent that a five-second timeout does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContinueSource {
    Operator,
    Idle,
}

/// What one call to the shared dispatch path did.
///
/// `refused` distinguishes "nothing was runnable" from "dispatch was
/// not allowed to run at all" — the operator needs to be told which,
/// and the reaper needs to know whether to expect a later retry.
#[derive(Debug, Default)]
struct DispatchOutcome {
    dispatched: usize,
    blocked: usize,
    remaining: usize,
    refused: Option<String>,
}

impl DispatchOutcome {
    fn refused(reason: &str) -> Self {
        Self::refused_owned(reason.to_string())
    }

    fn refused_owned(reason: String) -> Self {
        Self {
            refused: Some(reason),
            ..Self::default()
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
    agent_runner: Option<Arc<AgentRunner>>,
    consolidator: Option<Arc<kres_agents::ConsolidatorClient>>,
    workflow_classifier: Option<kres_agents::workflow_runner::AgentEnv>,
    todo_client: Option<Arc<kres_agents::TodoClient>>,
    goal_client: Option<Arc<kres_agents::GoalClient>>,
    /// Ranks the dispatchable todo rows before each wave. Runs on the
    /// slow coding agent — the model that has been reading the source
    /// — and is the only thing that decides execution order now that
    /// the todo agent stores the list unordered.
    prioritize_client: Option<Arc<kres_agents::PrioritizeClient>>,
    review_todo_client: Option<Arc<kres_agents::TodoClient>>,
    review_goal_client: Option<Arc<kres_agents::GoalClient>>,
    findings_store: Option<Arc<FindingsStore>>,
    usage: Arc<UsageTracker>,
    lenses: Arc<tokio::sync::RwLock<Vec<kres_core::LensSpec>>>,
    lens_consolidate_rules: Arc<tokio::sync::RwLock<Option<String>>>,
    initial_prompt: Option<String>,
    initial_prompt_mode: Option<kres_agents::TaskMode>,
    review_file_scan_target: Arc<tokio::sync::RwLock<Option<String>>>,
    /// Last reaped task's analysis — consumed by /reply.
    last_analysis: Arc<tokio::sync::Mutex<Option<String>>>,
    /// Findings loaded from disk at Session::new time. Applied to
    /// the TaskManager synchronously at the top of `run()` so the
    /// first submit_prompt observes a non-empty previous_findings.
    pending_bootstrap: Vec<kres_core::findings::Finding>,
    /// Per-session turn logger. Created lazily by `with_logger` or
    /// implicitly in `with_agent_runner` when the caller hasn't set
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
    /// Latched by the reaper once `--turns N` is reached. Dispatch
    /// consults it directly: with the full-drain barrier gone,
    /// "stop launching new work" can no longer be enforced by the
    /// idle loop alone, because the reaper now dispatches too.
    turns_cap_reached: Arc<std::sync::atomic::AtomicBool>,
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

/// Build a [`kres_core::SessionState`] from live manager state and
/// persist it atomically to `path`. No-op on write errors
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
    last_prompt: Option<String>,
    last_sig: Option<&std::sync::atomic::AtomicU64>,
) {
    use std::hash::{Hash, Hasher};
    let plan_before = mgr.plan_snapshot().await;
    // Sync and snapshot all manager-owned session state as one
    // generation so plan, todo, and the turn counter cannot disagree.
    let (plan_after, todo, deferred, completed_run_count) = mgr.sync_and_snapshot_runtime().await;
    log_plan_status_transitions(plan_before.as_ref(), plan_after.as_ref());
    let review_file_scan = mgr
        .get_cached_context(REVIEW_FILE_SCAN_CACHE_KEY)
        .await
        .and_then(|value| serde_json::from_value(value).ok())
        .filter(|state: &kres_core::ReviewFileScanState| {
            !state.target.trim().is_empty()
                && !state.source_hash.trim().is_empty()
                && !state.baseline.trim().is_empty()
                && !state.head.trim().is_empty()
                && !state.scan.trim().is_empty()
        });
    let state = kres_core::SessionState {
        version: 3,
        last_prompt,
        plan: plan_after,
        review_file_scan,
        todo,
        deferred,
        completed_run_count,
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
                agent_runner: None,
                consolidator: None,
                workflow_classifier: None,
                todo_client: None,
                goal_client: None,
                prioritize_client: None,
                review_todo_client: None,
                review_goal_client: None,
                findings_store,
                usage: Arc::new(UsageTracker::new()),
                lenses: Arc::new(tokio::sync::RwLock::new(Vec::new())),
                lens_consolidate_rules: Arc::new(tokio::sync::RwLock::new(None)),
                initial_prompt: None,
                initial_prompt_mode: None,
                review_file_scan_target: Arc::new(tokio::sync::RwLock::new(None)),
                last_analysis: Arc::new(tokio::sync::Mutex::new(None)),
                pending_bootstrap: findings,
                logger: None,
                task_goals: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
                task_prompts: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
                session_goal: Arc::new(tokio::sync::Mutex::new(None)),
                accumulated: Arc::new(tokio::sync::Mutex::new(Vec::new())),
                interrupted_prompt: Arc::new(tokio::sync::Mutex::new(None)),
                last_prompt: Arc::new(tokio::sync::Mutex::new(None)),
                persist_sig: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                stop_latched: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                turns_cap_reached: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                stop_notify: Arc::new(tokio::sync::Notify::new()),
                status_paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                input_ack_tx: tokio::sync::Mutex::new(None),
                mcp_shutdown: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            };
        }
        Self {
            mgr,
            cfg,
            agent_runner: None,
            consolidator: None,
            workflow_classifier: None,
            todo_client: None,
            goal_client: None,
            prioritize_client: None,
            review_todo_client: None,
            review_goal_client: None,
            findings_store,
            usage: Arc::new(UsageTracker::new()),
            lenses: Arc::new(tokio::sync::RwLock::new(Vec::new())),
            lens_consolidate_rules: Arc::new(tokio::sync::RwLock::new(None)),
            initial_prompt: None,
            initial_prompt_mode: None,
            review_file_scan_target: Arc::new(tokio::sync::RwLock::new(None)),
            last_analysis: Arc::new(tokio::sync::Mutex::new(None)),
            pending_bootstrap: Vec::new(),
            logger: None,
            task_goals: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            task_prompts: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            session_goal: Arc::new(tokio::sync::Mutex::new(None)),
            accumulated: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            interrupted_prompt: Arc::new(tokio::sync::Mutex::new(None)),
            last_prompt: Arc::new(tokio::sync::Mutex::new(None)),
            persist_sig: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            stop_latched: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            turns_cap_reached: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
    /// AgentRunner builder can splice it into AgentRunner.logger.
    pub fn logger(&self) -> Option<Arc<TurnLogger>> {
        self.logger.clone()
    }

    pub fn with_consolidator(mut self, c: Arc<kres_agents::ConsolidatorClient>) -> Self {
        self.consolidator = Some(c);
        self
    }

    pub fn with_workflow_classifier(mut self, env: kres_agents::workflow_runner::AgentEnv) -> Self {
        self.workflow_classifier = Some(env);
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

    /// Attach the authoritative review planner. These structured
    /// goal and todo clients share the primary slow model, while
    /// non-review workflows keep their existing clients.
    pub fn with_review_planner(
        mut self,
        goal: Arc<kres_agents::GoalClient>,
        todo: Arc<kres_agents::TodoClient>,
    ) -> Self {
        self.review_goal_client = Some(goal);
        self.review_todo_client = Some(todo);
        self
    }

    /// Snapshot of the accumulated findings ledger. Used by `/report`,
    /// `/summary`, and the end-of-session write path.
    pub async fn accumulated_snapshot(&self) -> Vec<AccumulatedEntry> {
        self.accumulated.lock().await.clone()
    }

    /// Snapshot of the deferred todos (`/followup`).
    pub async fn deferred_snapshot(&self) -> Vec<kres_core::TodoItem> {
        self.mgr.deferred_snapshot().await
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
        persist_session_state_to(path, &self.mgr, last_prompt, Some(&self.persist_sig)).await;
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
        self.mgr
            .load_runtime_state(
                state.todo.clone(),
                state.deferred.clone(),
                state.plan.clone(),
                state.completed_run_count,
            )
            .await;
        self.mgr
            .remove_cached_context(REVIEW_FILE_SCAN_CACHE_KEY)
            .await;
        *self.review_file_scan_target.write().await = None;
        if let Some(scan) = state.review_file_scan.as_ref() {
            *self.review_file_scan_target.write().await = Some(scan.target.clone());
            match review_file_scan_matches_current_window(&self.cfg.workspace, scan).await {
                Ok(true) => {
                    self.mgr
                        .cache_context(
                            REVIEW_FILE_SCAN_CACHE_KEY,
                            serde_json::to_value(scan).expect("review scan state serializes"),
                        )
                        .await;
                }
                Ok(false) => kres_core::async_eprintln!(
                    "/resume: discarded stale whole-file risk scan for {}",
                    scan.target
                ),
                Err(error) => kres_core::async_eprintln!(
                    "/resume: could not validate whole-file risk scan for {}: {error}",
                    scan.target
                ),
            }
        }
        // Pull deferred items back into the active todo list as
        // Pending so auto-continue can dispatch them immediately
        // after resume. Without this, the deferred list is
        // invisible to should_auto_continue (which checks the todo
        // list only), and --one / exit_on_idle sessions exit before
        // deferred work is ever dispatched.
        let (carry, added) = self.mgr.restore_deferred().await;
        if carry > 0 {
            kres_core::async_eprintln!(
                "resume: pulled {carry} deferred item(s), added {added} to todo list"
            );
        }
        if let Some(p) = state.last_prompt.clone() {
            *self.last_prompt.lock().await = Some(p);
        }
        Ok(Some(state))
    }

    pub fn with_prompt_file(mut self, pf: kres_agents::PromptFile) -> Self {
        self.lenses = Arc::new(tokio::sync::RwLock::new(pf.lenses));
        self.lens_consolidate_rules = Arc::new(tokio::sync::RwLock::new(None));
        self.initial_prompt_mode = None;
        if !pf.prompt.is_empty() {
            self.initial_prompt = Some(pf.prompt);
        }
        self
    }

    pub fn with_review_prompt_config(mut self, cfg: crate::workflow::ReviewPromptConfig) -> Self {
        self.lenses = Arc::new(tokio::sync::RwLock::new(cfg.prompt_file.lenses));
        self.lens_consolidate_rules = Arc::new(tokio::sync::RwLock::new(cfg.consolidate_rules));
        self.review_file_scan_target = Arc::new(tokio::sync::RwLock::new(cfg.file_scan_target));
        if !cfg.prompt_file.prompt.is_empty() {
            self.initial_prompt = Some(cfg.prompt_file.prompt);
            self.initial_prompt_mode = Some(kres_agents::TaskMode::Audit);
        }
        self
    }

    async fn install_review_config_and_submit(&self, cfg: crate::workflow::ReviewPromptConfig) {
        let previous_lenses = self.lenses.read().await.clone();
        let previous_rules = self.lens_consolidate_rules.read().await.clone();
        let previous_target = self.review_file_scan_target.read().await.clone();
        let previous_scan = self
            .mgr
            .remove_cached_context(REVIEW_FILE_SCAN_CACHE_KEY)
            .await;
        *self.lenses.write().await = cfg.prompt_file.lenses;
        *self.lens_consolidate_rules.write().await = cfg.consolidate_rules;
        *self.review_file_scan_target.write().await = cfg.file_scan_target;
        if !cfg.prompt_file.prompt.trim().is_empty() {
            let submitted = self
                .submit_prompt_inner(
                    cfg.prompt_file.prompt,
                    true,
                    None,
                    None,
                    Some(kres_agents::TaskMode::Audit),
                )
                .await;
            if !submitted {
                *self.lenses.write().await = previous_lenses;
                *self.lens_consolidate_rules.write().await = previous_rules;
                *self.review_file_scan_target.write().await = previous_target;
                if let Some(scan) = previous_scan {
                    self.mgr
                        .cache_context(REVIEW_FILE_SCAN_CACHE_KEY, scan)
                        .await;
                }
                self.stop_latched.store(true, Ordering::Release);
                kres_core::async_eprintln!(
                    "/review: bootstrap failed; restored prior review configuration and paused continuation"
                );
            }
        }
    }

    pub fn usage_tracker(&self) -> Arc<UsageTracker> {
        self.usage.clone()
    }

    pub fn with_agent_runner(mut self, o: Arc<AgentRunner>) -> Self {
        // The prioritizer IS the slow agent: same client, model,
        // token budget and thinking config. Deriving it here rather
        // than from a separate config entry means there is no way to
        // point the two at different models by accident.
        //
        // `system` is filled in per call from the session's mode via
        // `AgentRunner::slow_system_for_mode`, not pinned here. The
        // system prompt is part of the Anthropic cache prefix, and a
        // review runs as `Audit`, which uses `slow_system` — pinning
        // `slow_coding_system` here guaranteed the prioritizer and
        // the lenses could never share a cached block.
        self.prioritize_client = Some(Arc::new(kres_agents::PrioritizeClient {
            client: o.slow_client.clone(),
            model: o.slow_model.clone(),
            system: None,
            max_tokens: o.slow_max_tokens,
            max_input_tokens: o.slow_max_input_tokens,
            thinking: o.slow_thinking,
            usage: o.usage.clone(),
        }));
        self.agent_runner = Some(o);
        self
    }

    /// Pick the ready rows for the next wave, best first.
    ///
    /// Returns the ids to claim. An empty return means "no ranking
    /// available" and the caller falls back to storage order — the
    /// prioritizer is an optimisation, and a flaky call must not stall
    /// dispatch.
    async fn rank_ready(&self, ready: &[kres_core::TodoItem], limit: usize) -> Vec<String> {
        let Some(pc) = self.prioritize_client.as_ref() else {
            return Vec::new();
        };
        if limit == 0 || ready.len() <= limit {
            // Nothing to choose between: either no slots, or every
            // ready row fits. Both make the ranking call pure cost.
            return ready.iter().take(limit).map(|i| i.id.clone()).collect();
        }
        // Redacted, because these bytes ARE the cache head the wave's
        // lens fan-out will read, and `prepare_lens_fanout` redacts.
        // Raw findings would differ by the per-task provenance fields
        // and turn a shared block into two writes of ~166KB.
        let findings = kres_core::findings_for_prompt_history(&self.mgr.findings_snapshot().await);
        let plan = self.mgr.plan_snapshot().await;
        // The OPERATOR's prompt, not the last thing dispatched.
        // `last_prompt` is overwritten by every pipeline submission
        // (submit_from_pipeline -> submit_prompt_inner), so reading it
        // here would tell the prioritizer the run is about whichever
        // todo happened to go last, and would also put a per-wave
        // value in the cached prefix — the 4692adc bug again.
        // `Plan.prompt` is documented as the operator's raw prompt and
        // is stable for as long as the plan is.
        let question = plan
            .as_ref()
            .map(|p| p.prompt.clone())
            .filter(|p| !p.is_empty())
            .or_else(|| self.initial_prompt.clone())
            .unwrap_or_default();
        // Match the system prompt the slow lenses of this session run
        // under. Same reason as the model: it is part of the cache
        // prefix, and a mismatch makes sharing impossible.
        let mode = plan.as_ref().map(|p| p.mode).unwrap_or_default();
        let pc = kres_agents::PrioritizeClient {
            system: self
                .agent_runner
                .as_ref()
                .and_then(|runner| runner.slow_system_for_mode(mode).cloned()),
            ..(**pc).clone()
        };
        let pc = &pc;
        // The lens path sends `synthesis_skills.common` — the payload
        // minus the task's grafted `skill_reads`. No gather has run for
        // the wave being dispatched, so no reads are grafted yet and
        // the common half is the whole payload. Verified on the
        // 2026-08-06 run: the lens `common_skills` and this value
        // hashed identically (2d2852514e77f45bc70c9e598d5dbaf3), each
        // with exactly one distinct value across the run.
        let skills = self
            .agent_runner
            .as_ref()
            .and_then(|runner| runner.skills.clone());
        kres_core::async_eprintln!(
            "[prioritize] ranking {} ready item(s) for {limit} slot(s)",
            ready.len()
        );
        match kres_agents::prioritize_pending_with_logger(
            pc,
            kres_agents::PrioritizeInputs {
                question: &question,
                ready,
                previous_findings: &findings,
                skills: skills.as_ref(),
                plan: plan.as_ref(),
                limit,
            },
            self.logger.clone(),
            Some(self.mgr.root_shutdown().clone()),
        )
        .await
        {
            Ok(ids) => ids,
            Err(e) => {
                kres_core::async_eprintln!("[prioritize] failed ({e}); using storage order");
                Vec::new()
            }
        }
    }

    /// Whether a ranking refresh is worth making.
    ///
    /// Ranking exists to decide what runs next. When nothing will run
    /// next it is a slow-agent round-trip — measured at 17.5s and
    /// 85,578 input tokens, growing with findings — spent on an answer
    /// no one reads. After `/stop` it also violates the rule that the
    /// operator's stop skips every inference-heavy reaper post-step.
    fn ranking_refresh_allowed(&self) -> bool {
        use std::sync::atomic::Ordering;
        !self.stop_latched.load(Ordering::Acquire)
            && !self.turns_cap_reached.load(Ordering::Acquire)
    }

    /// Refresh the stored ranked order, detached.
    ///
    /// Ranking is a property the todo list always has, refreshed
    /// whenever the list changes — not something a dispatch waits for.
    /// The call is a slow-agent round-trip (measured at 17.5s against
    /// 40 ready rows and 14 findings, and it grows with findings), so
    /// putting it on the dispatch path would idle a slot for its whole
    /// duration and destroy the batch amortisation that made one
    /// ranking authorise ten dispatches.
    ///
    /// Detaching is safe by construction: `claim_ranked_todos`
    /// re-validates status and dependencies under the write lock, so
    /// consuming a slightly stale order can only mean a suboptimal
    /// pick, never an invalid one.
    fn spawn_ranking_refresh(self: &Arc<Self>) {
        if self.prioritize_client.is_none() || !self.ranking_refresh_allowed() {
            return;
        }
        let session = Arc::clone(self);
        tokio::spawn(async move {
            // Rank for the slots that will actually exist, which is
            // the concurrency cap — the same N the old per-wave
            // dispatch budget used, so the prioritizer's "at most N
            // ids, best first" contract is unchanged.
            let limit = match session.mgr.max_parallel() {
                0 => UNBOUNDED_RANKING_DEPTH,
                cap => cap,
            };
            let ready = session
                .mgr
                .ready_pending_snapshot(session.cfg.turns_limit)
                .await;
            if ready.items.is_empty() {
                return;
            }
            let ranked = session.rank_ready(&ready.items, limit).await;
            if ranked.is_empty() {
                // Failed or skipped. Leave the previous order in
                // place: a stale preference beats no preference, and
                // either way dispatch degrades to storage order.
                return;
            }
            session.mgr.set_ranked_order(ranked).await;
        });
    }

    /// Run the REPL. Takes `Arc<Self>` because the reaper needs to
    /// reach back into the session: with the full-drain barrier gone,
    /// the reaper is what dispatches the next tasks once its batch is
    /// published, and what refreshes the ranked order.
    pub async fn run(self: &Arc<Self>) -> Result<()> {
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
        // enter ratatui's inline viewport / raw mode.
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

        for line in &self.cfg.startup_lines {
            kres_core::async_eprintln!("{line}");
        }

        let root = self.mgr.root_shutdown().clone();
        let mgr_for_ctrlc = self.mgr.clone();
        let persist_for_ctrlc = self.cfg.persist_path.clone();
        let last_prompt_for_ctrlc = self.last_prompt.clone();
        let persist_sig_for_ctrlc = self.persist_sig.clone();
        let usage_for_ctrlc = self.usage.clone();
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
                    persist_session_state_to(p, &mgr_for_ctrlc, lp, Some(&persist_sig_for_ctrlc))
                        .await;
                }
                root.cancel();
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {
                        kres_core::async_eprintln!("\n(second ctrl-c — aborting)");
                        crate::tui::emergency_restore_terminal();
                        crate::status::restore();
                        if let Some(out) = format_usage_summary(
                            &usage_for_ctrlc,
                            "final usage before exit",
                            Some("final usage before exit: no API usage recorded"),
                        ) {
                            eprintln!("{out}");
                        }
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
        let review_todo_client = self.review_todo_client.clone();
        let lenses_for_reaper = self.lenses.clone();
        let logger_for_reaper = self.logger.clone();
        let goal_client_for_reaper = self.goal_client.clone();
        let review_goal_client_for_reaper = self.review_goal_client.clone();
        let task_goals_for_reaper = self.task_goals.clone();
        let task_prompts_for_reaper = self.task_prompts.clone();
        let accumulated_for_reaper = self.accumulated.clone();
        let persist_path_for_reaper = self.cfg.persist_path.clone();
        let last_prompt_for_reaper = self.last_prompt.clone();
        let persist_sig_for_reaper = self.persist_sig.clone();
        let store_for_reaper = self.findings_store.clone();
        let promoter_for_reaper = self.consolidator.clone();
        let usage_for_reaper = self.usage.clone();
        let interrupted_for_reaper = self.interrupted_prompt.clone();
        let report_path_for_reaper = self.cfg.report_path.clone();
        let turns_cap_reached = self.turns_cap_reached.clone();
        let turns_cap_reached_for_reaper = turns_cap_reached.clone();
        // Destination for coding-mode file output. Coding tasks emit
        // path-relative files; they land under the workspace (i.e.
        // the operator's cwd at kres-start time, or --workspace) so
        // "write hello-world.c" does what it says on the tin
        // instead of burying the file in
        // ~/.kres/sessions/<ts>/code/hello-world.c.
        let code_output_root_for_reaper: PathBuf = self.cfg.workspace.clone();
        let stop_latched_for_reaper = self.stop_latched.clone();
        // Post-reap refill signal, carrying nothing. Dispatch is no
        // longer excluded during a reap, so there is no lock to hand
        // over — this only says "a batch just published, the start
        // budget is re-armed, come and take the slots it vacated".
        // Depth 1 — a second batch finishing while a refill is queued
        // needs no second refill, since the queued one will see both.
        let (refill_tx, mut refill_rx) = tokio::sync::mpsc::channel::<()>(1);
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
        let mut turns_limit_announced = false;
        let mut turns_limit_waiting_active: Option<usize> = None;
        let reaper_handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(250));
            macro_rules! persist_reaper_tick {
                () => {
                    if let Some(ref p) = persist_path_for_reaper {
                        let lp = last_prompt_for_reaper.lock().await.clone();
                        persist_session_state_to(
                            p,
                            &mgr_for_reaper,
                            lp,
                            Some(&persist_sig_for_reaper),
                        )
                        .await;
                    }
                };
            }
            loop {
                tokio::select! {
                    _ = reaper_shutdown.cancelled() => break,
                    _ = ticker.tick() => {}
                }
                let reaped = mgr_for_reaper.reap().await;
                let reaped_count = reaped.len();
                // Per-task publication happens in the loop below; the
                // todo and goal agents then run ONCE over this.
                let mut batch: Vec<ReapedBatchEntry> = Vec::with_capacity(reaped_count);
                // Set by the consecutive-error watchdog when it decides
                // to halt the session. The batch pass below is skipped
                // in that case: root_shutdown is already cancelled, so
                // its todo and goal calls would abort mid-flight and
                // the batch's edits would be lost silently.
                let mut halted = false;
                for r in reaped {
                    report_reaped(&r);
                    // §22: a task that reaches a terminal state
                    // (Done or Errored) is no longer interruptable
                    // — clear the stash so /continue doesn't
                    // re-submit a completed prompt.
                    if matches!(r.state, TaskState::Done | TaskState::Errored) {
                        *interrupted_for_reaper.lock().await = None;
                    }
                    // Non-coding completion has no deferred workspace side
                    // effects. Publish it before any continuation inference.
                    // Coding completion is checkpointed below only after its
                    // file/edit/git actions have run.
                    if matches!(r.state, TaskState::Done)
                        && !matches!(r.mode, kres_core::TaskMode::Coding)
                    {
                        if let Some(todo_id) = r.todo_name.as_deref() {
                            mgr_for_reaper.mark_todo_done(todo_id).await;
                        }
                    }
                    if !matches!(r.mode, kres_core::TaskMode::Coding) {
                        persist_reaper_tick!();
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
                        // All deterministic coding side effects have now
                        // been attempted. Only now may the resumable state
                        // claim that this executor completed.
                        if matches!(r.state, TaskState::Done) {
                            if let Some(todo_id) = r.todo_name.as_deref() {
                                mgr_for_reaper.mark_todo_done(todo_id).await;
                            }
                        }
                        persist_reaper_tick!();
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
                                // Include the complete file. Goal checks are
                                // inference calls too; replacing an artifact
                                // with an excerpt can change their decision.
                                s.push_str("```\n");
                                s.push_str(&f.content);
                                if !f.content.ends_with('\n') {
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
                    let mut unrepaired_promotion_note: Option<String> = None;
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
                            // downstream: filter_promoted_delta sees
                            // the full `all_known`, preserves same-id
                            // invalidations/reactivations, and RENAMES
                            // colliding new active ids rather than
                            // dropping them.
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
                                kres_agents::promote::PromoteInputs {
                                    task_brief: &r.name,
                                    analysis: &effective_analysis,
                                    prose_relevant_existing: &prose_relevant,
                                    dedup_against: &all_known_for_dedup,
                                    cancel: Some(stop_notify_for_reaper.clone()),
                                    usage: Some(usage_for_reaper.clone()),
                                    thinking: promoter.thinking,
                                },
                                logger_for_reaper.clone(),
                            )
                            .await
                            {
                                Ok(outcome) => {
                                    if !outcome.findings.is_empty() {
                                        kres_core::async_eprintln!(
                                            "[promote] {} prose-only bug(s) promoted to findings",
                                            outcome.findings.len()
                                        );
                                        promoted_ids
                                            .extend(outcome.findings.iter().map(|f| f.id.clone()));
                                        working_delta.extend(outcome.findings);
                                    }
                                    if !outcome.unrepaired.is_empty() {
                                        unrepaired_promotion_note = Some(
                                            kres_agents::finding_repair::format_unrepaired_findings(
                                                &outcome.unrepaired,
                                            ),
                                        );
                                    }
                                }
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
                    if let Some(note) = unrepaired_promotion_note.as_deref() {
                        if let Some(ref s) = store_for_reaper {
                            if let Err(e) = s.append_task_prose(&stamp, note).await {
                                kres_core::async_eprintln!("finding repair task_prose append: {e}");
                            }
                        }
                        if let Some(ref rp) = report_path_for_reaper {
                            if let Err(e) = crate::report::append_task_section(rp, &r.name, note) {
                                tracing::warn!(
                                    target: "kres_repl",
                                    "report finding-repair append to {}: {e}",
                                    rp.display()
                                );
                            }
                        }
                    }
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
                            let mgr_c = mgr_for_reaper.clone();
                            let stamp_c = stamp.clone();
                            let prose_c = prose_for_details.clone();
                            let report = mgr_for_reaper
                                .with_findings_extract_lock(|| async move {
                                    let report = s_c
                                        .apply_delta(&delta, Some(&stamp_c), Some(&prose_c))
                                        .await?;
                                    mgr_c.replace_findings(report.merged.clone()).await;
                                    Ok::<_, kres_core::findings::FindingsError>(report)
                                })
                                .await;
                            match report {
                                Ok(rep) => {
                                    apply_changed = rep.changed;
                                    apply_added = rep.added;
                                    apply_updated = rep.updated;
                                    apply_invalidated = rep.invalidated;
                                    apply_reactivated = rep.reactivated;
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
                            let counts = mgr_for_reaper
                                .apply_findings_delta(
                                    &delta,
                                    Some(&stamp),
                                    Some(&prose_for_details),
                                )
                                .await;
                            apply_changed = counts.changed;
                            apply_added = counts.added;
                            apply_updated = counts.updated;
                            apply_invalidated = counts.invalidated;
                            apply_reactivated = counts.reactivated;
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
                    // §turns0: a task counts as progress if it grew
                    // its mode's primary output. Audit/generic grow
                    // the merged findings list; coding grows
                    // code_output / code_edits. Anything else — empty
                    // delta, apply_delta folded into existing findings,
                    // a coding turn that produced neither file nor
                    // edit — ticks the streak.
                    if !r.analysis.is_empty() {
                        let grew = if r.mode.produces_findings() {
                            final_list.len() > pre_size
                        } else {
                            !r.code_output.is_empty() || !r.code_edits.is_empty()
                        };
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
                    let mut followups_for_todo: Vec<_> =
                        if matches!(r.mode, kres_core::TaskMode::Coding) {
                            r.followups
                                .iter()
                                .filter(|f| reaper_followup_kind(f).is_none())
                                .cloned()
                                .collect()
                        } else {
                            r.followups.clone()
                        };
                    if unrepaired_promotion_note.is_some() {
                        followups_for_todo.push(serde_json::json!({
                            "type": "question",
                            "name": "Repair malformed Finding records from the completed task",
                            "reason": "[MISSING] Rust preserved Finding objects that remained schema-invalid after one repair attempt; re-emit the same claims with valid typed fields"
                        }));
                    }
                    // If this task pushed us to/past --turns N, stop
                    // before continuation LLMs. Findings/report/state
                    // above are already published for the completed
                    // task; todo-agent and goal-agent calls below are
                    // only about deciding what to run next. Running
                    // them after the cap can hang or create more
                    // pending work before the cap check at the bottom
                    // of the reaper tick gets a chance to drain.
                    if turns_limit > 0 && mgr_for_reaper.completed_run_count().await >= turns_limit
                    {
                        turns_cap_reached_for_reaper.store(true, Ordering::Release);
                        if !followups_for_todo.is_empty() {
                            add_followups_as_pending(
                                &mgr_for_reaper,
                                &followups_for_todo,
                                "turn-cap followup emitted by completed task",
                            )
                            .await;
                        }
                        continue;
                    }
                    // Defer the todo and goal agents to a single pass
                    // over the whole batch, below. Running them per
                    // task cost 46s + 7s of strictly serial time each,
                    // multiplied by however many tasks a wave reaped,
                    // and it made the todo agent reconcile siblings one
                    // at a time — so a followup two parallel tasks both
                    // emitted could only be deduped after the first had
                    // already become a row.
                    //
                    // Errored tasks reach this path with analysis="".
                    // Without surfacing the error the todo agent reads
                    // "no analysis" as "task didn't run, re-queue" and
                    // we spin (see consecutive_errors above). Inject
                    // the error so the agent has something concrete to
                    // react to (skip vs. retry).
                    let analysis_for_todo = if matches!(r.state, TaskState::Errored) {
                        format!(
                            "[task errored: {}]",
                            r.error.as_deref().unwrap_or("(no error text)")
                        )
                    } else {
                        effective_analysis.clone()
                    };
                    batch.push(ReapedBatchEntry {
                        task_id: r.id,
                        task_name: r.name.clone(),
                        todo_id: if matches!(r.state, TaskState::Done) {
                            r.todo_name.clone()
                        } else {
                            None
                        },
                        analysis: analysis_for_todo,
                        followups: followups_for_todo,
                        mode: r.mode,
                        lensed_review: matches!(r.mode, kres_core::TaskMode::Audit)
                            && !lenses_for_reaper.read().await.is_empty(),
                    });
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
                        let carry = mgr_for_reaper.defer_pending().await;
                        if carry > 0 {
                            kres_core::async_eprintln!(
                                "[{carry} pending item(s) moved to /followup]"
                            );
                        }
                        if exit_on_idle {
                            // Preserve what the batch would have
                            // reconciled. The todo agent cannot run
                            // after the cancel, so record the
                            // followups deterministically instead of
                            // dropping them: `--resume` picks them up.
                            let carried: Vec<serde_json::Value> = batch
                                .iter()
                                .flat_map(|e| e.followups.iter().cloned())
                                .collect();
                            if !carried.is_empty() {
                                add_followups_as_pending(
                                    &mgr_for_reaper,
                                    &carried,
                                    "followup emitted by a task reaped in the halting batch",
                                )
                                .await;
                            }
                            halted = true;
                            persist_reaper_tick!();
                            mgr_for_reaper.root_shutdown().cancel();
                            break;
                        }
                    }
                }
                // --- one todo/goal pass for the whole batch ---------
                //
                // Grouped by which client pair the task belongs to. A
                // lensed review uses the review todo/goal clients and
                // everything else uses the general ones; a mixed batch
                // therefore costs two rounds, never N.
                if !batch.is_empty() && !halted {
                    for lensed in [false, true] {
                        let group: Vec<&ReapedBatchEntry> =
                            batch.iter().filter(|e| e.lensed_review == lensed).collect();
                        if group.is_empty() {
                            continue;
                        }
                        let group_todo_client = if lensed {
                            review_todo_client.as_ref()
                        } else {
                            todo_client.as_ref()
                        };
                        let group_goal_client = if lensed {
                            review_goal_client_for_reaper.clone()
                        } else {
                            goal_client_for_reaper.clone()
                        };
                        let group_followups: Vec<serde_json::Value> = group
                            .iter()
                            .flat_map(|e| e.followups.iter().cloned())
                            .collect();
                        // Update todo list via todo-agent when one is
                        // configured. Non-fatal on any failure — the
                        // todo list is maintained best-effort.
                        if let Some(tc) = group_todo_client {
                            let current = mgr_for_reaper.todo_snapshot().await;
                            let completed: Vec<kres_agents::CompletedTask<'_>> = group
                                .iter()
                                .map(|e| kres_agents::CompletedTask {
                                    query: &e.task_name,
                                    todo_id: e.todo_id.as_deref(),
                                    analysis: &e.analysis,
                                    followups: &e.followups,
                                })
                                .collect();
                            kres_core::async_eprintln!(
                                "[todo update] before: {} item(s) ({} pending, {} done); {} completed task(s), {} new followup(s)",
                                current.len(),
                                current
                                    .iter()
                                    .filter(|t| t.status == kres_core::TodoStatus::Pending)
                                    .count(),
                                current
                                    .iter()
                                    .filter(|t| t.status == kres_core::TodoStatus::Done)
                                    .count(),
                                completed.len(),
                                group_followups.len(),
                            );
                            let plan_for_todo = mgr_for_reaper.plan_snapshot().await;
                            match kres_agents::update_todo_via_agent_with_logger(
                                tc,
                                kres_agents::TodoAgentInputs {
                                    completed: &completed,
                                    current_todo: &current,
                                    plan: plan_for_todo.as_ref(),
                                },
                                logger_for_reaper.clone(),
                                Some(mgr_for_reaper.root_shutdown().clone()),
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
                                    if let Some(change) = mgr_for_reaper
                                        .merge_inferred_state(kres_core::task::InferredTodoUpdate {
                                            items: updated.todo,
                                            completed_todo_ids: group
                                                .iter()
                                                .filter_map(|e| e.todo_id.clone())
                                                .collect(),
                                            inference_snapshot: current,
                                            plan_rewrite: updated.plan,
                                        })
                                        .await
                                    {
                                        log_plan_change(
                                            "todo agent: plan rewrite",
                                            change.prior.as_ref(),
                                            &change.current,
                                        );
                                    }
                                    persist_reaper_tick!();
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        target: "kres_repl",
                                        "todo-agent update failed: {e}"
                                    );
                                }
                            }
                        }
                        if lensed && !group_followups.is_empty() {
                            ensure_review_followups_remain_pending(
                                &mgr_for_reaper,
                                &group_followups,
                            )
                            .await;
                        }

                        // §4 goal check: if a goal is set, ask the
                        // main agent whether the current analyses
                        // satisfy it. When met, every pending todo
                        // moves to the deferred list and running tasks
                        // get cancelled so the operator reclaims the
                        // prompt.
                        //
                        // For coding-mode tasks (fix flow), skip the
                        // goal check entirely. A "turn" in the fix flow
                        // is the full fix+compile+review cycle, not
                        // each individual sub-task (git add, git
                        // commit, make, etc.). Checking after each
                        // sub-step wastes tokens and risks a premature
                        // met=true. The todo list drives iteration;
                        // check_goal runs only when the list drains
                        // (no more pending items) or when an
                        // audit/generic task completes.
                        //
                        // The goal itself is per-task: submit_prompt
                        // parked one under each task id. Pipeline
                        // submissions inherit the cached session goal,
                        // so a batch almost always carries one distinct
                        // goal — but check each distinct (goal, prompt)
                        // pair rather than assuming that.
                        let mut checked: Vec<(String, String)> = Vec::new();
                        for entry in &group {
                            let per_task_goal =
                                task_goals_for_reaper.lock().await.remove(&entry.task_id);
                            let per_task_prompt = task_prompts_for_reaper
                                .lock()
                                .await
                                .remove(&entry.task_id)
                                .unwrap_or_default();
                            if matches!(entry.mode, kres_core::TaskMode::Coding) {
                                kres_core::async_eprintln!(
                                    "[goal check] skipped for coding-mode task — \
                                     goal evaluated when todo list drains"
                                );
                                continue;
                            }
                            let (Some(gc), Some(goal)) = (group_goal_client.clone(), per_task_goal)
                            else {
                                continue;
                            };
                            if checked
                                .iter()
                                .any(|(g, p)| g == &goal && p == &per_task_prompt)
                            {
                                continue;
                            }
                            checked.push((goal.clone(), per_task_prompt.clone()));
                            let outcome = run_batch_goal_check(BatchGoalCheck {
                                mgr: &mgr_for_reaper,
                                goal_client: &gc,
                                goal: &goal,
                                prompt: &per_task_prompt,
                                accumulated: &accumulated_for_reaper,
                                lensed_review: lensed,
                                followup_count: group_followups.len(),
                                follow_followups,
                                turns_limit,
                            })
                            .await;
                            let BatchGoalOutcome { missing } = outcome;
                            if missing.is_empty() {
                                continue;
                            }
                            // Spec in AGENTS.md: "Goal not met → only
                            // missing items become followups." Convert
                            // each to a 'question' followup and funnel
                            // it through the todo agent so it gets
                            // deduped against existing items.
                            let Some(tc) = group_todo_client else {
                                continue;
                            };
                            let reason_prefix = format!("goal not met: {}", missing.join("; "));
                            let missing_fus: Vec<serde_json::Value> = missing
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
                            kres_core::async_eprintln!(
                                "[goal-not-met → todo update] injecting {} missing item(s) as question followups",
                                missing_fus.len()
                            );
                            let plan_for_todo = mgr_for_reaper.plan_snapshot().await;
                            let completed = [kres_agents::CompletedTask {
                                query: &group[0].task_name,
                                todo_id: None,
                                analysis: "",
                                followups: &missing_fus,
                            }];
                            match kres_agents::update_todo_via_agent_with_logger(
                                tc,
                                kres_agents::TodoAgentInputs {
                                    completed: &completed,
                                    current_todo: &current,
                                    plan: plan_for_todo.as_ref(),
                                },
                                logger_for_reaper.clone(),
                                Some(mgr_for_reaper.root_shutdown().clone()),
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
                                    if let Some(change) = mgr_for_reaper
                                        .merge_inferred_state(kres_core::task::InferredTodoUpdate {
                                            items: updated.todo,
                                            completed_todo_ids: Vec::new(),
                                            inference_snapshot: current,
                                            plan_rewrite: updated.plan,
                                        })
                                        .await
                                    {
                                        log_plan_change(
                                            "todo agent: plan rewrite (goal-not-met)",
                                            change.prior.as_ref(),
                                            &change.current,
                                        );
                                    }
                                    persist_reaper_tick!();
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
                // The batch is published: findings applied, todo rows
                // completed, followups merged.
                if reaped_count > 0 {
                    // Re-arm the start budget. Dispatch may run during
                    // a reap, but only up to `max_parallel` starts
                    // between completions, so this is what lets the
                    // next wave through.
                    mgr_for_reaper.note_reap_completed().await;
                    // Then ask the REPL loop to refill the slots this
                    // batch vacated. The dispatch runs there rather
                    // than here because `submit_prompt_inner` reaches
                    // change-survey closures whose futures rustc
                    // cannot prove `Send`, and everything inside this
                    // `tokio::spawn` must be. The REPL loop is not
                    // spawned, so it has no such bound. The reaper
                    // still decides WHEN.
                    if let Err(e) = refill_tx.try_send(()) {
                        // Loop busy with an operator command, or a
                        // refill already queued. The idle poll picks
                        // the work up within its interval.
                        tracing::debug!(
                            target: "kres_repl",
                            "post-reap refill not queued ({e}); idle loop will dispatch"
                        );
                    }
                }
                // --turns N limit: once the slow-agent run count hits
                // the configured cap, stop launching new work and
                // defer not-yet-started followups. Do not reset or
                // cancel active tasks here: completed_run_count is
                // bumped as tasks finish, before the reaper has
                // necessarily merged every concurrently completed
                // result into findings.json/report.md. Active tasks
                // must be allowed to finish and be reaped, otherwise
                // their LLM output can exist only in code.jsonl.
                if turns_limit > 0 {
                    let done = mgr_for_reaper.completed_run_count().await;
                    if done >= turns_limit {
                        turns_cap_reached_for_reaper.store(true, Ordering::Release);
                        if !turns_limit_announced {
                            kres_core::async_eprintln!(
                                "\n=== --turns {turns_limit} reached — {done} task run(s) completed ==="
                            );
                            turns_limit_announced = true;
                        }
                        let carry = mgr_for_reaper.defer_pending().await;
                        if carry > 0 {
                            kres_core::async_eprintln!(
                                "[{carry} pending item(s) deferred — see /followup]"
                            );
                        }
                        // `active_count` excludes terminal tasks, but a
                        // sibling can become Done while this tick is busy
                        // publishing an earlier result. Require the entire
                        // manager task list to be empty so terminal results
                        // receive a subsequent reap/publication pass before
                        // exit.
                        let outstanding = mgr_for_reaper.snapshot().await.len();
                        match turns_cap_action(done, turns_limit, outstanding) {
                            TurnsCapAction::Continue => {}
                            TurnsCapAction::DrainAndWait => {
                                if turns_limit_waiting_active != Some(outstanding) {
                                    kres_core::async_eprintln!(
                                        "[--turns cap reached; waiting for {outstanding} active or terminal task(s) to finish and publish results]"
                                    );
                                    turns_limit_waiting_active = Some(outstanding);
                                }
                                persist_reaper_tick!();
                                continue;
                            }
                            TurnsCapAction::DrainAndExit => {
                                // No executor remains, so an InProgress todo
                                // cannot legitimately survive this snapshot.
                                // Successful reaped tasks were marked Done
                                // before the cap check above; leftovers are
                                // interrupted/orphaned work that must be
                                // resumable rather than permanently owned by
                                // a vanished task.
                                let (reset, carry) =
                                    reconcile_turn_cap_todos(&mgr_for_reaper).await;
                                if reset > 0 || carry > 0 {
                                    kres_core::async_eprintln!(
                                        "[--turns cap final reconciliation: reset {reset} orphaned in-progress item(s), deferred {carry} resumable item(s)]"
                                    );
                                }
                                kres_core::async_eprintln!("exiting REPL.");
                                persist_reaper_tick!();
                                mgr_for_reaper.root_shutdown().cancel();
                                break;
                            }
                        }
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
                            persist_reaper_tick!();
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
                        // Move any leftover pending/blocked items to
                        // /followup's deferred list. Done/Skipped
                        // items and executor-owned InProgress rows stay
                        // active. Unlike the --turns N path
                        // we do NOT cancel the root shutdown — the
                        // user wants to keep driving the REPL after
                        // goal met.
                        let carry = mgr_for_reaper.defer_pending().await;
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
                            persist_reaper_tick!();
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
                persist_reaper_tick!();
            }
        });

        // Install a session-scoped consent store so access outside
        // --workspace can be auto-granted by mention in the
        // operator's prompt (see consent::grant_paths_from_text in
        // submit_prompt).  install() returns Err when the slot was
        // already set; that's fine — subsequent Sessions in the
        // same process (rare; tests) will see the first one's
        // store, which is acceptable for the unit-test surface.
        let _ = kres_core::consent::install(Arc::new(kres_core::ConsentStore::new()));
        print_banner();
        let installed_lenses = self.lenses.read().await.clone();
        if !installed_lenses.is_empty() {
            kres_core::async_eprintln!(
                "installed {} session-wide slow-agent lens(es):",
                installed_lenses.len()
            );
            for l in &installed_lenses {
                kres_core::async_eprintln!("  [{}] {}", l.kind, l.name);
            }
        }
        if let Some(ref p) = self.initial_prompt {
            kres_core::async_eprintln!("submitting initial prompt from --prompt");
            let submitted = self
                .submit_prompt_inner(p.clone(), true, None, None, self.initial_prompt_mode)
                .await;
            if !submitted {
                // `--prompt` is a batch instruction. When it cannot start
                // there is no work, and a failed review bootstrap also latches
                // `stop_latched`, so auto-continue will never fire either.
                // Falling through to the interactive loop leaves the process
                // alive forever with nothing to do and no exit status — a
                // scripted run just blocks. Observed on 2026-08-05: a review
                // whose file survey failed validation sat idle for 25 minutes
                // until it was killed by hand.
                //
                // Every reaper exit path is gated on completed work, so none
                // of them can rescue this; report and fail here instead.
                kres_core::async_eprintln!(
                    "--prompt could not be started; exiting rather than idling with no work"
                );
                self.mgr.root_shutdown().cancel();
                // main() turns this Err into `std::process::exit(1)`, which
                // skips Drop. The TUI runs on a detached spawn_blocking task
                // with no guard, so raw mode has to come off here or the
                // parent shell loses Ctrl-C.
                self.restore_terminal_for_final_output();
                return Err(anyhow::anyhow!(
                    "initial --prompt submission failed; see the errors above"
                ));
            }
        }
        let root_shutdown = self.mgr.root_shutdown().clone();
        let turns_cap_reached_for_loop = turns_cap_reached.clone();
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
                signal = refill_rx.recv() => {
                    // A reap batch just published its results. Refill
                    // the slots it vacated, then re-rank for the next
                    // dispatch.
                    if signal.is_none() {
                        continue;
                    }
                    let outcome = self.dispatch_ready(None, "post-reap").await;
                    if let Some(reason) = outcome.refused {
                        kres_core::async_eprintln!("[dispatch post-reap] skipped: {reason}");
                    }
                    self.spawn_ranking_refresh();
                    auto_continue_idle_since = None;
                    continue;
                }
                l = rx.recv() => {
                    // Input arrived: reset idle clock.
                    auto_continue_idle_since = None;
                    match l {
                        Some(s) => s,
                        None => break,
                    }
                }
                _ = tokio::time::sleep(Duration::from_secs(1)) => {
                    if turns_cap_reached_for_loop.load(Ordering::Acquire) {
                        auto_continue_idle_since = None;
                    } else if self.should_auto_continue().await {
                        let since = auto_continue_idle_since.get_or_insert_with(std::time::Instant::now);
                        if since.elapsed() >= AUTO_CONTINUE_IDLE {
                            self.auto_continue().await;
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
                Command::Triage { target } => self.cmd_triage(target).await,
                Command::Validate { finding, workspace } => {
                    self.cmd_validate(finding, workspace).await
                }
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
                    // AgentRunner / task future awaiting cancellation
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
                    self.restore_terminal_for_final_output();
                    self.print_exit_cost_summary_direct();
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
        self.restore_terminal_for_final_output();
        self.print_exit_cost_summary_direct();

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
        client: Option<&Arc<kres_agents::GoalClient>>,
        text: &str,
        plan: Option<&kres_core::Plan>,
        label: &str,
    ) -> (Option<String>, kres_agents::TaskMode) {
        let Some(gc) = client else {
            return (None, kres_agents::TaskMode::default());
        };
        match kres_agents::define_goal(gc, text, plan, Some(self.mgr.root_shutdown().clone())).await
        {
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
        let _ = self.submit_prompt_inner(text, true, None, None, None).await;
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
    async fn submit_from_pipeline(
        &self,
        text: String,
        todo_tag: Option<String>,
        step_id: Option<String>,
    ) {
        let _ = self
            .submit_prompt_inner(text, false, todo_tag, step_id, None)
            .await;
    }

    async fn stash_interruptible_prompt(&self, text: &str, operator_submission: bool) {
        if operator_submission {
            *self.interrupted_prompt.lock().await = Some(text.to_string());
        }
    }

    async fn submit_prompt_inner(
        &self,
        text: String,
        include_recent_context: bool,
        todo_tag: Option<String>,
        step_id: Option<String>,
        forced_mode: Option<kres_agents::TaskMode>,
    ) -> bool {
        let Some(orc) = self.agent_runner.clone() else {
            kres_core::async_eprintln!("(no AgentRunner configured — prompt dropped)");
            kres_core::async_eprintln!(
                "hint: rerun `kres repl` with agent configs to enable prompt handling"
            );
            return false;
        };
        // Operator engaged — clear the /stop latch so auto-continue
        // works again after this task completes.
        self.stop_latched
            .store(false, std::sync::atomic::Ordering::Release);
        // Auto-grant access consent for any file or directory the
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
                        "consent: granted access to {} dir(s) named in the prompt: {}",
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

        // Extract an embedded plan from the prompt template, if present.
        // The stripped text is used downstream; agents never see the
        // PLAN: block. If the parse fails, embedded_steps is None and
        // the normal define_plan LLM call fires later.
        let (text, embedded_steps) = kres_core::extract_embedded_plan(&text);

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

        // §22: only operator submissions use the interruption stash.
        // Pipeline work is already represented by a todo carrying its
        // identity, dependency, and plan-step linkage; resubmitting its bare
        // prompt through the operator path would lose that state and derive a
        // new narrow session goal.
        self.stash_interruptible_prompt(&text, include_recent_context)
            .await;
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
        let mut existing_plan = self.mgr.plan_snapshot().await;
        let review_submission = forced_mode == Some(kres_agents::TaskMode::Audit)
            && !self.lenses.read().await.is_empty();
        let fresh_review_submission = include_recent_context
            && forced_mode == Some(kres_agents::TaskMode::Audit)
            && review_submission;
        // Persist the operator/workflow prompt without the generated risk
        // scan. Goal and plan inference below still receive the scan, but the
        // completed scan has its own session-state field and is injected once
        // into task questions. Keeping it in Plan::prompt as well caused every
        // fast/slow request to carry two identical copies.
        let persisted_plan_prompt = text.clone();
        if include_recent_context && !review_submission {
            self.mgr
                .remove_cached_context(REVIEW_FILE_SCAN_CACHE_KEY)
                .await;
        }
        let planning_text = if include_recent_context && review_submission {
            let target = self.review_file_scan_target.read().await.clone();
            if let Some(target) = target {
                let prior_scan = if fresh_review_submission {
                    None
                } else {
                    review_file_scan_context(&self.mgr, &self.cfg.workspace, &target).await
                };
                let scan = match prior_scan {
                    Some(scan) => Some(scan),
                    None => {
                        match run_review_file_scan(
                            &orc,
                            &self.cfg.workspace,
                            &target,
                            self.cfg
                                .persist_path
                                .as_ref()
                                .map(|path| path.with_file_name("change-survey.json")),
                            self.cfg.resume_change_survey,
                            self.mgr.root_shutdown(),
                        )
                        .await
                        {
                            Ok(scan) => {
                                cache_review_file_scan(&self.mgr, &scan).await;
                                Some(scan.scan)
                            }
                            Err(error) => {
                                kres_core::async_eprintln!(
                                    "review bootstrap scan failed for {target}: {error}"
                                );
                                return false;
                            }
                        }
                    }
                };
                match scan {
                    Some(scan) => format!(
                        "{persisted_plan_prompt}\n\n--- WHOLE-FILE RISK SCAN ---\n{scan}\n--- END WHOLE-FILE RISK SCAN ---\nUse this source-derived ranking when defining the completion goal and semantic coverage plan. Do not add a survey/scan step to the plan."
                    ),
                    None => persisted_plan_prompt.clone(),
                }
            } else {
                persisted_plan_prompt.clone()
            }
        } else {
            persisted_plan_prompt.clone()
        };
        if fresh_review_submission {
            self.mgr.clear_session_work().await;
            existing_plan = None;
        }
        let planning_goal_client = if review_submission {
            self.review_goal_client.as_ref()
        } else {
            self.goal_client.as_ref()
        };
        let (defined_goal, task_mode): (Option<String>, kres_agents::TaskMode) =
            if include_recent_context {
                // Operator-typed submission: derive a fresh goal and
                // cache it for downstream pipeline follow-ups.
                let r = self
                    .derive_goal(
                        planning_goal_client,
                        &planning_text,
                        existing_plan.as_ref(),
                        "fresh",
                    )
                    .await;
                let r = if let Some(mode) = forced_mode {
                    kres_core::async_eprintln!(
                        "goal mode forced by command: {} -> {}",
                        r.1.as_str(),
                        mode.as_str()
                    );
                    (r.0, mode)
                } else {
                    r
                };
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
                        .derive_goal(
                            planning_goal_client,
                            &planning_text,
                            existing_plan.as_ref(),
                            "fallback",
                        )
                        .await;
                    if let Some(g) = r.0.as_ref() {
                        *self.session_goal.lock().await = Some((g.clone(), r.1));
                    }
                    r
                }
            };
        // Step-level context injection: a plan step can carry a
        // `context` string (e.g. review lenses protocol) that gets
        // prepended to the derived task's prompt. This puts the
        // step's protocol directly in the question where the slow
        // agent can't miss it, without changing the pipeline mode
        // or system prompt.
        let step_context = if let Some(ref sid) = step_id {
            existing_plan
                .as_ref()
                .map(|p| p.step_context(sid))
                .unwrap_or("")
        } else {
            ""
        };
        let text = if step_context.is_empty() {
            persisted_plan_prompt.clone()
        } else {
            if let Some(ref sid) = step_id {
                kres_core::async_eprintln!(
                    "injecting step context from plan step {} ({}k chars)",
                    sid,
                    step_context.len() / 1000,
                );
            }
            format!("{step_context}\n\n---\n\n{persisted_plan_prompt}")
        };
        let planning_text = if step_context.is_empty() {
            planning_text
        } else {
            format!("{step_context}\n\n---\n\n{planning_text}")
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
            if let (Some(gc), Some(goal)) = (planning_goal_client, defined_goal.as_ref()) {
                let existing = self.mgr.plan_snapshot().await;
                let plan = if let Some(steps) = embedded_steps {
                    let steps = kres_core::plan::normalize_steps(steps);
                    if steps.is_empty() {
                        kres_agents::define_plan(
                            gc,
                            &planning_text,
                            goal,
                            task_mode,
                            existing.as_ref(),
                            Some(self.mgr.root_shutdown().clone()),
                        )
                        .await
                    } else {
                        kres_core::async_eprintln!(
                            "[embedded plan] {} step(s) from prompt template",
                            steps.len()
                        );
                        let mut plan =
                            kres_core::Plan::new(&persisted_plan_prompt, goal, task_mode);
                        plan.steps = steps;
                        Some(plan)
                    }
                } else {
                    kres_agents::define_plan(
                        gc,
                        &planning_text,
                        goal,
                        task_mode,
                        existing.as_ref(),
                        Some(self.mgr.root_shutdown().clone()),
                    )
                    .await
                };
                if let Some(mut plan) = plan {
                    // define_plan needs the scan to rank its steps, but the
                    // persisted plan owns only the immutable operator prompt.
                    plan.prompt.clone_from(&persisted_plan_prompt);
                    log_plan_change("define_plan", existing.as_ref(), &plan);
                    self.mgr.set_plan(Some(plan.clone())).await;
                    if review_submission {
                        let todos = review_todos_from_plan(&plan);
                        let todo_count = todos.len();
                        if todo_count > 0 && self.mgr.seed_todo_if_empty(todos).await {
                            kres_core::async_eprintln!(
                                "review planner: seeded {} linked todo(s)",
                                todo_count
                            );
                            self.interrupted_prompt.lock().await.take();
                            return true;
                        }
                    }
                }
            }
        }
        let orc_task = orc.clone();
        // Snapshot findings BEFORE spawning so the task's
        // RunContext sees the running list. bugs.md#H1: the read is
        // cheap and doesn't hold any lock across the API call.
        let previous_findings = self.mgr.findings_snapshot().await;
        // This value reaches the promoter and lens consolidator, so it must be
        // the complete task scope. UI renderers truncate independently.
        let task_brief = text.clone();
        let task_brief_clone = task_brief.clone();
        let active_plan_step_id = step_id.clone();
        let lenses = self.lenses.read().await.clone();
        let lens_consolidate_rules = self.lens_consolidate_rules.read().await.clone();
        let consolidator = self.consolidator.clone();
        // The current operator submission is already the full prompt. Derived
        // todo tasks instead inherit the clean top-level plan prompt, so the
        // pipeline can prepend real original scope without duplicating the
        // current task (or its whole-file scan).
        let plan_for_ctx = self.mgr.plan_snapshot().await;
        let original_prompt = if include_recent_context {
            String::new()
        } else {
            plan_for_ctx
                .as_ref()
                .map(|plan| plan.prompt.clone())
                .unwrap_or_default()
        };
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
            let context_preamble = build_recent_context_preamble(&self.accumulated.lock().await);
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
        let review_target = self.review_file_scan_target.read().await.clone();
        let review_scan = if matches!(task_mode, kres_agents::TaskMode::Audit) {
            match review_target.as_deref() {
                Some(target) => {
                    review_file_scan_context(&self.mgr, &self.cfg.workspace, target).await
                }
                None => None,
            }
        } else {
            None
        };
        if matches!(task_mode, kres_agents::TaskMode::Audit)
            && review_target.is_some()
            && review_scan.is_none()
        {
            self.stop_latched.store(true, Ordering::Release);
            *self.interrupted_prompt.lock().await = Some(prompt_for_park.clone());
            kres_core::async_eprintln!(
                "review target or revision changed after the whole-file risk scan; task was parked instead of running without authoritative ratings"
            );
            return false;
        }
        let has_review_scan = review_scan.is_some();
        let text = match review_scan {
                Some(scan) => format!(
                    "WHOLE-FILE RISK SCAN (already completed; do not request another survey):\n{scan}\n\n---\n\n{text}"
                ),
                None => text,
        };
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
                    active_plan_step_id,
                    task_brief: task_brief_clone,
                    original_prompt,
                    gather_prompt: None,
                    disable_skill_reads: false,
                    allowed_gather_kinds: if has_review_scan {
                        Some(
                            [
                                "source", "type", "callers", "callees", "search", "grep", "read",
                                "file", "find", "git", "question",
                            ]
                            .into_iter()
                            .map(str::to_string)
                            .collect(),
                        )
                    } else {
                        None
                    },
                    mode: task_mode,
                    plan: plan_for_ctx,
                    allow_plan_rewrite,
                    synthesis_use_fast: false,
                    // The REPL task path is not a workflow step: no
                    // OUTPUT SCHEMA tail, so the historical per-mode
                    // slow prompt selection applies.
                    synthesis_system: None,
                    // The REPL task path uses TaskManager's own
                    // symbol/context cache, not the workflow per-step
                    // gather seed.
                    ..RunContext::default()
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
                                .run_with_lenses(
                                    &text,
                                    &lenses,
                                    &c,
                                    lens_consolidate_rules.as_deref(),
                                    &ctx,
                                    &handle.shutdown,
                                )
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
                                let change = mgr.apply_plan_rewrite(rewrite).await;
                                log_plan_change(
                                    "slow: plan rewrite",
                                    change.prior.as_ref(),
                                    &change.current,
                                );
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
        true
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
        // Pending together with running work. Otherwise /stop leaves the queue full and
        // the next /continue (or the reaper's goal-not-met
        // injection after the next task completes) immediately
        // redispatches what the operator just stopped. Operator
        // can get them back with /followup.
        let carry = self.mgr.defer_all_after_stop().await;
        kres_core::async_eprintln!(
            "/stop: requested={} stopped={} grace_expired={} (auto-continue paused; {} pending item(s) moved to /followup; /continue or a new prompt resumes)",
            out.requested, out.stopped, out.grace_expired, carry
        );
    }

    async fn cmd_continue(&self) {
        self.continue_work(ContinueSource::Operator).await
    }

    /// Auto-continue from the idle loop.
    ///
    /// Deliberately NOT `/continue`. Two of that command's side
    /// effects are statements of operator intent and must not fire on
    /// a timer: clearing the `/stop` latch, and pulling the deferred
    /// ledger back into the todo list. The second matters more now
    /// that dispatch is no longer gated on a full drain — the idle
    /// loop can fire while tasks are still running, so work the goal
    /// agent deliberately deferred would be resurrected by a timeout
    /// rather than by someone asking for it.
    async fn auto_continue(&self) {
        self.continue_work(ContinueSource::Idle).await
    }

    async fn continue_work(&self, source: ContinueSource) {
        let label = match source {
            ContinueSource::Operator => "/continue",
            ContinueSource::Idle => "auto-continue",
        };
        if matches!(source, ContinueSource::Operator) {
            // Operator opted back in — clear the /stop auto-continue latch.
            self.stop_latched
                .store(false, std::sync::atomic::Ordering::Release);
        }
        // §22: an interrupted inference wins over batch dispatch.
        // Re-submit the stashed prompt and return — the operator
        // gets their work back before new items start.
        let stashed = self.interrupted_prompt.lock().await.take();
        if let Some(prompt) = stashed {
            kres_core::async_eprintln!(
                "{label}: resuming interrupted prompt: {}",
                truncate(&prompt, 80)
            );
            self.submit_prompt(prompt).await;
            return;
        }
        if matches!(source, ContinueSource::Operator) {
            // Pull any deferred items (from goal-met or --turns drains)
            // back into the active todo list as Pending so they get
            // dispatched here. The "/continue to pursue" message we
            // print on goal-met implies this will happen; without it
            // the operator has to re-type every deferred item by hand.
            let (carry, added) = self.mgr.restore_deferred().await;
            if carry > 0 {
                kres_core::async_eprintln!(
                    "/continue: pulled {carry} deferred item(s), added {added} to todo list"
                );
            }
        }
        let outcome = self.dispatch_ready(None, label).await;
        if let Some(reason) = outcome.refused {
            // A refusal is not a dispatch. Saying so plainly matters:
            // the idle loop polls every five seconds, and phrasing
            // this as an action made the log read as though batch
            // after batch was starting when nothing was.
            kres_core::async_eprintln!("{label}: deferred — {reason}");
            return;
        }
        let mut msg = format!("{label}: dispatched {} item(s)", outcome.dispatched);
        if outcome.blocked > 0 {
            msg.push_str(&format!(", {} blocked on unfinished deps", outcome.blocked));
        }
        if outcome.remaining > 0 {
            msg.push_str(&format!(
                ", {} left — dispatched automatically as slots free up",
                outcome.remaining
            ));
        }
        kres_core::async_eprintln!("{msg}");
    }

    /// Claim ready work and start it. This is the ONLY path that
    /// starts pipeline tasks: `/continue`, `/next` and the post-reap
    /// refill all come through here.
    ///
    /// Dispatch does NOT wait for the reaper. An earlier design made
    /// it wait for the reap queue to drain, and that serialised every
    /// new task behind a ~65s publication: half the refill attempts in
    /// the 2026-08-06 aug6-4 run were refused with "N task(s) waiting
    /// to be reaped", handing back much of the idle time the whole
    /// rework had just reclaimed.
    ///
    /// Claiming during a reap is safe because the todo list is
    /// Rust-owned and every mutation takes the manager's write lock.
    /// The one hazard — a row being claimed while the todo agent's
    /// round trip is in flight — is already handled by
    /// `merge_inferred_state`, which restores the live status of any
    /// row that went InProgress or terminal after its inference
    /// snapshot was taken.
    ///
    /// The bound is a start budget instead: at most `max_parallel`
    /// tasks may start without a reap completing. Fast tasks cannot
    /// churn forever while the reaper never gets a turn at the rate
    /// limiter, and a slow reaper cannot stall dispatch outright.
    async fn dispatch_ready(&self, limit: Option<usize>, reason: &'static str) -> DispatchOutcome {
        use std::sync::atomic::Ordering;
        if self.stop_latched.load(Ordering::Acquire) {
            return DispatchOutcome::refused("/stop is latched; /continue re-arms dispatch");
        }
        if self.turns_cap_reached.load(Ordering::Acquire) {
            return DispatchOutcome::refused("--turns cap reached; no new work will start");
        }
        let free = self.mgr.free_slots().await;
        if free == 0 {
            return DispatchOutcome::refused_owned(format!(
                "all {} slot(s) busy",
                self.mgr.max_parallel()
            ));
        }
        if self.mgr.start_budget().await == 0 {
            return DispatchOutcome::refused_owned(format!(
                "{} task(s) started since the last reap completed; waiting for one to publish",
                self.mgr.max_parallel()
            ));
        }
        // Every remaining bound — free slots, turn budget, start
        // budget — is applied inside the claim, under the same lock
        // that flips the rows, so a concurrent dispatch cannot
        // double-spend a slot.
        let claims = self
            .mgr
            .claim_ranked_todos(limit.unwrap_or(usize::MAX), self.cfg.turns_limit)
            .await;
        let dispatched = claims.items.len();
        if dispatched > 0 {
            fn bound(n: usize) -> String {
                if n == usize::MAX {
                    "unbounded".to_string()
                } else {
                    n.to_string()
                }
            }
            kres_core::async_eprintln!(
                "[dispatch {reason}] starting {dispatched} task(s); {} slot(s) were free, {} start(s) left before a reap must publish",
                bound(free),
                bound(self.mgr.start_budget().await),
            );
        }
        for item in &claims.items {
            let prompt = if item.reason.is_empty() {
                format!("[{}] {}", item.kind, item.name)
            } else {
                format!("[{}] {}: {}", item.kind, item.name, item.reason)
            };
            let tag = if !item.id.is_empty() {
                item.id.clone()
            } else {
                item.name.clone()
            };
            let sid = if item.step_id.is_empty() {
                None
            } else {
                Some(item.step_id.clone())
            };
            self.submit_from_pipeline(prompt, Some(tag), sid).await;
        }
        DispatchOutcome {
            dispatched,
            blocked: claims.blocked,
            remaining: claims.remaining,
            refused: None,
        }
    }

    async fn cmd_next(&self) {
        let outcome = self.dispatch_ready(Some(1), "/next").await;
        if let Some(reason) = outcome.refused {
            kres_core::async_eprintln!("/next: {reason}");
            return;
        }
        if outcome.dispatched == 0 {
            if outcome.blocked == 0 {
                kres_core::async_eprintln!("/next: nothing pending");
            } else {
                kres_core::async_eprintln!(
                    "/next: {} pending item(s) but all are blocked on unfinished deps",
                    outcome.blocked
                );
            }
        }
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
        let def = self.mgr.deferred_snapshot().await;
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

    /// `/summary` — validate every finding with the shared validate workflow,
    /// then render a plain-text bug report via the fast agent using the
    /// `summary` slash-command template. Pass `markdown=true` (via
    /// `/summary-markdown`) to select the markdown-variant template
    /// and default the output filename to `summary.md` instead of
    /// `summary.txt`.
    ///
    /// report.md is NOT consulted. Validation artifacts are written beneath
    /// the results directory and only validated narratives reach the renderer.
    async fn cmd_summary(&self, filename: Option<String>, markdown: bool) {
        let Some(orc) = self.agent_runner.as_ref() else {
            async_println(
                "/summary: AgentRunner not configured (need --fast-agent and --slow-agent)",
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
                std::fs::read_to_string(&p)
                    .ok()
                    .filter(|s| !s.trim().is_empty())
            }),
        };
        let validation_dir = output_dir
            .as_ref()
            .map(|dir| dir.join("summary-validation"))
            .unwrap_or_else(|| PathBuf::from("summary-validation"));
        let validated_findings = match crate::summary::validate_findings_for_summary(
            crate::summary::SummaryValidationInputs {
                findings_path: findings_path.clone(),
                validation_dir,
                workspace: self.cfg.workspace.clone(),
                agent_runner: orc.clone(),
                skills_dir: dirs::home_dir().map(|home| home.join(".kres/skills")),
                shutdown: self.mgr.root_shutdown().child(),
            },
        )
        .await
        {
            Ok(findings) => findings,
            Err(error) => {
                async_println(format!("/summary: validation failed: {error:#}"));
                return;
            }
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
            thinking: orc.fast_thinking,
            validated_findings,
            logger: self.logger.clone(),
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

    /// `/fix <target>` — dispatch the embedded `fix` workflow with
    /// the operator's target string. `/fix` is workflow-only.
    async fn cmd_fix(&self, target: String) {
        self.dispatch_workflow("fix", target).await;
    }

    async fn cmd_review(&self, target: String) {
        let target_trimmed = target.trim();
        if target_trimmed.is_empty() {
            async_println("/review: expected a target, e.g. /review <path or diff>".to_string());
            return;
        }
        let override_dir = dirs::home_dir().map(|h| h.join(".kres"));
        match crate::workflow::review_prompt_file_from_target(
            target_trimmed,
            override_dir.as_deref(),
        ) {
            Ok(cfg) => {
                async_println(format!(
                    "/review: loaded {} lens(es) + {} chars of prose from {}",
                    cfg.prompt_file.lenses.len(),
                    cfg.prompt_file.prompt.len(),
                    cfg.source,
                ));
                self.install_review_config_and_submit(cfg).await;
            }
            Err(e) => {
                async_println(format!("/review: failed to load review workflow — {e:#}"));
            }
        }
    }

    async fn cmd_triage(&self, target: String) {
        self.dispatch_workflow("triage", target).await;
    }

    async fn cmd_validate(&self, finding: String, workspace: Option<String>) {
        let finding_trimmed = finding.trim();
        if finding_trimmed.is_empty() {
            async_println(
                "/validate: expected a finding directory, e.g. /validate <finding-dir> [source-workspace]"
                    .to_string(),
            );
            return;
        }
        let workflow_workspace =
            resolve_validate_workspace(&self.cfg.workspace, workspace.as_deref());
        let mut inputs = serde_json::Map::new();
        inputs.insert(
            "target".into(),
            serde_json::Value::String(finding_trimmed.to_string()),
        );
        inputs.insert(
            "source_workspace".into(),
            serde_json::Value::String(workflow_workspace.display().to_string()),
        );
        self.dispatch_workflow_inputs("validate", inputs, workflow_workspace)
            .await;
    }

    /// Shared backend for `/fix`, `/triage`, and simple one-target
    /// workflows. When a workflow with id `<name>` exists (operator
    /// override at `~/.kres/workflows/<name>.json` wins; otherwise
    /// the embedded copy), build an [`LlmDriver`] with the session's
    /// AgentRunner and run the workflow with `target=<target>` as
    /// the input.
    async fn dispatch_workflow(&self, name: &str, target: String) {
        let target_trimmed = target.trim();
        if target_trimmed.is_empty() {
            async_println(format!(
                "/{name}: expected a target, e.g. /{name} <path or freeform text>"
            ));
            return;
        }
        let mut inputs = serde_json::Map::new();
        inputs.insert(
            "target".into(),
            serde_json::Value::String(target_trimmed.to_string()),
        );
        self.dispatch_workflow_inputs(name, inputs, self.cfg.workspace.clone())
            .await;
    }

    /// Shared backend for `/fix`, `/triage`, `/validate`. When a
    /// workflow with id `<name>` exists (operator override at
    /// `~/.kres/workflows/<name>.json` wins; otherwise the
    /// embedded copy), build an [`LlmDriver`] with the session's
    /// AgentRunner and run the workflow with the provided inputs.
    /// Trace events stream to the REPL via async_println.
    ///
    async fn dispatch_workflow_inputs(
        &self,
        name: &str,
        mut inputs: serde_json::Map<String, serde_json::Value>,
        workflow_workspace: PathBuf,
    ) {
        // Operator override > embedded.
        let override_dir = dirs::home_dir().map(|h| h.join(".kres").join("workflows"));
        let workflow = match kres_agents::workflow::lookup_workflow(override_dir.as_deref(), name) {
            Ok(wf) => wf,
            Err(_) => {
                async_println(format!("/{name}: no workflow named '{name}'"));
                return;
            }
        };

        let Some(orch) = self.agent_runner.clone() else {
            async_println(format!(
                "/{name}: workflow .{name}. is loaded but the REPL has no AgentRunner wired. \
                 Restart kres with --fast-agent <path> AND --slow-agent <path> (or --slow <tag>) \
                 to enable workflow dispatch.",
            ));
            return;
        };
        let orch = self
            .workflow_agent_runner_for_workspace(orch, &workflow_workspace, name)
            .await;

        crate::workflow::apply_results_artifact_dir(
            &workflow,
            &mut inputs,
            self.cfg.results_dir.as_deref(),
        );
        if workflow.inputs.contains_key("slow_secondary_available") {
            let has_secondary = self
                .agent_runner
                .as_ref()
                .map(|runner| runner.slow_variants.len() > 1)
                .unwrap_or(false);
            inputs.insert(
                "slow_secondary_available".into(),
                serde_json::Value::Bool(has_secondary),
            );
        }
        if workflow.inputs.contains_key("assisted_by") {
            inputs.insert(
                "assisted_by".into(),
                serde_json::Value::String(self.cfg.assisted_by.clone()),
            );
        }
        let inputs = kres_agents::workflow_runner::derive_inputs(&workflow, inputs);

        async_println(format!(
            "/{name}: dispatching to workflow '{}' ({} step(s))",
            workflow.id,
            workflow.steps.len()
        ));

        // Build a fresh LlmDriver against the session's
        // AgentRunner + workspace. Skills are loaded on a
        // best-effort basis from ~/.kres/skills.
        // Fix #8: thread the session's root shutdown into the
        // workflow runner so ctrl-C in the REPL cancels the
        // in-flight LLM calls. We pass a child token so cancelling
        // the workflow doesn't kill the rest of the REPL.
        let workflow_shutdown = self.mgr.root_shutdown().child();
        let driver_init = kres_agents::workflow_runner::LlmDriver::new(
            workflow_workspace.clone(),
            workflow.clone(),
        )
        .with_agent_runner(orch)
        .with_shutdown(workflow_shutdown);
        let driver_init = match self.workflow_classifier.as_ref() {
            Some(env) => driver_init.with_classifier(env.clone()),
            None => driver_init,
        };
        let skills_dir = dirs::home_dir().map(|h| h.join(".kres").join("skills"));
        let mut driver = match skills_dir.as_ref() {
            Some(dir) => match driver_init.with_skills_dir(dir) {
                Ok((d, warnings)) => {
                    for w in &warnings {
                        async_println(format!("/{name}: skill warning: {w}"));
                    }
                    d
                }
                Err(e) => {
                    async_println(format!("/{name}: skill loading failed: {e}"));
                    kres_agents::workflow_runner::LlmDriver::new(
                        workflow_workspace.clone(),
                        workflow.clone(),
                    )
                }
            },
            None => driver_init,
        };

        // Fix #9: stream trace events as they happen so the
        // operator sees fast-round counters / fan-out / lens
        // results live, not just after the run finishes.
        let observer: kres_agents::workflow_exec::EventObserver = std::sync::Arc::new(move |ev| {
            async_println(
                kres_agents::workflow_exec::format_event(ev)
                    .trim_end_matches('\n')
                    .to_string(),
            );
        });
        let run = crate::workflow::run_workflow_driver(
            &workflow,
            &mut driver,
            inputs,
            crate::workflow::WorkflowRunOptions {
                iteration_cap: 200,
                // This run's own directory, never a shared one.
                state_dir: Some(self.cfg.session_dir.join("workflow-state")),
                results_dir: self.cfg.results_dir.clone(),
                observer: Some(observer),
                ..Default::default()
            },
        )
        .await;
        match run {
            Ok(result) => {
                async_println(format!(
                    "/{name}: workflow {}",
                    crate::workflow::workflow_status_label(&result.trace.status)
                ));
                for path in result.written_artifacts {
                    async_println(format!("/{name}: wrote {}", path.display()));
                }
            }
            Err(e) => {
                async_println(format!("/{name}: workflow failed before execution — {e:#}"));
            }
        }
    }

    async fn workflow_agent_runner_for_workspace(
        &self,
        base: Arc<AgentRunner>,
        workflow_workspace: &Path,
        name: &str,
    ) -> Arc<AgentRunner> {
        if same_workspace(&self.cfg.workspace, workflow_workspace) {
            return base;
        }
        let fetcher = self
            .workflow_fetcher_for_workspace(workflow_workspace, name)
            .await;
        Arc::new(AgentRunner {
            fast_client: base.fast_client.clone(),
            fast_model: base.fast_model.clone(),
            fast_system: base.fast_system.clone(),
            fast_max_tokens: base.fast_max_tokens,
            fast_max_input_tokens: base.fast_max_input_tokens,
            fast_thinking: base.fast_thinking,
            slow_client: base.slow_client.clone(),
            slow_model: base.slow_model.clone(),
            slow_system: base.slow_system.clone(),
            slow_max_tokens: base.slow_max_tokens,
            slow_max_input_tokens: base.slow_max_input_tokens,
            slow_thinking: base.slow_thinking,
            slow_variants: base.slow_variants.clone(),
            comparison_path: base.comparison_path.clone(),
            comparison_lock: base.comparison_lock.clone(),
            slow_coding_system: base.slow_coding_system.clone(),
            slow_generic_system: base.slow_generic_system.clone(),
            routing_system: base.routing_system.clone(),
            workflow_synthesis_system: base.workflow_synthesis_system.clone(),
            fetcher,
            max_fast_rounds: base.max_fast_rounds,
            // The base runner's skills were auto-selected for the
            // REPL workspace. Leave this empty so LlmDriver's
            // workflow-local `skills: ["auto"]` prelude is used for
            // the validation source workspace instead.
            skills: None,
            usage: base.usage.clone(),
            logger: base.logger.clone(),
        })
    }

    async fn workflow_fetcher_for_workspace(
        &self,
        workflow_workspace: &Path,
        name: &str,
    ) -> Arc<dyn DataFetcher> {
        let workspace_fetcher = kres_agents::WorkspaceFetcher::new(workflow_workspace);
        let mcp_path = self
            .cfg
            .mcp_config
            .clone()
            .or_else(|| dirs::home_dir().map(|h| h.join(".kres").join("mcp.json")));
        let Some(mcp_path) = mcp_path.filter(|p| p.exists()) else {
            return workspace_fetcher;
        };
        let registry = match kres_mcp::ServerRegistry::load_from_file(&mcp_path) {
            Ok(registry) => registry,
            Err(e) => {
                async_println(format!(
                    "/{name}: mcp-config load failed ({}): {e}; using workspace-only fetcher",
                    mcp_path.display()
                ));
                return workspace_fetcher;
            }
        };
        let Some((server_name, server_cfg)) = registry.servers.iter().next() else {
            return workspace_fetcher;
        };
        let log_dir = self
            .logger
            .as_ref()
            .map(|logger| logger.session_dir().join("mcp-logs"))
            .unwrap_or_else(|| workflow_workspace.join(".kres").join("logs").join("mcp"));
        let server_cfg = server_cfg.with_workspace_cwd(server_name, workflow_workspace);
        let client = match kres_mcp::McpClient::spawn(server_name, &server_cfg, &log_dir).await {
            Ok(client) => client,
            Err(e) => {
                async_println(format!(
                    "/{name}: mcp spawn `{server_name}` failed: {e}; using workspace-only fetcher"
                ));
                return workspace_fetcher;
            }
        };
        async_println(format!(
            "/{name}: spawned mcp `{server_name}` for workspace {} (log: {})",
            workflow_workspace.display(),
            client.stderr_log_path().display()
        ));
        let shared = Arc::new(tokio::sync::Mutex::new(client));
        self.register_mcp_clients(vec![shared.clone()]).await;
        kres_agents::McpFetcher::from_shared(shared, workspace_fetcher)
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
        let target_id = if pending[index - 1].id.is_empty() {
            target_name.clone()
        } else {
            pending[index - 1].id.clone()
        };
        self.mgr.remove_todo(&target_id).await;
        kres_core::async_eprintln!("/done: removed {}", truncate(&target_name, 80));
    }

    /// §46: decide whether the idle loop should auto-launch a
    /// `/continue` on timeout. Conditions (mirroring):
    /// no tracked tasks (including terminal tasks awaiting reaping), at least
    /// one pending todo, and at least one pending item whose deps are
    /// satisfied.
    async fn should_auto_continue(&self) -> bool {
        use kres_core::TodoStatus;
        if self.stop_latched.load(std::sync::atomic::Ordering::Acquire) {
            return false;
        }
        // Neither a running task nor an unpublished terminal one is a
        // reason to hold back work. Requiring the whole task list to
        // be empty cost 33% of wall-clock time idle; requiring the
        // reap queue to be empty then serialised each new task behind
        // a ~65s publication. What bounds dispatch now is the slot cap
        // and the start budget, both enforced inside the claim.
        if self.mgr.free_slots().await == 0 || self.mgr.start_budget().await == 0 {
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
        self.mgr.clear_active_todos().await;
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
        if let Some(out) =
            format_usage_summary(&self.usage, "usage", Some("(no API usage recorded yet)"))
        {
            self.print_command_output(&out);
        }
    }

    fn print_exit_cost_summary_direct(&self) {
        let Some(out) = format_usage_summary(
            &self.usage,
            "final usage before exit",
            Some("final usage before exit: no API usage recorded"),
        ) else {
            return;
        };
        use std::io::Write as _;
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(stderr, "{out}");
        let _ = stderr.flush();
    }

    fn restore_terminal_for_final_output(&self) {
        if self.cfg.tui && !self.cfg.stdio {
            crate::tui::emergency_restore_terminal();
        } else if !self.cfg.stdio {
            crate::status::restore();
        }
    }

    fn print_command_output(&self, out: &str) {
        if !self.cfg.stdio && !self.cfg.tui && crate::status::print_command_block(out) {
            return;
        }
        kres_core::async_eprintln!("{out}");
    }

    async fn cmd_clear(&self) {
        // bugs.md#C2: cancel first, reset state after.
        let out = self.mgr.stop_all(self.cfg.stop_grace).await;
        self.mgr.replace_findings(vec![]).await;
        self.mgr.clear_session_work().await;
        // Also wipe the accumulated-analysis ledger so the next
        // prompt starts with a clean slate. Without this, the
        // "recent context" preamble submit_prompt injects would
        // keep referencing work the operator just said to forget.
        self.accumulated.lock().await.clear();
        *self.last_analysis.lock().await = None;
        // Drop the cached session goal too. Without this, the
        // first pipeline-driven follow-up after /clear would
        // inherit the prior topic's goal — exactly the
        // cross-topic bleed /clear exists to prevent.
        *self.session_goal.lock().await = None;
        let removed_change_checkpoint = self
            .cfg
            .persist_path
            .as_ref()
            .map(|path| remove_change_survey_checkpoint(&path.with_file_name("change-survey.json")))
            .transpose()
            .map(Option::unwrap_or_default)
            .unwrap_or_else(|error| {
                kres_core::async_eprintln!("/clear: failed to remove change survey: {error}");
                false
            });
        // Drop every outside-workspace consent. The store is
        // global (OnceLock); without this a /clear would leave
        // grants from the prior topic in place and a follow-up
        // prompt on a different topic could quietly read paths the
        // operator forgot they'd allowed.
        let dropped_grants = kres_core::consent::get().map(|s| s.clear()).unwrap_or(0);
        kres_core::async_eprintln!(
            "/clear: stopped {} task(s), reset findings + todo + accumulated context, dropped {} consent grant(s), removed change survey: {}",
            out.stopped + out.grace_expired,
            dropped_grants,
            removed_change_checkpoint
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
        let Some(orc) = self.agent_runner.as_ref() else {
            kres_core::async_eprintln!("/compact: no AgentRunner configured");
            return;
        };
        // Build the inference request: feed every accumulated entry
        // to the fast agent and ask for a terse single-paragraph
        // summary. Reuse the fast client the AgentRunner already
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
            "instructions": "Compress the preceding task-by-task analysis ledger into a single TERSE summary — 2 to 6 sentences total — that preserves: (a) what code was examined, (b) what files were written, if any, (c) key findings or decisions, (d) open questions still worth pulling on. Omit per-task boilerplate and restated code. Return raw, unfenced JSON only—no Markdown backticks: {\"summary\": \"the compressed text\"}"
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
        if let Some(thinking) = orc.fast_thinking {
            cfg = cfg.with_thinking(thinking);
        }
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
            cached_prefixes: Vec::new(),
        }];
        if let Some(lg) = &self.logger {
            let request = cfg.request_meta();
            lg.log_main_with_request(
                "user",
                Some("phase=compact"),
                &body,
                None,
                None,
                Some(&request),
            );
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
                Some("phase=compact"),
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
        let summary = parse_compact_response(&text);
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

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CompactResponse {
    summary: String,
}

fn parse_compact_response(text: &str) -> Option<String> {
    kres_agents::json_repair::parse_strict_json::<CompactResponse>("compact", text)
        .ok()
        .and_then(|response| (!response.summary.trim().is_empty()).then_some(response.summary))
}

fn review_todos_from_plan(plan: &kres_core::Plan) -> Vec<kres_core::TodoItem> {
    plan.steps
        .iter()
        .map(|step| {
            let mut todo = kres_core::TodoItem::new(step.title.clone(), "review");
            todo.id = format!("review-{}", step.id);
            todo.step_id = step.id.clone();
            todo.reason = step.description.clone();
            todo.depends_on = step
                .depends_on
                .iter()
                .map(|id| format!("review-{id}"))
                .collect();
            todo
        })
        .collect()
}

/// Max total size of the "recent context" preamble
/// `submit_prompt` injects ahead of a new operator prompt. The
/// accumulated ledger can grow without bound across a long session;
/// capping here keeps the attached-context cost bounded. Use
/// /compact to trim the ledger itself; this cap only limits what
/// leaks into each new task's prompt.
const REVIEW_FILE_SCAN_CACHE_KEY: &str = "review:file-risk-scan";
const CHANGE_SURVEY_CHECKPOINT_VERSION: u32 = 3;
/// A semantic partition target, never a request or information ceiling. Every
/// byte of the diff is sent exactly once across the chunk bodies (with repeated
/// hunk headers for orientation), and provider transport may frame each prompt
/// further without altering its visible content.
const CHANGE_SURVEY_DIFF_PARTITION_BYTES: usize = 500_000;
// A large change-survey map request carries one source partition and one diff
// partition. Keep their combined payload near the same target as the small
// path instead of allowing two individually-large halves to double it.
const CHANGE_SURVEY_PAIR_PARTITION_BYTES: usize = CHANGE_SURVEY_DIFF_PARTITION_BYTES / 2;
const CHANGE_SURVEY_CHUNK_CONCURRENCY: usize = 8;

/// Render every accumulated-analysis entry into the inference preamble,
/// newest-first. Selection happens at the call-site; once selected, an entry
/// is preserved completely.
/// Returns an empty string when the ledger is empty.
fn build_recent_context_preamble(entries: &[AccumulatedEntry]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let mut out = String::from("Recent context from this session (most recent first):\n\n");
    for e in entries.iter().rev() {
        out.push_str(&format!("### {}\n{}", e.task, e.analysis));
        if !e.analysis.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
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

fn load_routing_system() -> Option<String> {
    load_prompt_disk_then_embedded("routing-agent.system.md")
}

fn load_workflow_synthesis_system() -> Option<String> {
    load_prompt_disk_then_embedded("workflow-synthesis.system.md")
}

/// Convenience: build an AgentRunner from paths to agent configs and
/// a workspace directory. The DataFetcher is a WorkspaceFetcher over
/// the given workspace; MCP integration is a Phase 8 add-on.
/// Built components from a pair of agent configs.
///
/// The AgentRunner is the task runner; the ConsolidatorClient is the
/// fast-agent-flavoured LLM handle used by `run_with_lenses` to merge
/// N parallel lens outputs into a unified analysis + deduplicated
/// findings list.
pub struct BuiltAgents {
    pub agent_runner: Arc<AgentRunner>,
    pub consolidator: Arc<kres_agents::ConsolidatorClient>,
    /// Review planning uses the primary slow model.  These clients
    /// deliberately share its transport/rate limiter while using the
    /// structured goal and todo contracts.
    pub review_goal_client: Arc<kres_agents::GoalClient>,
    pub review_todo_client: Arc<kres_agents::TodoClient>,
}

/// Optional knobs threaded into `build_agent_runner`. Splitting them
/// out keeps the call site readable and the function signature
/// narrow.
#[derive(Default)]
pub struct AgentRunnerBuildOptions {
    pub extra_slow_cfgs: Vec<(PathBuf, Option<String>)>,
    pub compare_slow_models: bool,
    pub skills: Option<serde_json::Value>,
    pub usage: Option<Arc<UsageTracker>>,
    pub gather_turns: u8,
    pub logger: Option<Arc<TurnLogger>>,
    pub comparison_path: Option<PathBuf>,
}

pub async fn build_agent_runner(
    fast_cfg_path: &Path,
    slow_cfg_path: &Path,
    workspace: impl Into<PathBuf>,
    fetcher: Arc<dyn DataFetcher>,
    settings: &crate::settings::Settings,
    options: AgentRunnerBuildOptions,
) -> Result<BuiltAgents> {
    let AgentRunnerBuildOptions {
        extra_slow_cfgs,
        compare_slow_models,
        skills,
        usage,
        gather_turns,
        logger,
        comparison_path,
    } = options;
    let fast_cfg = AgentConfig::load_for_role(fast_cfg_path, AgentKind::Fast)
        .with_context(|| format!("loading fast agent config {}", fast_cfg_path.display()))?;
    let slow_cfg = AgentConfig::load_for_role(slow_cfg_path, AgentKind::Slow)
        .with_context(|| format!("loading slow agent config {}", slow_cfg_path.display()))?;

    let fast_credentials = fast_cfg.credentials()?;
    let slow_credentials = slow_cfg.credentials()?;
    let fast_key = fast_credentials.cache_key();
    let slow_key = slow_credentials.cache_key();

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
    let fast_max_tokens = fast_cfg.max_tokens.unwrap_or(fast_model.max_output_tokens);
    let slow_max_tokens = slow_cfg.max_tokens.unwrap_or(slow_model.max_output_tokens);
    let fast_thinking = fast_cfg
        .thinking
        .as_ref()
        .map(|thinking| thinking.to_budget(fast_max_tokens));
    let slow_thinking = slow_cfg
        .thinking
        .as_ref()
        .map(|thinking| thinking.to_budget(slow_max_tokens));

    // Shared rate limiter keyed by API-key string: agents using the
    // same key share a bucket so they can't collectively burst past
    // the per-key server limit. Capacity comes from whichever config
    // was read first for that key. Keys are inline in model configs,
    // so the literal secret is the shared limiter key when two roles
    // intentionally use the same account.
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
    let fast_client = Arc::new(
        fast_cfg
            .client_builder()?
            .rate_limiter(fast_limiter.clone())
            .build()?,
    );
    let slow_client = Arc::new(
        slow_cfg
            .client_builder()?
            .rate_limiter(slow_limiter.clone())
            .build()?,
    );
    let mut slow_variants = vec![kres_agents::pipeline::SlowAgentVariant {
        client: slow_client.clone(),
        model: slow_model.clone(),
        system: slow_cfg.system.clone(),
        max_tokens: slow_max_tokens,
        max_input_tokens: slow_cfg.max_input_tokens,
        thinking: slow_thinking,
        label: slow_model.id.clone(),
        supplemental_lens_only: false,
    }];
    for (cfg_path, model_override) in extra_slow_cfgs {
        if cfg_path == slow_cfg_path {
            continue;
        }
        let cfg = AgentConfig::load_for_role(&cfg_path, AgentKind::Slow)
            .with_context(|| format!("loading slow agent config {}", cfg_path.display()))?;
        let mut variant_settings = settings.clone();
        if let Some(id) = model_override {
            variant_settings.set_model(crate::settings::ModelRole::Slow, Some(id));
        }
        let model = crate::settings::pick_model(
            cfg.model.as_deref(),
            crate::settings::ModelRole::Slow,
            &variant_settings,
        );
        let credentials = cfg.credentials()?;
        let key = credentials.cache_key();
        let limiter = if let Some(existing) = limiters.get(&key) {
            Some(existing.clone())
        } else {
            let limiter = cfg.rate_limit.and_then(|c| RateLimiter::new(c as u64));
            if let Some(ref r) = limiter {
                limiters.insert(key.clone(), r.clone());
            }
            limiter
        };
        let client = Arc::new(cfg.client_builder()?.rate_limiter(limiter).build()?);
        let max_tokens = cfg.max_tokens.unwrap_or(model.max_output_tokens);
        let thinking = cfg
            .thinking
            .as_ref()
            .map(|thinking| thinking.to_budget(max_tokens));
        slow_variants.push(kres_agents::pipeline::SlowAgentVariant {
            client,
            model: model.clone(),
            system: cfg.system.clone(),
            max_tokens,
            max_input_tokens: cfg.max_input_tokens,
            thinking,
            label: model.id.clone(),
            supplemental_lens_only: !compare_slow_models,
        });
    }

    let _workspace = workspace.into(); // retained by caller; fetcher already knows.

    let consolidator = Arc::new(kres_agents::ConsolidatorClient {
        client: fast_client.clone(),
        model: fast_model.clone(),
        system: fast_cfg.system.clone(),
        max_tokens: fast_max_tokens,
        max_input_tokens: fast_cfg.max_input_tokens,
        thinking: fast_thinking,
        usage: usage.clone(),
    });

    let review_goal_client = Arc::new(kres_agents::GoalClient {
        client: slow_client.clone(),
        model: slow_model.clone(),
        system: Some(format!(
            "{}\n\nREVIEW PLANNING POLICY:\nYou own the review goal, coverage plan, and completion decision. Obey the explicit TARGET KIND in the original prompt: a current-workspace source target has no implied revision or diff, while a git commit/range starts from its diff. Never invent a ref, base revision, or changed-hunk scope for a source target. For a named source-file target, the prompt contains a WHOLE-FILE RISK SCAN gathered before goal selection. It includes six-month change-informed function ratings, interaction-filtered external research questions, and one final file risk rating. Use that ranked inventory to define an evidence-backed completion goal and a staged plan; never add another survey or scan step. Give retained external research questions priority only because the file survey established an interaction with the target. Return 3 or 4 independent semantic path/contract groups with no dependencies, partitioned by real code paths and prioritized by the ranked functions, plus exactly one final cross-contract completeness step depending on every group. For other targets, create one orientation/context step, a bounded middle wave, and a final completeness step. For define_plan, return steps with id, title, description, and depends_on (an array of earlier step IDs). Never partition by generic review lenses. Do not create more than 5 total steps for a scanned file. Preserve explicit operator scope and require typed followups for evidence that is still missing. The dependency graph is execution policy, not advisory prose.",
            kres_agents::GOAL_INSTRUCTIONS
        )),
        max_tokens: slow_max_tokens,
        max_input_tokens: slow_cfg.max_input_tokens,
        thinking: slow_thinking,
        logger: logger.clone(),
        usage: usage.clone(),
    });
    let review_todo_client = Arc::new(kres_agents::TodoClient {
        client: slow_client.clone(),
        model: slow_model.clone(),
        system: Some(format!(
            "{}\n\nREVIEW PLANNING POLICY:\nThe todo list implements a staged review plan. Preserve existing depends_on edges and stable IDs unless concrete completed evidence makes a dependency obsolete. Orientation/context work must finish before semantic path groups become runnable. Keep the middle wave bounded to 3 or 4 independent groups. Keep the final cross-contract completeness todo dependent on every surviving middle-wave group. Do not flatten all pending review work into one parallel batch. When orientation evidence changes the decomposition, revise the middle groups and final dependencies explicitly while preserving completed history.",
            kres_agents::TODO_INSTRUCTIONS
        )),
        max_tokens: slow_max_tokens,
        max_input_tokens: slow_cfg.max_input_tokens,
        thinking: slow_thinking,
        usage: usage.clone(),
    });

    let slow_coding_system = load_slow_coding_system();
    let slow_generic_system = load_slow_generic_system();
    let routing_system = load_routing_system();
    let workflow_synthesis_system = load_workflow_synthesis_system();
    let agent_runner = Arc::new(AgentRunner {
        fast_client,
        fast_model: fast_model.clone(),
        fast_system: fast_cfg.system,
        fast_max_tokens,
        fast_max_input_tokens: fast_cfg.max_input_tokens,
        fast_thinking,
        slow_client,
        slow_model: slow_model.clone(),
        slow_system: slow_cfg.system,
        slow_max_tokens,
        slow_max_input_tokens: slow_cfg.max_input_tokens,
        slow_thinking,
        slow_variants,
        comparison_path,
        comparison_lock: Arc::new(std::sync::Mutex::new(())),
        slow_coding_system,
        slow_generic_system,
        routing_system,
        workflow_synthesis_system,
        fetcher,
        max_fast_rounds: gather_turns,
        skills,
        usage,
        logger,
    });

    Ok(BuiltAgents {
        agent_runner,
        consolidator,
        review_goal_client,
        review_todo_client,
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
                    "[coding] rejecting absolute path '{}' (outside workspace and no consent on file — mention the containing directory in a prompt to grant this session access)",
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
        if let Err(e) = kres_core::validate_metadata_yaml_content(&out, &f.content) {
            kres_core::async_eprintln!("[coding] rejecting {}: {e}", out.display());
            continue;
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
    kres_core::async_eprintln!("  /review <target>       run the embedded `review` workflow");
    kres_core::async_eprintln!(
        "  /fix <target>          run the embedded `fix` workflow (finding dir or prose)"
    );
    kres_core::async_eprintln!("  /triage <finding-dir>  run the embedded `triage` workflow");
    kres_core::async_eprintln!(
        "  /validate <finding-dir> [workspace]  validate a finding against source"
    );
    kres_core::async_eprintln!("  /summary [FILE]        validate findings, then render a plain-text summary (default summary.txt)");
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

fn resolve_validate_workspace(active_workspace: &Path, workspace: Option<&str>) -> PathBuf {
    let raw = workspace
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(".");
    let expanded = if raw == "~" {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from(raw))
    } else if let Some(rest) = raw.strip_prefix("~/") {
        dirs::home_dir()
            .map(|home| home.join(rest))
            .unwrap_or_else(|| PathBuf::from(raw))
    } else {
        PathBuf::from(raw)
    };
    let resolved = if raw == "." {
        active_workspace.to_path_buf()
    } else if expanded.is_absolute() {
        expanded
    } else {
        active_workspace.join(expanded)
    };
    std::fs::canonicalize(&resolved).unwrap_or(resolved)
}

fn same_workspace(left: &Path, right: &Path) -> bool {
    let left = std::fs::canonicalize(left).unwrap_or_else(|_| left.to_path_buf());
    let right = std::fs::canonicalize(right).unwrap_or_else(|_| right.to_path_buf());
    left == right
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

/// One reaped task, held until the batch's todo/goal pass runs.
///
/// Everything per-task — publication, findings, report, promote — has
/// already happened by the time an entry lands here. What remains is
/// the part that reasons over the shared todo list, and that is done
/// once for the whole batch.
struct ReapedBatchEntry {
    task_id: kres_core::task::TaskId,
    task_name: String,
    /// The row this task was executing, when it completed successfully.
    /// `None` for an errored task: nothing may be marked done.
    todo_id: Option<String>,
    /// What the todo agent reads as this task's result. Carries the
    /// error text instead of the analysis for an errored task.
    analysis: String,
    followups: Vec<serde_json::Value>,
    mode: kres_core::TaskMode,
    lensed_review: bool,
}

struct BatchGoalCheck<'a> {
    mgr: &'a Arc<kres_core::TaskManager>,
    goal_client: &'a kres_agents::GoalClient,
    goal: &'a str,
    prompt: &'a str,
    accumulated: &'a Arc<tokio::sync::Mutex<Vec<AccumulatedEntry>>>,
    lensed_review: bool,
    followup_count: usize,
    follow_followups: bool,
    turns_limit: u32,
}

struct BatchGoalOutcome {
    /// Empty when the goal was met, when no goal agent answered, or
    /// when the agent said "not met" without naming anything concrete.
    missing: Vec<String>,
}

/// Ask the goal agent whether the accumulated analyses satisfy the
/// goal, and act on a met verdict by draining the todo list.
///
/// Split out of the reaper loop when the todo/goal pass moved from
/// per-task to per-batch: the body is unchanged, but it now runs once
/// per distinct goal in a batch rather than once per reaped task.
async fn run_batch_goal_check(check_inputs: BatchGoalCheck<'_>) -> BatchGoalOutcome {
    let BatchGoalCheck {
        mgr,
        goal_client,
        goal,
        prompt,
        accumulated,
        lensed_review,
        followup_count,
        follow_followups,
        turns_limit,
    } = check_inputs;
    let entries = accumulated.lock().await.clone();
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
    let recorded_findings = mgr.findings_snapshot().await;
    let recorded_findings_context = recorded_findings_goal_context(&recorded_findings);
    if !recorded_findings_context.is_empty() {
        combined.push_str("\n\n---\n\n");
        combined.push_str(&recorded_findings_context);
    }
    let plan_for_check = mgr.plan_snapshot().await;
    let check = kres_agents::check_goal(
        goal_client,
        prompt,
        goal,
        &combined,
        plan_for_check.as_ref(),
        Some(mgr.root_shutdown().clone()),
    )
    .await;
    kres_core::async_eprintln!(
        "[goal check] met={} reason={}",
        check.met,
        truncate(&check.reason, 120)
    );
    if !check.met {
        if !check.missing.is_empty() {
            kres_core::async_eprintln!(
                "[goal not yet met — missing: {}]",
                check.missing.join(", ")
            );
        }
        return BatchGoalOutcome {
            missing: check.missing,
        };
    }
    kres_core::async_eprintln!("[goal met: {}]", truncate(&check.reason, 200));
    let pending_or_blocked = mgr
        .todo_snapshot()
        .await
        .iter()
        .filter(|t| {
            matches!(
                t.status,
                kres_core::TodoStatus::Pending | kres_core::TodoStatus::Blocked
            )
        })
        .count();
    // A lensed review turns its remaining followups into the next
    // turn's work instead of draining them.
    let keep_for_next_turn =
        review_followups_drive_next_turn(lensed_review, pending_or_blocked, followup_count);
    if keep_for_next_turn {
        kres_core::async_eprintln!(
            "[goal met, review followups remain: keeping {pending_or_blocked} pending/blocked item(s) as next-turn review work]"
        );
        return BatchGoalOutcome {
            missing: Vec::new(),
        };
    }
    // Drain pending todos into the deferred ledger so /followup can
    // list them. InProgress rows remain active until their executors
    // finish and are reaped. Done/Skipped items stay on the todo list
    // so their step_id linkage survives — the next sync_plan_from_todo
    // tick can then flip any fully-covered plan step to Done.
    let carry = mgr.defer_pending().await;
    if carry > 0 {
        if follow_followups && turns_limit > 0 {
            // --follow + --turns N: pull the deferred items right back
            // into the todo list so auto-continue dispatches them.
            // Without this, goal-met drains to deferred,
            // followups_drained fires, and the session exits with
            // turns still remaining.
            let (_, pulled) = mgr.restore_deferred().await;
            kres_core::async_eprintln!(
                "[goal met, --follow: pulled {pulled} deferred item(s) back into todo list ({} turns remaining)]",
                turns_limit.saturating_sub(mgr.completed_run_count().await)
            );
        } else {
            kres_core::async_eprintln!(
                "[{carry} pending item(s) moved to deferred — run /followup to list, /continue to pursue]"
            );
        }
    }
    BatchGoalOutcome {
        missing: Vec::new(),
    }
}

/// Whether a goal-met verdict should keep the remaining work as the
/// next review turn instead of draining it to /followup.
///
/// `lensed_review` is "this batch is an Audit with lenses installed" —
/// the caller already knows that, so it is passed as the fact it is
/// rather than reconstructed from a `TaskMode`.
fn review_followups_drive_next_turn(
    lensed_review: bool,
    pending_or_blocked: usize,
    new_followups: usize,
) -> bool {
    lensed_review && (pending_or_blocked > 0 || new_followups > 0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnsCapAction {
    Continue,
    DrainAndWait,
    DrainAndExit,
}

fn turns_cap_action(done: u32, limit: u32, outstanding: usize) -> TurnsCapAction {
    if limit == 0 || done < limit {
        TurnsCapAction::Continue
    } else if outstanding > 0 {
        TurnsCapAction::DrainAndWait
    } else {
        TurnsCapAction::DrainAndExit
    }
}

async fn reconcile_turn_cap_todos(mgr: &Arc<TaskManager>) -> (usize, usize) {
    let reset = mgr.reset_in_progress_to_pending().await;
    let deferred = mgr.defer_pending().await;
    (reset, deferred)
}

#[derive(Debug, Clone)]
struct CompletedReviewFileScan {
    target: String,
    source_hash: String,
    baseline: String,
    head: String,
    scan: String,
}

impl CompletedReviewFileScan {
    fn persisted(&self) -> kres_core::ReviewFileScanState {
        kres_core::ReviewFileScanState {
            target: self.target.clone(),
            source_hash: self.source_hash.clone(),
            baseline: self.baseline.clone(),
            head: self.head.clone(),
            scan: self.scan.clone(),
        }
    }
}

async fn cache_review_file_scan(mgr: &Arc<TaskManager>, scan: &CompletedReviewFileScan) {
    mgr.cache_context(
        REVIEW_FILE_SCAN_CACHE_KEY,
        serde_json::to_value(scan.persisted()).expect("review scan state serializes"),
    )
    .await;
}

fn review_target_path(workspace: &Path, target: &str) -> PathBuf {
    if Path::new(target).is_absolute() {
        PathBuf::from(target)
    } else {
        workspace.join(target)
    }
}

fn current_review_head(workspace: &Path) -> Result<String> {
    let head = gix::discover(workspace)
        .context("discovering review repository")?
        .head_id()
        .context("resolving review repository HEAD")?
        .to_string();
    Ok(format!("WORKTREE@{head}"))
}

fn review_file_scan_matches_current_source(
    workspace: &Path,
    state: &kres_core::ReviewFileScanState,
) -> Result<bool> {
    let target = review_target_path(workspace, &state.target);
    let source = std::fs::read_to_string(&target)?;
    Ok(
        change_survey_source_hash(&target, &source)? == state.source_hash
            && current_review_head(workspace)? == state.head,
    )
}

async fn review_file_scan_matches_current_window(
    workspace: &Path,
    state: &kres_core::ReviewFileScanState,
) -> Result<bool> {
    if !review_file_scan_matches_current_source(workspace, state)? {
        return Ok(false);
    }
    let cutoff = chrono::Utc::now()
        .checked_sub_months(chrono::Months::new(6))
        .context("computing six-month review scan cutoff")?
        .timestamp();
    let workspace = workspace.to_path_buf();
    let target = state.target.clone();
    let window = tokio::task::spawn_blocking(move || {
        crate::change_survey::aggregate_target_diff(&workspace, &target, cutoff)
    })
    .await
    .context("joining review scan fingerprint computation")??;
    Ok(window.baseline == state.baseline && window.head == state.head)
}

async fn review_file_scan_context(
    mgr: &Arc<TaskManager>,
    workspace: &Path,
    expected_target: &str,
) -> Option<String> {
    let value = mgr.get_cached_context(REVIEW_FILE_SCAN_CACHE_KEY).await?;
    let state: kres_core::ReviewFileScanState = serde_json::from_value(value).ok()?;
    if state.target != expected_target
        || state.scan.trim().is_empty()
        || !review_file_scan_matches_current_source(workspace, &state).unwrap_or(false)
    {
        mgr.remove_cached_context(REVIEW_FILE_SCAN_CACHE_KEY).await;
        return None;
    }
    Some(state.scan)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChangeSurveyCheckpoint {
    version: u32,
    target: String,
    source_hash: String,
    baseline: String,
    head: String,
    report: Option<ChangeSurveyReport>,
}

#[derive(Clone)]
struct ChangeSurveyCheckpointStore {
    path: PathBuf,
    state: Arc<tokio::sync::Mutex<ChangeSurveyCheckpoint>>,
}

impl ChangeSurveyCheckpointStore {
    fn open(
        path: PathBuf,
        target: String,
        source_hash: String,
        baseline: String,
        head: String,
        reuse_existing: bool,
    ) -> Result<Self> {
        let loaded = reuse_existing
            .then(|| std::fs::read_to_string(&path).ok())
            .flatten()
            .and_then(|body| serde_json::from_str::<ChangeSurveyCheckpoint>(&body).ok())
            .filter(|checkpoint| {
                checkpoint.version == CHANGE_SURVEY_CHECKPOINT_VERSION
                    && checkpoint.target == target
                    && checkpoint.source_hash == source_hash
                    && checkpoint.baseline == baseline
                    && checkpoint.head == head
            });
        let state = loaded.unwrap_or(ChangeSurveyCheckpoint {
            version: CHANGE_SURVEY_CHECKPOINT_VERSION,
            target,
            source_hash,
            baseline,
            head,
            report: None,
        });
        save_change_survey_checkpoint(&path, &state)?;
        Ok(Self {
            path,
            state: Arc::new(tokio::sync::Mutex::new(state)),
        })
    }

    async fn report(&self) -> Option<ChangeSurveyReport> {
        self.state.lock().await.report.clone()
    }

    async fn record(&self, report: ChangeSurveyReport) -> Result<()> {
        let mut state = self.state.lock().await;
        state.report = Some(report);
        let snapshot = state.clone();
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || save_change_survey_checkpoint(&path, &snapshot))
            .await
            .context("joining change-survey checkpoint write")??;
        Ok(())
    }
}

fn save_change_survey_checkpoint(path: &Path, checkpoint: &ChangeSurveyCheckpoint) -> Result<()> {
    use std::io::Write;

    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(checkpoint)?;
    let temporary = path.with_extension("json.tmp");
    {
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(&body)?;
        file.sync_all()?;
    }
    std::fs::rename(&temporary, path)?;
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        if let Ok(directory) = std::fs::File::open(parent) {
            let _ = directory.sync_all();
        }
    }
    Ok(())
}

fn remove_change_survey_checkpoint(path: &Path) -> Result<bool> {
    let mut removed = false;
    for candidate in [path.to_path_buf(), path.with_extension("json.tmp")] {
        match std::fs::remove_file(&candidate) {
            Ok(()) => removed = true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("removing change-survey checkpoint {}", candidate.display())
                });
            }
        }
    }
    Ok(removed)
}

fn change_survey_source_hash(target: &Path, source: &str) -> Result<String> {
    let mut hasher = gix::hash::hasher(gix::hash::Kind::Sha1);
    hasher.update(source.as_bytes());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let executable = std::fs::metadata(target)?.permissions().mode() & 0o111 != 0;
        hasher.update(if executable {
            b"\0mode=100755"
        } else {
            b"\0mode=100644"
        });
    }
    Ok(hasher
        .try_finalize()
        .context("hashing whole-file change-survey source")?
        .to_string())
}

async fn run_review_file_scan(
    runner: &Arc<AgentRunner>,
    workspace: &Path,
    target: &str,
    checkpoint_path: Option<PathBuf>,
    reuse_checkpoint: bool,
    shutdown: &kres_core::Shutdown,
) -> Result<CompletedReviewFileScan> {
    let (_change_window, change_report, _checkpoint) = run_review_change_survey(
        runner,
        workspace,
        target,
        checkpoint_path,
        reuse_checkpoint,
        shutdown,
    )
    .await?;
    let survey = runner
        .fetcher
        .fetch(
            &[Followup {
                kind: "survey".to_string(),
                name: target.to_string(),
                reason: "whole-file structural inventory".to_string(),
                path: None,
                required_for_progress: true,
            }],
            None,
        )
        .await
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    // semcode is an accelerator, not an authority: when its survey is
    // missing or unparseable, rebuild the inventory by inference
    // rather than treating the file as unavailable.
    let inventory = match FileSurveyInventory::from_context(&survey.context) {
        Some(inventory) => inventory,
        None => {
            infer_fallback_file_survey_inventory(
                runner,
                workspace,
                target,
                &survey.context,
                shutdown,
            )
            .await?
        }
    };
    let inventory_functions = inventory.function_names();
    // The survey is a starting point, not an inventory. Keep the
    // ratings that name a real target function, drop the rest, and
    // never re-run: a function it never mentioned is simply unrated,
    // which the scan renders as 0.
    let change_report = change_report
        .map(|report| crate::change_survey::retain_known_functions(report, &inventory_functions))
        .unwrap_or_default();
    let target_path = if Path::new(target).is_absolute() {
        PathBuf::from(target)
    } else {
        workspace.join(target)
    };
    let target_source = std::fs::read_to_string(&target_path)
        .with_context(|| format!("reading whole-file review target {}", target_path.display()))?;
    // The whole-file survey is assembled by Rust from the change
    // survey and the structural inventory. There is no second
    // inference pass.
    //
    // There used to be one: a slow-agent call that combined the two
    // into a "final" rating per function. It was measured on the
    // 2026-08-06 mm/page_alloc.c review and did nothing. It received
    // the change survey's ratings in its prompt and was forbidden from
    // rating any function below them, so of 236 functions it changed
    // 12, all upward, only one crossed into the high band, and none of
    // the 12 produced a finding. Removing it also removes a failure
    // mode: run standalone, without the change survey's report to
    // enumerate the function set, it returned 251 functions instead of
    // 236 and failed validation on two consecutive runs.
    //
    // `file_risk_rating` is the highest function rating, which is what
    // the model was instructed to produce anyway ("must be at least
    // the highest combined function rating"). Research questions are
    // the external interactions Rust already established, one per
    // entry, which is exactly what the model was told to emit.
    let mut risk_of: std::collections::BTreeMap<&str, u8> = change_report
        .target_function_risks
        .iter()
        .map(|risk| (risk.name.as_str(), risk.risk_rating))
        .collect();
    let research_questions: Vec<ReviewResearchQuestion> = change_report
        .external_major_risks
        .iter()
        .filter_map(|risk| {
            inventory
                .interaction_kind(&risk.name, &target_source)
                .map(|kind| ReviewResearchQuestion {
                    question: format!(
                        "{} interacts with {target} via {kind}: {}",
                        risk.name, risk.reason
                    ),
                    function: risk.name.clone(),
                    file: risk.file.clone(),
                    priority: EXTERNAL_RESEARCH_PRIORITY,
                })
        })
        .collect();
    let functions: Vec<ScanFunctionRisk> = inventory
        .functions
        .iter()
        .map(|(name, uses)| ScanFunctionRisk {
            name: name.as_str(),
            uses: *uses,
            risk_rating: risk_of.remove(name.as_str()).unwrap_or_default(),
        })
        .collect();
    let file_risk_rating = functions
        .iter()
        .map(|function| function.risk_rating)
        .max()
        .unwrap_or_default();
    let scan = ScanFileSurvey {
        functions,
        research_questions: &research_questions,
        file_risk_rating,
    };
    let serialized = serde_json::to_string(&scan).context("serializing review scan")?;
    // Keep the completed checkpoint beside session.json so the net-diff
    // assessment remains resumable until the scan reaches the persisted plan.
    Ok(CompletedReviewFileScan {
        target: target.to_string(),
        source_hash: change_survey_source_hash(&target_path, &target_source)?,
        baseline: change_report.baseline,
        head: change_report.head,
        scan: serialized,
    })
}

async fn run_review_change_survey(
    runner: &Arc<AgentRunner>,
    workspace: &Path,
    target: &str,
    checkpoint_path: Option<PathBuf>,
    reuse_checkpoint: bool,
    shutdown: &kres_core::Shutdown,
) -> Result<(
    crate::change_survey::AggregateTargetDiff,
    Option<ChangeSurveyReport>,
    Option<ChangeSurveyCheckpointStore>,
)> {
    let cutoff = chrono::Utc::now()
        .checked_sub_months(chrono::Months::new(6))
        .context("computing six-month change-survey cutoff")?
        .timestamp();
    let diff_workspace = workspace.to_path_buf();
    let diff_target = target.to_string();
    let window = tokio::task::spawn_blocking(move || {
        crate::change_survey::aggregate_target_diff(&diff_workspace, &diff_target, cutoff)
    })
    .await
    .context("joining gix six-month target diff")??;
    let target_path = if Path::new(target).is_absolute() {
        PathBuf::from(target)
    } else {
        workspace.join(target)
    };
    let target_source = std::fs::read_to_string(&target_path)
        .with_context(|| format!("reading whole-file review target {}", target_path.display()))?;
    let checkpoint = if let Some(path) = checkpoint_path {
        Some(ChangeSurveyCheckpointStore::open(
            path,
            target_path.to_string_lossy().into_owned(),
            change_survey_source_hash(&target_path, &target_source)?,
            window.baseline.clone(),
            window.head.clone(),
            reuse_checkpoint,
        )?)
    } else {
        None
    };
    let mut report = if let Some(checkpoint) = &checkpoint {
        checkpoint.report().await
    } else {
        None
    };
    if report.is_some() {
        kres_core::async_eprintln!(
            "[change survey] resumed completed six-month net-diff assessment"
        );
    } else {
        kres_core::async_eprintln!(
            "[change survey] generated {}-byte target-file diff from {} to {}",
            window.diff.len(),
            window.baseline,
            window.head
        );
        report = assess_change_survey(runner, target, &target_source, &window, shutdown).await?;
        if let (Some(checkpoint), Some(report)) = (&checkpoint, &report) {
            checkpoint.record(report.clone()).await?;
        }
    }
    Ok((window, report, checkpoint))
}

async fn infer_fallback_file_survey_inventory(
    runner: &Arc<AgentRunner>,
    workspace: &Path,
    target: &str,
    fallback_context: &[serde_json::Value],
    shutdown: &kres_core::Shutdown,
) -> Result<FileSurveyInventory> {
    let target_path = if Path::new(target).is_absolute() {
        PathBuf::from(target)
    } else {
        workspace.join(target)
    };
    let target_source = std::fs::read_to_string(&target_path)
        .with_context(|| format!("reading whole-file review target {}", target_path.display()))?;
    let ctags_functions = ctags_function_inventory(&target_path)?;
    let ctags_context = serde_json::to_string(&ctags_functions)
        .context("serializing deterministic fallback function inventory")?;
    let fallback_context = serde_json::to_string(fallback_context)
        .context("serializing local file-survey fallback evidence")?;
    if target_source.len() > CHANGE_SURVEY_DIFF_PARTITION_BYTES {
        return infer_fallback_file_survey_chunks(
            runner,
            target,
            &target_source,
            &ctags_functions,
            &ctags_context,
            &fallback_context,
            shutdown,
        )
        .await;
    }
    let prompt = format!(
        "Semcode file_survey was unavailable for {target}. Build the typed structural inventory from the complete current source below, using the preserved local fallback matches as supporting evidence. Return exactly one raw JSON object {{\"functions\":[{{\"name\":string}}],\"calls\":[string]}}. `functions` must list every function defined in the target. Do not report use counts; Rust computes them from source. `calls` must list every function named by a call expression in the target; retain qualified/member call spellings when present. Do not include declarations or referenced-only names in `functions`. Every name in CTAGS FUNCTION FLOOR is known to be defined and must be present; inspect the complete source for macro-shaped definitions ctags may miss. No markdown or prose outside JSON.\n\nCTAGS FUNCTION FLOOR:\n{ctags_context}\n\nLOCAL FALLBACK EVIDENCE:\n{fallback_context}\n\nCURRENT TARGET FILE ({target}):\n{target_source}"
    );
    let mut errors = Vec::new();
    for attempt in 1..=2 {
        let retry_prompt;
        let attempt_prompt = if let Some(previous_error) = errors.last() {
            retry_prompt = format!(
                "{prompt}\n\nYour previous response failed validation: {previous_error}\nReturn a corrected complete JSON object."
            );
            retry_prompt.as_str()
        } else {
            prompt.as_str()
        };
        let response = runner
            .run_primary_slow_inference(
                "You produce a typed structural inventory for one source file. Follow the requested JSON schema exactly and do not emit markdown or commentary.",
                attempt_prompt,
                &format!("fallback-file-survey {target} attempt {attempt}"),
                shutdown,
            )
            .await;
        match response {
            Ok(response) => match serde_json::from_str::<InferredFileSurveyInventory>(&response)
                .context("fallback file survey response is not raw JSON")
                .and_then(|inventory| {
                    let inventory =
                        FileSurveyInventory::try_from_inferred(inventory, &target_source)?;
                    inventory.validate_fallback(&ctags_functions)?;
                    Ok(inventory)
                }) {
                Ok(inventory) => return Ok(inventory),
                Err(error) => errors.push(format!("attempt {attempt}: {error}")),
            },
            Err(error) if shutdown.is_cancelled() => {
                anyhow::bail!("cancelled during fallback file survey: {error}")
            }
            Err(error) => errors.push(format!("attempt {attempt}: {error}")),
        }
    }
    anyhow::bail!(
        "fallback file survey failed after retries: {}",
        errors.join("; ")
    )
}

async fn infer_fallback_file_survey_chunks(
    runner: &Arc<AgentRunner>,
    target: &str,
    target_source: &str,
    ctags_functions: &BTreeSet<String>,
    ctags_context: &str,
    fallback_context: &str,
    shutdown: &kres_core::Shutdown,
) -> Result<FileSurveyInventory> {
    const SOURCE_OVERLAP_BYTES: usize = 4096;
    let chunks = split_source_for_inference(target_source, CHANGE_SURVEY_DIFF_PARTITION_BYTES)?;
    let count = chunks.len();
    let cached_prefix = format!(
        "Build a sparse structural inventory from one lossless source partition of {target}. Return exactly one raw JSON object {{\"functions\":[{{\"name\":string}}],\"calls\":[string]}}. List every function definition visible in the supplied chunk and every function call expression visible in it. Adjacent chunks overlap for syntax context; duplicates are merged by Rust. Do not report use counts; Rust computes whole-file spelling counts itself. Do not invent definitions merely because CTAGS FUNCTION FLOOR names them. No markdown or prose outside JSON.\n\nCTAGS FUNCTION FLOOR:\n{ctags_context}\n\nLOCAL FALLBACK EVIDENCE:\n{fallback_context}\n\n"
    );
    kres_core::async_eprintln!(
        "[file survey] fallback source is large; inventorying {} lossless chunk(s)",
        count
    );
    let reports = stream::iter(chunks.iter().enumerate().map(|(index, chunk)| {
        let mut context_start = chunk.source_start.saturating_sub(SOURCE_OVERLAP_BYTES);
        while context_start < chunk.source_start && !target_source.is_char_boundary(context_start) {
            context_start += 1;
        }
        let source = &target_source[context_start..chunk.source_end];
        let prompt_tail = format!(
            "SOURCE CHUNK: {}/{}\n\nCURRENT TARGET FILE CHUNK:\n{source}",
            index + 1,
            count,
        );
        let cached_prefix = cached_prefix.as_str();
        async move {
            let mut errors = Vec::new();
            for attempt in 1..=2 {
                if shutdown.is_cancelled() {
                    anyhow::bail!("cancelled during fallback file survey");
                }
                let retry;
                let attempt_tail = if let Some(error) = errors.last() {
                    retry = format!(
                        "{prompt_tail}\n\nYour previous response failed validation: {error}\nReturn corrected raw JSON."
                    );
                    retry.as_str()
                } else {
                    prompt_tail.as_str()
                };
                let response = runner
                    .run_primary_slow_inference_low_effort(
                        "You produce a sparse typed structural inventory for one source-file chunk. Follow the requested JSON schema exactly and do not emit markdown or commentary.",
                        cached_prefix,
                        attempt_tail,
                        true,
                        &format!("fallback-file-survey {target} chunk {}/{} attempt {attempt}", index + 1, count),
                        shutdown,
                    )
                    .await;
                match response {
                    Ok(response) => match serde_json::from_str::<InferredFileSurveyInventory>(&response)
                        .context("fallback file survey chunk is not raw JSON")
                        .and_then(|inventory| {
                            FileSurveyInventory::try_from_inferred(inventory, target_source)
                        })
                    {
                        Ok(inventory) => return Ok(inventory),
                        Err(error) => errors.push(error.to_string()),
                    },
                    Err(error) if shutdown.is_cancelled() => {
                        anyhow::bail!("cancelled during fallback file survey: {error}")
                    }
                    Err(error) => errors.push(error.to_string()),
                }
            }
            anyhow::bail!(errors.join("; "))
        }
    }))
    .buffer_unordered(CHANGE_SURVEY_CHUNK_CONCURRENCY)
    .collect::<Vec<_>>()
    .await
    .into_iter()
    .collect::<Result<Vec<_>>>()?;

    let mut names = ctags_functions.clone();
    let mut calls = BTreeSet::new();
    for report in reports {
        names.extend(report.functions.into_keys());
        calls.extend(report.calls);
    }
    let inventory = FileSurveyInventory {
        functions: names
            .into_iter()
            .map(|name| {
                let uses = identifier_occurrences(target_source, &name);
                (name, uses)
            })
            .collect(),
        calls: calls.into_iter().collect(),
    };
    inventory.validate_fallback(ctags_functions)?;
    Ok(inventory)
}

/// Rate the target's functions against the six-month net diff.
///
/// A broad first guess, not an inventory. It reads a diff and returns
/// ratings; whatever comes back is what we use. Unknown names are
/// dropped by the caller and unmentioned functions stay unrated.
///
/// It used to be held to the authoritative function set, and that was
/// wrong three separate ways on kernel/sched/fair.c (421 functions):
/// demanding an exact roster produced an invented
/// `__account_cfs_rq_runtime_placeholder`; demanding exactness per
/// 150-name batch threw away 147 correct ratings for being three
/// short; demanding each batch stay inside its own slice rejected
/// `__min_slice_update` and `detach_tasks`, real functions the model
/// rated unprompted. Each failure killed the entire review bootstrap
/// over a heuristic. Do not reintroduce a coverage contract here.
async fn assess_change_survey(
    runner: &Arc<AgentRunner>,
    target: &str,
    target_source: &str,
    window: &crate::change_survey::AggregateTargetDiff,
    shutdown: &kres_core::Shutdown,
) -> Result<Option<ChangeSurveyReport>> {
    if shutdown.is_cancelled() {
        anyhow::bail!("cancelled during whole-file change survey");
    }
    if window.diff.len().saturating_add(target_source.len()) <= CHANGE_SURVEY_DIFF_PARTITION_BYTES {
        let prompt = change_survey_prompt(target, target_source, window, None);
        return infer_change_survey_prompt(
            runner,
            &prompt,
            ChangeSurveyWindowId {
                baseline: &window.baseline,
                head: &window.head,
            },
            ChangeSurveyCall {
                task_kind: "change-survey net-diff",
                cache_prefix: false,
            },
            shutdown,
        )
        .await
        .map(Some);
    }

    // Too large for one call: partition so the INPUT fits, then union
    // the partitions in Rust. No model reassembles the result.
    let chunks = split_diff_for_inference(&window.diff, CHANGE_SURVEY_PAIR_PARTITION_BYTES)?;
    let source_chunks = if target_source
        .len()
        .saturating_add(CHANGE_SURVEY_PAIR_PARTITION_BYTES)
        <= CHANGE_SURVEY_DIFF_PARTITION_BYTES
    {
        vec![crate::change_survey::PreparedDiffChunk {
            text: target_source.to_string(),
            source_start: 0,
            source_end: target_source.len(),
        }]
    } else {
        split_source_for_inference(target_source, CHANGE_SURVEY_PAIR_PARTITION_BYTES)?
    };
    let chunk_count = chunks.len();
    let source_count = source_chunks.len();
    kres_core::async_eprintln!(
        "[change survey] target-file input is large ({} diff bytes, {} source bytes); assessing {} source scope(s) against {} diff chunk(s) in {} semantic call(s), concurrency {}",
        window.diff.len(),
        target_source.len(),
        source_count,
        chunk_count,
        chunk_count.saturating_mul(source_count),
        CHANGE_SURVEY_CHUNK_CONCURRENCY,
    );
    let pairs: Vec<(usize, usize)> = (0..source_count)
        .flat_map(|source_index| (0..chunk_count).map(move |diff_index| (source_index, diff_index)))
        .collect();
    let reports = stream::iter(pairs.into_iter().map(|(source_index, diff_index)| {
        let prompt = change_survey_chunk_prompt(
            target,
            window,
            None,
            Some(ChangeSurveySourceChunk {
                text: &source_chunks[source_index].text,
                index: source_index,
                count: source_count,
            }),
            Some(ChangeSurveyDiffChunk {
                text: &chunks[diff_index].text,
                index: diff_index,
                count: chunk_count,
            }),
        );
        let label = format!(
            "change-survey source {}/{} diff {}/{}",
            source_index + 1,
            source_count,
            diff_index + 1,
            chunk_count
        );
        async move {
            infer_change_survey_prompt(
                runner,
                &prompt,
                ChangeSurveyWindowId {
                    baseline: &window.baseline,
                    head: &window.head,
                },
                ChangeSurveyCall {
                    task_kind: &label,
                    cache_prefix: true,
                },
                shutdown,
            )
            .await
        }
    }))
    .buffer_unordered(CHANGE_SURVEY_CHUNK_CONCURRENCY)
    .collect::<Vec<_>>()
    .await
    .into_iter()
    .collect::<Result<Vec<_>>>()?;
    Ok(Some(crate::change_survey::merge_change_survey_reports(
        &window.baseline,
        &window.head,
        reports,
    )))
}

/// Identity of the six-month window a change-survey call is assessing.
/// Carried together because every call needs both halves and neither is
/// meaningful alone.
#[derive(Clone, Copy)]
struct ChangeSurveyWindowId<'a> {
    baseline: &'a str,
    head: &'a str,
}

/// How one change-survey inference call is issued.
#[derive(Clone, Copy)]
struct ChangeSurveyCall<'a> {
    task_kind: &'a str,
    /// True only when sibling calls reuse these exact prefix bytes. A cache
    /// write costs more than plain input, so a single-use prefix must not be
    /// marked.
    cache_prefix: bool,
}

async fn infer_change_survey_prompt(
    runner: &Arc<AgentRunner>,
    prompt: &crate::change_survey::ChangeSurveyPrompt,
    window: ChangeSurveyWindowId<'_>,
    call: ChangeSurveyCall<'_>,
    shutdown: &kres_core::Shutdown,
) -> Result<ChangeSurveyReport> {
    let ChangeSurveyWindowId { baseline, head } = window;
    let ChangeSurveyCall {
        task_kind,
        cache_prefix,
    } = call;
    let mut errors = Vec::new();
    for attempt in 1..=2 {
        if shutdown.is_cancelled() {
            anyhow::bail!("cancelled during whole-file change survey");
        }
        let retry_tail;
        let attempt_tail = if let Some(previous_error) = errors.last() {
            retry_tail = format!(
                "{}\n\nYour previous response failed validation: {previous_error}\nReturn a corrected complete JSON object.",
                prompt.tail
            );
            retry_tail.as_str()
        } else {
            prompt.tail.as_str()
        };
        let response = runner
            .run_primary_slow_inference_low_effort(
                "You quickly classify code-change risk from one six-month net target-file diff. Judge the final code, use low reasoning effort, keep reasons terse, follow the requested JSON schema exactly, and do not emit markdown or commentary.",
                &prompt.cached_prefix,
                attempt_tail,
                cache_prefix,
                &format!("{task_kind} attempt {attempt}"),
                shutdown,
            )
            .await;
        match response {
            Ok(response) => {
                // Only the parse can fail. Coverage is not a contract:
                // unknown names are dropped by the caller and
                // unmentioned functions stay unrated.
                match parse_inference_risks(&response, baseline, head) {
                    Ok(rating) => return Ok(rating),
                    Err(error) => errors.push(format!("attempt {attempt}: {error}")),
                }
            }
            Err(error) if shutdown.is_cancelled() => {
                return Err(anyhow::anyhow!(error.to_string()));
            }
            Err(error) => errors.push(format!("attempt {attempt}: {error}")),
        }
    }
    anyhow::bail!(errors.join("; "))
}

/// Priority stamped on every Rust-derived external research question.
///
/// The retired file-survey prompt asked the model for an integer in
/// 80-100 per question and gave it nothing to discriminate on — every
/// retained entry had already passed the same Rust interaction filter.
/// A constant says that plainly instead of dressing it up as judgement.
const EXTERNAL_RESEARCH_PRIORITY: u8 = 90;

/// Serialized shape of the completed scan. Same fields the agents have always
/// seen, with `uses` and now the ratings supplied by Rust rather than echoed
/// by the model.
#[derive(Debug, Serialize)]
struct ScanFunctionRisk<'a> {
    name: &'a str,
    uses: u64,
    risk_rating: u8,
}

#[derive(Debug, Serialize)]
struct ScanFileSurvey<'a> {
    functions: Vec<ScanFunctionRisk<'a>>,
    research_questions: &'a [ReviewResearchQuestion],
    file_risk_rating: u8,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewResearchQuestion {
    function: String,
    file: String,
    question: String,
    priority: u8,
}

#[derive(Debug)]
struct FileSurveyInventory {
    functions: std::collections::BTreeMap<String, u64>,
    calls: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InferredFileSurveyInventory {
    functions: Vec<InferredFunctionInventory>,
    calls: Vec<String>,
}

/// A function definition reported by the fallback inventory inference.
///
/// Deliberately has no `uses` field. Rust computes spelling counts from source
/// with `identifier_occurrences`, so asking the model to reproduce them added
/// nothing and turned a single wrong integer into a whole-response rejection.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InferredFunctionInventory {
    name: String,
}

impl FileSurveyInventory {
    fn from_context(context: &[serde_json::Value]) -> Option<Self> {
        let survey = context
            .iter()
            .find_map(|item| item.get("result").filter(|result| result.is_object()))?;
        let function_entries = survey.get("functions_defined")?.as_array()?;
        let parsed_functions = function_entries
            .iter()
            .map(|entry| {
                let entry = entry.as_array()?;
                Some((
                    entry.first()?.as_str()?.to_string(),
                    entry.get(1)?.as_u64()?,
                ))
            })
            .collect::<Option<Vec<_>>>()?;
        // A function may be defined more than once in one translation unit
        // under mutually exclusive preprocessor branches — mm/vmscan.c:204 and
        // :249 both define `cgroup_reclaim` across the `#ifdef CONFIG_MEMCG`
        // at :201, and twelve other pairs do the same. Rejecting the whole
        // survey for that discarded a perfectly good inventory and forced an
        // expensive inference fallback on most kernel files. Keep the highest
        // reported count per name instead; Rust recomputes the authoritative
        // count from source anyway.
        let mut functions: std::collections::BTreeMap<String, u64> =
            std::collections::BTreeMap::new();
        for (name, uses) in parsed_functions {
            let slot = functions.entry(name).or_insert(0);
            *slot = (*slot).max(uses);
        }
        let calls = survey
            .get("calls")?
            .as_array()?
            .iter()
            .map(|entry| entry.as_array()?.first()?.as_str().map(str::to_string))
            .collect::<Option<Vec<_>>>()?;
        if functions.is_empty() {
            return None;
        }
        Some(Self { functions, calls })
    }

    fn try_from_inferred(
        inferred: InferredFileSurveyInventory,
        target_source: &str,
    ) -> Result<Self> {
        if inferred
            .functions
            .iter()
            .map(|function| function.name.as_str())
            .chain(inferred.calls.iter().map(String::as_str))
            .any(|name| name.trim().is_empty())
        {
            anyhow::bail!("fallback file survey returned an empty function name");
        }
        // Rust owns the counts. The model supplies only the names it can see.
        let functions: std::collections::BTreeMap<String, u64> = inferred
            .functions
            .into_iter()
            .map(|function| {
                let uses = identifier_occurrences(target_source, &function.name);
                (function.name, uses)
            })
            .collect();
        Ok(Self {
            functions,
            calls: inferred.calls,
        })
    }

    fn function_names(&self) -> BTreeSet<String> {
        self.functions.keys().cloned().collect()
    }

    /// The model is only trusted for the set of names it can see. Counts are
    /// computed by Rust, so there is nothing left to disagree about.
    fn validate_fallback(&self, ctags_functions: &BTreeSet<String>) -> Result<()> {
        let names = self.function_names();
        if let Some(missing) = ctags_functions.difference(&names).next() {
            anyhow::bail!("fallback file survey omitted ctags function {missing}");
        }
        Ok(())
    }

    fn calls_function(&self, function: &str) -> bool {
        let Some(function) = terminal_identifier(function) else {
            return false;
        };
        self.calls.iter().any(|call| {
            call.split(|character: char| !(character.is_alphanumeric() || character == '_'))
                .any(|token| token == function)
        })
    }

    fn interaction_kind(&self, function: &str, target_source: &str) -> Option<&'static str> {
        let terminal = terminal_identifier(function)?;
        // A target-local definition shadows a same-named external function in
        // this translation unit. Keep the external risk in the survey report,
        // but do not manufacture a research question from calls that resolve
        // to the local definition.
        if self.functions.contains_key(terminal) {
            return None;
        }
        if self.calls_function(function) {
            return Some("call");
        }
        source_references_function_value(target_source, terminal)
            .then_some("function_value_reference")
    }
}

fn ctags_function_inventory(target: &Path) -> Result<BTreeSet<String>> {
    let output = match std::process::Command::new("ctags")
        .args([
            "--output-format=json",
            "--languages=C",
            "--kinds-C=f",
            "--sort=no",
            "-o",
            "-",
        ])
        .arg(target)
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(error) => return Err(error).context("running ctags fallback inventory"),
    };
    if !output.status.success() {
        kres_core::async_eprintln!(
            "[file survey] optional ctags inventory unavailable: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        return Ok(BTreeSet::new());
    }
    let Ok(stdout) = String::from_utf8(output.stdout) else {
        kres_core::async_eprintln!(
            "[file survey] optional ctags inventory returned non-UTF-8 output"
        );
        return Ok(BTreeSet::new());
    };
    let mut functions = BTreeSet::new();
    for line in stdout.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            kres_core::async_eprintln!(
                "[file survey] optional ctags inventory returned non-JSON output"
            );
            return Ok(BTreeSet::new());
        };
        if value.get("kind").and_then(serde_json::Value::as_str) == Some("function") {
            let Some(name) = value.get("name").and_then(serde_json::Value::as_str) else {
                continue;
            };
            functions.insert(name.to_string());
        }
    }
    Ok(functions)
}

fn identifier_occurrences(source: &str, identifier: &str) -> u64 {
    let code = code_without_comments_and_literals(source);
    let identifier = identifier.as_bytes();
    if identifier.is_empty() {
        return 0;
    }
    code.split(|byte| !is_identifier_byte(*byte))
        .filter(|token| *token == identifier)
        .count() as u64
}

fn terminal_identifier(name: &str) -> Option<&str> {
    name.split(|character: char| !(character.is_alphanumeric() || character == '_'))
        .rfind(|token| !token.is_empty())
}

fn source_references_function_value(source: &str, identifier: &str) -> bool {
    let code = code_without_comments_and_literals(source);
    let identifier = identifier.as_bytes();
    if identifier.is_empty() {
        return false;
    }
    let mut offset = 0;
    while let Some(relative) = code[offset..]
        .windows(identifier.len())
        .position(|window| window == identifier)
    {
        let start = offset + relative;
        let end = start + identifier.len();
        offset = end;
        let token_start = start == 0 || !is_identifier_byte(code[start - 1]);
        let token_end = end == code.len() || !is_identifier_byte(code[end]);
        if !token_start || !token_end {
            continue;
        }
        let next = code[end..]
            .iter()
            .position(|byte| !byte.is_ascii_whitespace())
            .map(|index| end + index);
        let next_byte = next.map(|index| code[index]);

        // A following '(' is either a call (covered by the structured call
        // inventory) or a declaration/definition, never callback evidence.
        if next_byte == Some(b'(') {
            continue;
        }
        // The external name is not a target-local definition (checked by the
        // caller), and declarations/prototypes put '(' after the identifier.
        // Every remaining code occurrence is conservatively an interaction:
        // assignment, address-taking, cast, ternary, initializer, macro
        // argument, return value, or typeof-style reference.
        return true;
    }
    false
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn code_without_comments_and_literals(source: &str) -> Vec<u8> {
    let input = source.as_bytes();
    let mut output = input.to_vec();
    let mut index = 0;
    while index < input.len() {
        if input[index..].starts_with(b"//") {
            let end = input[index..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(input.len(), |relative| index + relative);
            output[index..end].fill(b' ');
            index = end;
        } else if input[index..].starts_with(b"/*") {
            let start = index;
            index += 2;
            let mut depth = 1usize;
            while index < input.len() && depth > 0 {
                if input[index..].starts_with(b"/*") {
                    depth += 1;
                    index += 2;
                } else if input[index..].starts_with(b"*/") {
                    depth -= 1;
                    index += 2;
                } else {
                    index += 1;
                }
            }
            output[start..index].fill(b' ');
        } else if input[index] == b'"' || is_character_literal_start(input, index) {
            let quote = input[index];
            let start = index;
            index += 1;
            while index < input.len() {
                if input[index] == b'\\' {
                    index = (index + 2).min(input.len());
                } else {
                    let byte = input[index];
                    index += 1;
                    if byte == quote || byte == b'\n' {
                        break;
                    }
                }
            }
            output[start..index].fill(b' ');
        } else {
            index += 1;
        }
    }
    output
}

fn is_character_literal_start(input: &[u8], index: usize) -> bool {
    if input[index] != b'\'' {
        return false;
    }
    input[index + 1..]
        .iter()
        .take(12)
        .take_while(|byte| **byte != b'\n')
        .position(|byte| *byte == b'\'')
        .is_some()
}

async fn ensure_review_followups_remain_pending(
    mgr: &Arc<TaskManager>,
    followups: &[serde_json::Value],
) {
    if mgr.todo_snapshot().await.iter().any(|t| {
        matches!(
            t.status,
            kres_core::TodoStatus::Pending | kres_core::TodoStatus::Blocked
        )
    }) {
        return;
    }

    let added =
        add_followups_as_pending(mgr, followups, "review followup emitted by slow agent").await;
    if added > 0 {
        kres_core::async_eprintln!(
            "[todo update] restored {added} review followup(s) as pending next-turn work"
        );
    }
}

async fn add_followups_as_pending(
    mgr: &Arc<TaskManager>,
    followups: &[serde_json::Value],
    default_reason: &str,
) -> usize {
    let mut candidates = Vec::new();
    for fu in followups {
        let kind = fu
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("question")
            .to_string();
        let name = fu
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let reason = fu
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if name.trim().is_empty() {
            continue;
        }
        let mut item = kres_core::TodoItem::new(name, kind);
        item.reason = if reason.is_empty() {
            default_reason.to_string()
        } else {
            reason
        };
        if let Some(path) = fu.get("path").and_then(|v| v.as_str()) {
            if !path.is_empty() {
                item.coverage = format!("path: {path}");
            }
        }
        candidates.push(item);
    }
    mgr.append_todo_unique(candidates).await
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
/// legacy `publish-fix` followup. Workflow-owned `/fix` uses the
/// deterministic workflow reaper path instead. The argument is the absolute path
/// to a finding directory (the kres --export shape with
/// `metadata.yaml`, `FINDING.md`, `summary.md`).
///
/// On success the directory gains:
/// - `auto-generated-fix.diff` — the output of
///   `git format-patch -1 --stdout HEAD` from the workspace.
/// - `metadata.yaml` records the patch under `auto_generated_fixes:`
///   (idempotent — skipped if already present).
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

    let fix_path = dir.join(kres_core::AUTO_GENERATED_FIX_NAME);

    // Skip when auto-generated-fix.diff already records the current
    // HEAD. `git format-patch -1 --stdout HEAD` opens
    // each patch with `From <40-hex-sha> Mon Sep 17 00:00:00 2001`,
    // so comparing that prefix to `git rev-parse HEAD` is enough to
    // detect "already published this commit". A real amend changes
    // HEAD's sha and falls through to the rewrite path.
    if let Some(head_sha) = git_rev_parse_head(workspace).await {
        if kres_core::patch_file_matches_head(&dir, &head_sha).unwrap_or(false) {
            if let Err(e) = kres_core::clear_invalidation_artifacts(&dir) {
                return format!(
                    "[publish-fix FAILED] clear invalidation artifacts in {}: {e}",
                    dir.display()
                );
            }
            return format!(
                "[publish-fix] {} already up to date for HEAD {}",
                fix_path.display(),
                &head_sha[..12.min(head_sha.len())],
            );
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

    if let Err(e) = kres_core::record_auto_generated_fix(&dir) {
        return format!(
            "[publish-fix FAILED] record auto-generated fix in {}: {e}",
            dir.display()
        );
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
                    kres_core::findings::Status::Unconfirmed => "unconfirmed".to_string(),
                    kres_core::findings::Status::Fixed => "fixed".to_string(),
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

fn recorded_findings_goal_context(findings: &[kres_core::Finding]) -> String {
    let active: Vec<_> = findings
        .iter()
        .filter(|f| f.status == kres_core::findings::Status::Active)
        .collect();
    if active.is_empty() {
        return String::new();
    }

    let mut out = String::from("## Recorded findings in findings.json\n\n");
    for f in active {
        let locs = f
            .relevant_symbols
            .iter()
            .map(|s| format!("{}:{}", s.filename, s.line))
            .chain(
                f.relevant_file_sections
                    .iter()
                    .map(|s| format!("{}:{}-{}", s.filename, s.line_start, s.line_end)),
            )
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!(
            "- id: `{}`; title: {}; severity: {:?}; status: {:?}; locations: {}; summary: {}\n",
            f.id,
            f.title,
            f.severity,
            f.status,
            if locs.is_empty() { "(none)" } else { &locs },
            f.summary
        ));
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_response_requires_one_exact_json_object() {
        assert_eq!(
            parse_compact_response(r#"{"summary":"kept"}"#).as_deref(),
            Some("kept")
        );
        assert!(parse_compact_response("prose {\"summary\":\"hidden\"}").is_none());
        assert!(parse_compact_response(r#"{"summary":"","extra":true}"#).is_none());
        assert!(parse_compact_response(r#"{"summary":"   "}"#).is_none());
    }

    #[tokio::test]
    async fn session_without_agent_runner_drops_prompt() {
        let mgr = TaskManager::new();
        let s = Session::new(mgr, ReplConfig::default()).await;
        // We can't easily exercise submit_prompt from a unit test
        // without stdin plumbing, but we can assert construction
        // leaves `agent_runner` unset.
        assert!(s.agent_runner.is_none());
    }

    #[tokio::test]
    async fn terminal_unreaped_tasks_no_longer_block_dispatch() {
        // Superseded contract: this used to assert that a
        // terminal-but-unreaped task blocked auto-continue. Waiting
        // for the reap queue to drain serialised every new task behind
        // a ~65s publication, so the bound is now a start budget
        // instead — see `dispatch_stops_after_max_parallel_starts`.
        let mgr = TaskManager::new();
        assert!(
            mgr.seed_todo_if_empty(vec![kres_core::TodoItem::new("next", "review")])
                .await
        );
        let s = Session::new(mgr.clone(), ReplConfig::default()).await;
        mgr.spawn("finished but unreaped", None, |_| async {
            Ok(kres_core::task::TaskOutcome {
                analysis: "done".to_string(),
                ..Default::default()
            })
        })
        .await;

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while mgr.reap_queue_depth().await == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("task should reach a terminal state");

        assert_eq!(mgr.active_count().await, 0);
        assert!(
            s.should_auto_continue().await,
            "an unpublished terminal task must no longer hold back dispatch"
        );
    }

    #[tokio::test]
    async fn auto_continue_no_longer_waits_for_running_tasks() {
        // The point of the rework: a running task is not a reason to
        // hold back work. Only an unpublished completed one is.
        let mgr = TaskManager::new();
        assert!(
            mgr.seed_todo_if_empty(vec![kres_core::TodoItem::new("next", "review")])
                .await
        );
        let s = Session::new(mgr.clone(), ReplConfig::default()).await;
        let hold = Arc::new(tokio::sync::Notify::new());
        let held = hold.clone();
        mgr.spawn("still running", None, move |_| async move {
            held.notified().await;
            Ok(kres_core::task::TaskOutcome::default())
        })
        .await;
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while mgr.active_count().await == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("task should start");

        assert_eq!(mgr.reap_queue_depth().await, 0);
        assert!(
            s.should_auto_continue().await,
            "a running task must not block dispatch once the reap queue is empty"
        );
        hold.notify_waiters();
    }

    #[tokio::test]
    async fn dispatch_stops_after_max_parallel_starts_without_a_reap() {
        // Dispatch may run during a reap, but not without limit: the
        // reaper shares one rate limiter with the tasks, and a stream
        // of fast tasks would otherwise keep starting work while the
        // reaper never got a turn to publish any of it.
        //
        // A live task is held open for the duration: the budget
        // deliberately does not block when NO task is tracked, since
        // nothing could ever re-arm it, and that guard would otherwise
        // mask what this test is checking.
        let mgr = TaskManager::with_max_parallel(4);
        let hold = Arc::new(tokio::sync::Notify::new());
        let held = hold.clone();
        mgr.spawn("keeps the task list non-empty", None, move |_| async move {
            held.notified().await;
            Ok(kres_core::task::TaskOutcome::default())
        })
        .await;
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while mgr.active_count().await == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("holder should start");
        mgr.seed_todo_if_empty(
            (0..9)
                .map(|i| kres_core::TodoItem::new(format!("row {i}"), "review"))
                .collect(),
        )
        .await;
        let s = Session::new(mgr.clone(), ReplConfig::default()).await;

        // Cap 4, one slot taken by the holder: 3 free, budget 4.
        // Claimed rows never spawn (no AgentRunner), so free_slots
        // stays at 3 and only the budget can stop the sequence.
        assert_eq!(s.dispatch_ready(None, "test").await.dispatched, 3);
        assert_eq!(s.dispatch_ready(None, "test").await.dispatched, 1);
        let blocked = s.dispatch_ready(None, "test").await;
        assert_eq!(blocked.dispatched, 0);
        assert!(
            blocked
                .refused
                .as_deref()
                .is_some_and(|r| r.contains("since the last reap completed")),
            "expected a start-budget refusal, got {:?}",
            blocked.refused
        );

        // A completed reap batch re-arms it, and only that does.
        mgr.note_reap_completed().await;
        assert_eq!(s.dispatch_ready(None, "test").await.dispatched, 3);
        hold.notify_waiters();
    }

    #[tokio::test]
    async fn dispatch_is_refused_when_every_slot_is_busy() {
        // The cap lives on the manager, so the test configures it
        // there — there is no second copy to disagree with.
        let mgr = TaskManager::with_max_parallel(1);
        mgr.seed_todo_if_empty(vec![kres_core::TodoItem::new("work", "review")])
            .await;
        let s = Session::new(mgr.clone(), ReplConfig::default()).await;
        let hold = Arc::new(tokio::sync::Notify::new());
        let held = hold.clone();
        mgr.spawn("occupies the only slot", None, move |_| async move {
            held.notified().await;
            Ok(kres_core::task::TaskOutcome::default())
        })
        .await;
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while mgr.active_count().await == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("task should start");

        assert_eq!(mgr.free_slots().await, 0);
        let outcome = s.dispatch_ready(None, "test").await;
        assert!(
            outcome
                .refused
                .as_deref()
                .is_some_and(|r| r.contains("slot(s) busy")),
            "expected a slot refusal, got {:?}",
            outcome.refused
        );
        assert!(
            !s.should_auto_continue().await,
            "a full fleet must also stop the idle loop from firing"
        );
        hold.notify_waiters();
    }

    #[tokio::test]
    async fn ranking_refresh_stops_when_no_further_work_will_start() {
        use std::sync::atomic::Ordering;
        let s = Session::new(TaskManager::new(), ReplConfig::default()).await;
        assert!(s.ranking_refresh_allowed());
        // /stop must skip every inference-heavy reaper post-step, and
        // the refill signal still fires for a batch reaped just before
        // the latch — so the guard, not the caller, has to hold.
        s.stop_latched.store(true, Ordering::Release);
        assert!(!s.ranking_refresh_allowed());
        s.stop_latched.store(false, Ordering::Release);
        assert!(s.ranking_refresh_allowed());
        // Past the turns cap nothing more will be dispatched, so a
        // ranking has nothing to order.
        s.turns_cap_reached.store(true, Ordering::Release);
        assert!(!s.ranking_refresh_allowed());
    }

    #[tokio::test]
    async fn auto_continue_does_not_resurrect_deferred_work() {
        // /continue pulls the deferred ledger back because the
        // operator asked. The idle timer has not asked, and it now
        // fires while tasks are still running, so it must not undo a
        // goal-met or turn-cap drain on a timeout.
        let mgr = TaskManager::new();
        mgr.seed_todo_if_empty(vec![kres_core::TodoItem::new("deferred work", "review")])
            .await;
        assert_eq!(mgr.defer_pending().await, 1);
        assert_eq!(mgr.deferred_snapshot().await.len(), 1);
        let s = Session::new(mgr.clone(), ReplConfig::default()).await;

        s.auto_continue().await;
        assert_eq!(
            mgr.deferred_snapshot().await.len(),
            1,
            "idle auto-continue must leave the deferred ledger alone"
        );
        assert!(mgr.todo_snapshot().await.is_empty());

        s.cmd_continue().await;
        assert!(
            mgr.deferred_snapshot().await.is_empty(),
            "the operator's /continue still pulls deferred work back"
        );
        assert_eq!(mgr.todo_snapshot().await.len(), 1);
    }

    #[tokio::test]
    async fn auto_continue_does_not_clear_the_stop_latch() {
        use std::sync::atomic::Ordering;
        let mgr = TaskManager::new();
        let s = Session::new(mgr, ReplConfig::default()).await;
        s.stop_latched.store(true, Ordering::Release);
        s.auto_continue().await;
        assert!(
            s.stop_latched.load(Ordering::Acquire),
            "only the operator re-arms dispatch after /stop"
        );
        s.cmd_continue().await;
        assert!(!s.stop_latched.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn pipeline_submission_does_not_replace_operator_interruption_stash() {
        let s = Session::new(TaskManager::new(), ReplConfig::default()).await;

        s.stash_interruptible_prompt("operator prompt", true).await;
        s.stash_interruptible_prompt("pipeline prompt", false).await;

        assert_eq!(
            s.interrupted_prompt.lock().await.as_deref(),
            Some("operator prompt")
        );
    }

    #[tokio::test]
    async fn review_prompt_config_forces_initial_audit_mode() {
        let mgr = TaskManager::new();
        let cfg = crate::workflow::ReviewPromptConfig {
            source: "test".to_string(),
            prompt_file: kres_agents::PromptFile {
                prompt: "review this".to_string(),
                lenses: vec![kres_core::LensSpec {
                    id: "lifetime".to_string(),
                    kind: "review".to_string(),
                    name: "Lifetime".to_string(),
                    reason: "check lifetime".to_string(),
                }],
            },
            consolidate_rules: Some("merge carefully".to_string()),
            file_scan_target: Some("mm/filemap.c".to_string()),
        };
        let s = Session::new(mgr, ReplConfig::default())
            .await
            .with_review_prompt_config(cfg);

        assert_eq!(s.initial_prompt.as_deref(), Some("review this"));
        assert_eq!(s.initial_prompt_mode, Some(kres_agents::TaskMode::Audit));
    }

    #[tokio::test]
    async fn failed_review_submission_restores_config_and_pauses_old_work() {
        let mgr = TaskManager::new();
        let old_lens = kres_core::LensSpec {
            id: "old".to_string(),
            kind: "review".to_string(),
            name: "Old lens".to_string(),
            reason: "old reason".to_string(),
        };
        let old_cfg = crate::workflow::ReviewPromptConfig {
            source: "old".to_string(),
            prompt_file: kres_agents::PromptFile {
                prompt: "old review".to_string(),
                lenses: vec![old_lens.clone()],
            },
            consolidate_rules: Some("old rules".to_string()),
            file_scan_target: Some("old.c".to_string()),
        };
        let session = Session::new(mgr.clone(), ReplConfig::default())
            .await
            .with_review_prompt_config(old_cfg);
        let plan = kres_core::Plan::new("old review", "old goal", kres_core::TaskMode::Audit);
        mgr.set_plan(Some(plan.clone())).await;
        let old_todo = kres_core::TodoItem::new("old todo", "review");
        assert!(mgr.seed_todo_if_empty(vec![old_todo.clone()]).await);
        cache_review_file_scan(
            &mgr,
            &CompletedReviewFileScan {
                target: "old.c".into(),
                source_hash: "old-source".into(),
                baseline: "old-baseline".into(),
                head: "old-head".into(),
                scan: "old scan".into(),
            },
        )
        .await;
        let old_scan = mgr
            .get_cached_context(REVIEW_FILE_SCAN_CACHE_KEY)
            .await
            .unwrap();

        session
            .install_review_config_and_submit(crate::workflow::ReviewPromptConfig {
                source: "new".to_string(),
                prompt_file: kres_agents::PromptFile {
                    prompt: "new review".to_string(),
                    lenses: vec![kres_core::LensSpec {
                        id: "new".to_string(),
                        kind: "review".to_string(),
                        name: "New lens".to_string(),
                        reason: "new reason".to_string(),
                    }],
                },
                consolidate_rules: Some("new rules".to_string()),
                file_scan_target: Some("new.c".to_string()),
            })
            .await;

        assert_eq!(*session.lenses.read().await, vec![old_lens]);
        assert_eq!(
            session.lens_consolidate_rules.read().await.as_deref(),
            Some("old rules")
        );
        assert_eq!(
            session.review_file_scan_target.read().await.as_deref(),
            Some("old.c")
        );
        let restored_plan = mgr.plan_snapshot().await.unwrap();
        assert_eq!(restored_plan.prompt, plan.prompt);
        assert_eq!(restored_plan.goal, plan.goal);
        let restored_todos = mgr.todo_snapshot().await;
        assert_eq!(restored_todos.len(), 1);
        assert_eq!(restored_todos[0].id, old_todo.id);
        assert_eq!(restored_todos[0].name, old_todo.name);
        assert_eq!(
            mgr.get_cached_context(REVIEW_FILE_SCAN_CACHE_KEY).await,
            Some(old_scan)
        );
        assert!(session.stop_latched.load(Ordering::Acquire));
    }

    #[test]
    fn review_plan_steps_seed_linked_pending_todos() {
        let mut plan = kres_core::Plan::new(
            "review: mm/example.c",
            "cover the target",
            kres_core::TaskMode::Audit,
        );
        let mut step = kres_core::PlanStep::new("orient-target", "Map target contracts");
        step.description = "Gather definitions, callers, and history".to_string();
        plan.steps.push(step);
        let mut dependent = kres_core::PlanStep::new("trace-reads", "Trace read contracts");
        dependent.depends_on = vec!["orient-target".to_string()];
        plan.steps.push(dependent);

        let todos = review_todos_from_plan(&plan);
        assert_eq!(todos.len(), 2);
        assert_eq!(todos[0].id, "review-orient-target");
        assert_eq!(todos[0].step_id, "orient-target");
        assert_eq!(todos[0].kind, "review");
        assert_eq!(todos[0].status, kres_core::TodoStatus::Pending);
        assert_eq!(todos[0].reason, "Gather definitions, callers, and history");
        assert_eq!(todos[1].depends_on, vec!["review-orient-target"]);
    }

    #[test]
    fn lensed_review_keeps_pending_followups_after_goal_met() {
        // Something still to do -> next review turn, not a drain.
        assert!(review_followups_drive_next_turn(true, 1, 0));
        assert!(review_followups_drive_next_turn(true, 0, 1));
        // Genuinely nothing left -> drain.
        assert!(!review_followups_drive_next_turn(true, 0, 0));
        // Not a lensed review -> drain regardless of what remains.
        assert!(!review_followups_drive_next_turn(false, 1, 1));
    }

    #[test]
    fn turns_cap_waits_for_active_tasks_before_exit() {
        assert_eq!(turns_cap_action(9, 10, 0), TurnsCapAction::Continue);
        assert_eq!(turns_cap_action(10, 10, 2), TurnsCapAction::DrainAndWait);
        assert_eq!(turns_cap_action(11, 10, 1), TurnsCapAction::DrainAndWait);
        assert_eq!(turns_cap_action(10, 10, 0), TurnsCapAction::DrainAndExit);
    }

    #[tokio::test]
    async fn turns_cap_waits_for_terminal_tasks_until_they_are_reaped() {
        let mgr = TaskManager::new();
        mgr.spawn("finished", None, |_handle| async {
            Ok(kres_core::task::TaskOutcome {
                analysis: "published only after reap".into(),
                ..Default::default()
            })
        })
        .await;
        loop {
            let snapshot = mgr.snapshot().await;
            if snapshot.iter().all(|task| task.state.is_terminal()) {
                assert_eq!(mgr.active_count().await, 0);
                assert_eq!(
                    turns_cap_action(20, 20, snapshot.len()),
                    TurnsCapAction::DrainAndWait
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        assert_eq!(mgr.reap().await.len(), 1);
        assert_eq!(
            turns_cap_action(20, 20, mgr.snapshot().await.len()),
            TurnsCapAction::DrainAndExit
        );
    }

    #[tokio::test]
    async fn turns_cap_final_reconciliation_leaves_no_in_progress_todos() {
        let mgr = TaskManager::new();
        let mut completed = kres_core::TodoItem::new("completed", "review");
        completed.id = "completed".into();
        completed.status = kres_core::TodoStatus::Done;
        let mut orphaned = kres_core::TodoItem::new("orphaned", "review");
        orphaned.id = "orphaned".into();
        orphaned.status = kres_core::TodoStatus::InProgress;
        mgr.load_runtime_state(vec![completed, orphaned], Vec::new(), None, 20)
            .await;

        let (reset, deferred) = reconcile_turn_cap_todos(&mgr).await;

        assert_eq!((reset, deferred), (1, 1));
        let live = mgr.todo_snapshot().await;
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].id, "completed");
        assert_eq!(live[0].status, kres_core::TodoStatus::Done);
        let deferred = mgr.deferred_snapshot().await;
        assert_eq!(deferred.len(), 1);
        assert_eq!(deferred[0].id, "orphaned");
        assert_eq!(deferred[0].status, kres_core::TodoStatus::Pending);
    }

    #[tokio::test]
    async fn review_scan_context_does_not_parse_plan_prose() {
        let mgr = TaskManager::new();
        let scan = r#"{"functions":[{"name":"filemap_fault","risk_rating":72}],"research_questions":[],"file_risk_rating":72}"#;
        let plan = kres_core::Plan::new(
            format!(
                "review\n--- WHOLE-FILE RISK SCAN ---\n{scan}\n--- END WHOLE-FILE RISK SCAN ---"
            ),
            "goal",
            kres_core::TaskMode::Audit,
        );
        mgr.load_runtime_state(Vec::new(), Vec::new(), Some(plan), 1)
            .await;

        assert_eq!(
            review_file_scan_context(&mgr, Path::new(env!("CARGO_MANIFEST_DIR")), "mm/filemap.c")
                .await,
            None
        );
    }

    #[test]
    fn independently_large_source_crosses_every_scope_with_every_diff_chunk() {
        let target_source = "x".repeat(1_228_126);
        let expected = (0..191)
            .map(|index| format!("function_{index}"))
            .collect::<BTreeSet<_>>();
        let window = crate::change_survey::AggregateTargetDiff {
            baseline: "base".into(),
            head: "head".into(),
            diff: "d".repeat(6_885_530),
        };
        let chunks =
            split_diff_for_inference(&window.diff, CHANGE_SURVEY_PAIR_PARTITION_BYTES).unwrap();
        let source_chunks =
            split_source_for_inference(&target_source, CHANGE_SURVEY_PAIR_PARTITION_BYTES).unwrap();

        assert_eq!(
            chunks
                .iter()
                .map(|chunk| &window.diff[chunk.source_start..chunk.source_end])
                .collect::<String>(),
            window.diff
        );
        assert!(chunks.len() > 1);
        assert_eq!(
            source_chunks
                .iter()
                .map(|chunk| &target_source[chunk.source_start..chunk.source_end])
                .collect::<String>(),
            target_source
        );
        assert!(source_chunks.len() > 1);
        let pair_count = chunks.len() * source_chunks.len();
        let source_bytes_sent: usize = source_chunks
            .iter()
            .map(|chunk| chunk.text.len() * chunks.len())
            .sum();
        let diff_bytes_sent: usize = chunks
            .iter()
            .map(|chunk| (chunk.source_end - chunk.source_start) * source_chunks.len())
            .sum();
        assert_eq!(source_bytes_sent, target_source.len() * chunks.len());
        assert_eq!(diff_bytes_sent, window.diff.len() * source_chunks.len());
        assert_eq!(pair_count, chunks.len() * source_chunks.len());
        let prompt = change_survey_chunk_prompt(
            "mm/vmscan.c",
            &window,
            Some(&expected),
            Some(ChangeSurveySourceChunk {
                text: &source_chunks[0].text,
                index: 0,
                count: source_chunks.len(),
            }),
            Some(ChangeSurveyDiffChunk {
                text: &chunks[0].text,
                index: 0,
                count: chunks.len(),
            }),
        );
        assert!(prompt.cached_prefix.contains(&source_chunks[0].text));
        assert!(prompt.tail.contains(&chunks[0].text));
    }

    #[test]
    fn change_survey_losslessly_partitions_at_a_small_semantic_target() {
        let partition_bytes = 1024;
        let diff = "d".repeat(2048);
        let chunks = split_diff_for_inference(&diff, partition_bytes).unwrap();

        assert_eq!(
            chunks
                .iter()
                .map(|chunk| &diff[chunk.source_start..chunk.source_end])
                .collect::<String>(),
            diff
        );
        assert!(chunks.len() >= 2);
        assert!(chunks
            .iter()
            .all(|chunk| chunk.text.len() <= partition_bytes));
    }

    /// A function the target defines itself is not an external
    /// interaction, however the change survey rated the same name
    /// elsewhere. The file-survey inference that used to enforce this
    /// is gone; Rust now builds the research questions directly, so
    /// the filter is tested where it actually runs.
    #[test]
    fn local_definition_shadows_same_named_external_risk() {
        let inventory = FileSurveyInventory {
            functions: BTreeMap::from([("folio_put".to_string(), 2)]),
            calls: vec!["folio_put".to_string()],
        };
        assert_eq!(
            inventory.interaction_kind(
                "folio_put",
                "static void folio_put(void) {}\nvoid use(void) { folio_put(); }",
            ),
            None,
            "a locally defined function is not an external interaction"
        );
    }

    #[test]
    fn empty_structured_file_survey_triggers_fallback() {
        let context = vec![serde_json::json!({
            "result": {
                "functions_defined": [],
                "calls": []
            }
        })];

        assert!(FileSurveyInventory::from_context(&context).is_none());
    }

    #[tokio::test]
    async fn a_failed_initial_prompt_exits_instead_of_idling() {
        // A review whose bootstrap fails latches `stop_latched`, so
        // auto-continue never fires; and `exit_on_idle` is false whenever
        // stdout is a tty, so no reaper path can exit either. Before this
        // check the process sat in the interactive loop forever with no work
        // and no exit status. Assert the two conditions that made that
        // unrecoverable still hold, so the guard cannot be dropped silently.
        let session = Session::new(TaskManager::new(), ReplConfig::default()).await;
        assert!(
            !session.cfg.exit_on_idle,
            "default (tty-shaped) config must not exit on idle; that is why the \
             failed-submission guard exists"
        );
        session
            .stop_latched
            .store(true, std::sync::atomic::Ordering::Release);
        assert!(
            !session.should_auto_continue().await,
            "a latched session never resumes on its own"
        );
    }

    #[test]
    fn semcode_inventory_survives_ifdef_duplicate_definitions() {
        // mm/vmscan.c defines `cgroup_reclaim` at :204 and again at :249
        // across the `#ifdef CONFIG_MEMCG` at :201, and twelve other pairs do
        // the same. semcode reports 204 entries for 191 unique names.
        // Rejecting the survey over that forced a five-minute inference
        // fallback and ultimately aborted the 2026-08-05 review.
        let context = vec![serde_json::json!({
            "source": "mcp:survey:mm/vmscan.c",
            "result": {
                "functions_defined": [
                    ["cgroup_reclaim", 8],
                    ["shrink_node", 4],
                    ["cgroup_reclaim", 11],
                ],
                "calls": [["folio_put", 0]]
            }
        })];

        let inventory =
            FileSurveyInventory::from_context(&context).expect("ifdef duplicates must not reject");

        assert_eq!(inventory.functions.len(), 2);
        assert_eq!(inventory.functions.get("cgroup_reclaim").copied(), Some(11));
        assert_eq!(inventory.functions.get("shrink_node").copied(), Some(4));
    }

    #[test]
    fn inferred_file_survey_inventory_takes_names_and_rust_counts_them() {
        // The model supplies only names it can see; Rust computes every use
        // count from source, so a duplicate name is a merge rather than a
        // whole-response rejection.
        let source = "int one(void) { return one(); }\nint two(void) { return one(); }\n";
        let inventory = FileSurveyInventory::try_from_inferred(
            InferredFileSurveyInventory {
                functions: vec![
                    InferredFunctionInventory { name: "one".into() },
                    InferredFunctionInventory { name: "two".into() },
                    InferredFunctionInventory { name: "one".into() },
                ],
                calls: vec!["one".into()],
            },
            source,
        )
        .unwrap();

        assert_eq!(inventory.functions.get("one").copied(), Some(3));
        assert_eq!(inventory.functions.get("two").copied(), Some(1));
    }

    #[test]
    fn inferred_inventory_rejects_a_use_count_from_the_model() {
        let parsed = serde_json::from_str::<InferredFileSurveyInventory>(
            r#"{"functions":[{"name":"one","uses":2}],"calls":[]}"#,
        );
        assert!(parsed.is_err(), "the model must not supply use counts");
    }

    #[test]
    fn fallback_inventory_enforces_the_ctags_floor() {
        // The ctags floor is the only thing left to check: counts are Rust's,
        // so they can no longer disagree with anything.
        let inventory = FileSurveyInventory {
            functions: BTreeMap::from([("one".to_string(), 2)]),
            calls: vec!["one".into()],
        };
        inventory
            .validate_fallback(&BTreeSet::from(["one".to_string()]))
            .unwrap();
        assert!(inventory
            .validate_fallback(&BTreeSet::from(["one".to_string(), "missing".to_string()]))
            .is_err());
    }

    #[tokio::test]
    async fn change_survey_checkpoint_roundtrips_net_diff_assessment() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("change-survey.json");
        let store = ChangeSurveyCheckpointStore::open(
            path.clone(),
            "/repo/target.c".into(),
            "source-hash".into(),
            "base".into(),
            "head".into(),
            false,
        )
        .unwrap();
        let rating = ChangeSurveyReport {
            baseline: "base".into(),
            head: "head".into(),
            target_function_risks: vec![crate::change_survey::FunctionRisk {
                name: "target".into(),
                risk_rating: 70,
                reason: "recent rewrite".into(),
            }],
            external_major_risks: Vec::new(),
        };
        store.record(rating.clone()).await.unwrap();

        let reopened = ChangeSurveyCheckpointStore::open(
            path.clone(),
            "/repo/target.c".into(),
            "source-hash".into(),
            "base".into(),
            "head".into(),
            true,
        )
        .unwrap();
        assert_eq!(reopened.report().await, Some(rating));

        let fresh = ChangeSurveyCheckpointStore::open(
            path,
            "/repo/target.c".into(),
            "source-hash".into(),
            "base".into(),
            "head".into(),
            false,
        )
        .unwrap();
        assert!(fresh.report().await.is_none());
    }

    #[test]
    fn clear_removes_change_survey_checkpoint_and_temporary_file() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("change-survey.json");
        let temporary_path = path.with_extension("json.tmp");
        std::fs::write(&path, "checkpoint").unwrap();
        std::fs::write(&temporary_path, "temporary").unwrap();

        assert!(remove_change_survey_checkpoint(&path).unwrap());
        assert!(!path.exists());
        assert!(!temporary_path.exists());
        assert!(!remove_change_survey_checkpoint(&path).unwrap());
    }

    #[tokio::test]
    async fn review_scan_context_requires_matching_target() {
        let mgr = TaskManager::new();
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let target = "Cargo.toml";
        let source = std::fs::read_to_string(workspace.join(target)).unwrap();
        let scan = CompletedReviewFileScan {
            target: target.into(),
            source_hash: change_survey_source_hash(&workspace.join(target), &source).unwrap(),
            baseline: "test-baseline".into(),
            head: current_review_head(workspace).unwrap(),
            scan: "live scan".into(),
        };
        cache_review_file_scan(&mgr, &scan).await;

        assert_eq!(
            review_file_scan_context(&mgr, workspace, target)
                .await
                .as_deref(),
            Some("live scan")
        );
        assert_eq!(
            review_file_scan_context(&mgr, workspace, "mm/vmscan.c").await,
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn review_source_fingerprint_includes_executable_mode() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let target = temporary.path().join("target.c");
        let source = "int target(void) { return 0; }\n";
        std::fs::write(&target, source).unwrap();

        let mut permissions = std::fs::metadata(&target).unwrap().permissions();
        permissions.set_mode(0o644);
        std::fs::set_permissions(&target, permissions).unwrap();
        let ordinary = change_survey_source_hash(&target, source).unwrap();

        let mut permissions = std::fs::metadata(&target).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&target, permissions).unwrap();
        let executable = change_survey_source_hash(&target, source).unwrap();

        assert_ne!(ordinary, executable);
    }

    #[tokio::test]
    async fn lensed_review_restores_dropped_followups_as_pending_todos() {
        let mgr = TaskManager::new();
        let followups = vec![serde_json::json!({
            "type": "source",
            "name": "iptunnel_xmit_stats",
            "reason": "[MISSING] trace unchanged accounting helper"
        })];

        ensure_review_followups_remain_pending(&mgr, &followups).await;

        let todo = mgr.todo_snapshot().await;
        assert_eq!(todo.len(), 1);
        assert_eq!(todo[0].kind, "source");
        assert_eq!(todo[0].name, "iptunnel_xmit_stats");
        assert_eq!(todo[0].status, kres_core::TodoStatus::Pending);

        ensure_review_followups_remain_pending(&mgr, &followups).await;
        assert_eq!(
            mgr.todo_snapshot().await.len(),
            1,
            "restoring same followup twice must not duplicate it"
        );
    }

    #[tokio::test]
    async fn turn_cap_followups_are_recorded_without_todo_agent() {
        let mgr = TaskManager::new();
        let followups = vec![
            serde_json::json!({
                "type": "source",
                "name": "iptunnel_xmit_stats",
                "reason": "[EXTEND] preserve frontier at turns cap"
            }),
            serde_json::json!({
                "type": "source",
                "name": "iptunnel_xmit_stats",
                "reason": "duplicate"
            }),
        ];

        let added = add_followups_as_pending(&mgr, &followups, "turn cap").await;
        assert_eq!(added, 1);

        let todo = mgr.todo_snapshot().await;
        assert_eq!(todo.len(), 1);
        assert_eq!(todo[0].status, kres_core::TodoStatus::Pending);
        assert_eq!(todo[0].kind, "source");
        assert_eq!(todo[0].name, "iptunnel_xmit_stats");

        assert_eq!(mgr.defer_pending().await, 1);
        assert_eq!(mgr.todo_snapshot().await.len(), 0);
        assert_eq!(mgr.deferred_snapshot().await.len(), 1);
    }

    #[test]
    fn goal_context_lists_recorded_findings() {
        let findings = vec![kres_core::Finding {
            id: "lan78xx_eeprom_hw_cfg_led_restore_skipped".into(),
            title: "lan78xx restore skipped".into(),
            severity: kres_core::findings::Severity::Medium,
            status: kres_core::findings::Status::Active,
            relevant_symbols: vec![kres_core::findings::RelevantSymbol {
                name: "lan78xx_read_raw_eeprom".into(),
                filename: "drivers/net/usb/lan78xx.c".into(),
                line: 1041,
                definition: "fn body".into(),
            }],
            relevant_file_sections: Vec::new(),
            summary: "bare return skips HW_CFG restore".into(),
            reproducer_sketch: "fault USB".into(),
            impact: "LEDs remain disabled".into(),
            mechanism_detail: None,
            fix_sketch: None,
            open_questions: Vec::new(),
            first_seen_task: None,
            last_updated_task: None,
            first_seen_at: None,
            related_finding_ids: Vec::new(),
            details: Vec::new(),
            reactivate: false,
            resolved_questions: vec![],
            introduced_by: None,
        }];

        let ctx = recorded_findings_goal_context(&findings);
        assert!(ctx.contains("Recorded findings"));
        assert!(ctx.contains("lan78xx_eeprom_hw_cfg_led_restore_skipped"));
        assert!(ctx.contains("drivers/net/usb/lan78xx.c:1041"));
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

    /// `kres_core::consent::install` sets a `OnceLock`, so every test
    /// in this binary shares ONE store and `clear()` in one test wipes
    /// a grant another just made. Cargo runs these on parallel
    /// threads, so the consent tests must take this lock for their
    /// whole grant→act→assert window, not just around `install`.
    /// Tokio's mutex, not std's: the guard is held across the
    /// `persist_code_output` await, and it does not poison, so a
    /// panicking test cannot wedge the rest.
    static CONSENT_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Take the consent lock and hand back a store with no grants.
    async fn exclusive_consent() -> (
        tokio::sync::MutexGuard<'static, ()>,
        Arc<kres_core::ConsentStore>,
    ) {
        let guard = CONSENT_TEST_LOCK.lock().await;
        let _ = kres_core::consent::install(Arc::new(kres_core::ConsentStore::new()));
        let store = kres_core::consent::get().expect("consent installed");
        store.clear();
        (guard, store)
    }

    #[tokio::test]
    async fn code_output_absolute_outside_workspace_without_consent_is_rejected() {
        // Fresh consent store with NO grants.
        let (_consent, _store) = exclusive_consent().await;
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
        let (_consent, store) = exclusive_consent().await;
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
