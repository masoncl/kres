//! Workflow executor — walks a [`Workflow`] step by step, calls a
//! [`Driver`] for each, evaluates `eval.expr`, and dispatches the
//! `on_fail` action (repeat / rerun_chain / branch_to / continue /
//! exit_failure). No LLM call here: tests inject a [`ScriptedDriver`]
//! that returns pre-baked outputs keyed by `(step_id, attempt)`, so
//! we can watch the iteration logic land each branch (step repeats,
//! review failures branch back to patch writing, build triage
//! exhausts, etc.).
//!
//! The executor is workflow-driven, not LLM-driven: it does not
//! interpolate variables into prompt strings, run post_actions, or
//! call out to the network. Those concerns belong to the (still
//! unwritten) production runner. What this module exists to do is
//! exercise the **control-flow** semantics encoded in the workflow
//! schema, so that authoring fix.json (or any future workflow) is
//! testable without standing up the agent pipeline.
//!
//! ## Expression language
//!
//! [`expr::eval`] evaluates the `run_if` / `skip_if` / `eval.expr`
//! grammar pinned by the workflow `$format` block:
//!
//! ```text
//! expr   := or
//! or     := and ('||' and)*
//! and    := cmp ('&&' cmp)*
//! cmp    := '(' or ')' | path op rhs
//! op     := '==' | '!=' | '<' | '<=' | '>' | '>='
//! rhs    := path | string | int | bool
//! path   := ident ('.' ident)*
//! ```
//!
//! A bare path (no dots) inside an `eval.expr` resolves against the
//! current step's outputs (`commit_sha != ''` is shorthand for
//! `<current step>.commit_sha != ''`). Dotted paths name an explicit
//! step (`write-patch.attempt`) or `workflow.<input-or-derived-field>`.
//!
//! The two synthetic per-step fields documented in fix.json's
//! `$format.counters` (`{step}.attempt` and `{step}.eval_failures`)
//! are exposed by the resolver alongside whatever outputs the driver
//! has produced.

use std::collections::{BTreeMap, HashMap};

use async_trait::async_trait;
use serde_json::{Map, Value};

use crate::workflow::{Aggregate, Lens, OnExhausted, OnFailAction, Step, Workflow};

/// Snapshot of one step's runtime state. Carries the everything
/// needed to resume from disk: status, attempt counter, eval
/// failures, and last produced outputs.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StepState {
    pub id: String,
    pub status: StepStatus,
    pub attempt: u32,
    pub eval_failures: u32,
    pub outputs: Map<String, Value>,
    /// Outputs from the last settled run that should survive a
    /// future skip. Kept separate from `outputs` while the step is
    /// Pending so downstream expressions cannot observe stale data
    /// before the skip condition is actually evaluated.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub preserved_outputs_on_skip: Map<String, Value>,
    /// Partial per-lens outputs for a lensed step. Filled
    /// incrementally as each lens settles in the per-lens loop;
    /// cleared when the step's aggregator runs and `outputs` is
    /// populated. Persisted so a resume mid-fan-out doesn't re-run
    /// already-completed lenses (Fix #10). Empty for non-lensed
    /// steps.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub lens_outputs: Map<String, Value>,
}

/// On-disk snapshot of a workflow run — enough to resume from
/// where it left off. Written atomically (tmp + rename) every time
/// a step settles, plus on Trace::status changes. The snapshot is
/// addressable by `workflow.id`; multiple in-flight workflows can
/// coexist in the same dir.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WorkflowSnapshot {
    pub schema_version: u32,
    pub workflow_id: String,
    pub inputs: Map<String, Value>,
    pub steps: Vec<StepState>,
    pub events_count: usize,
}

impl WorkflowSnapshot {
    /// Current snapshot schema version. Bump when the on-disk shape
    /// changes in a way that would crash old loaders.
    pub const SCHEMA_VERSION: u32 = 1;

    /// Latest persisted snapshot for the given workflow id. Returns
    /// None when the file is missing OR carries a `schema_version`
    /// this binary doesn't understand — better to start clean than
    /// to deserialise into a struct shape that has since changed.
    pub fn load(dir: &std::path::Path, workflow_id: &str) -> Option<Self> {
        let p = dir.join(format!("workflow-{workflow_id}.json"));
        let body = std::fs::read_to_string(&p).ok()?;
        // Peek at the version BEFORE attempting full deserialisation
        // — if it's a future version, we want a clear "skip" rather
        // than a confusing parse error from a renamed field.
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
            let ver = v
                .get("schema_version")
                .and_then(|x| x.as_u64())
                .unwrap_or(0);
            if ver as u32 != Self::SCHEMA_VERSION {
                tracing::warn!(
                    target: "kres_agents::workflow_exec",
                    "snapshot {} has schema_version {ver}; this binary expects {} — ignoring",
                    p.display(),
                    Self::SCHEMA_VERSION,
                );
                return None;
            }
        }
        serde_json::from_str(&body).ok()
    }

    /// Atomically write to `<dir>/workflow-<id>.json` (tmp + rename).
    pub fn save(&self, dir: &std::path::Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let target = dir.join(format!("workflow-{}.json", self.workflow_id));
        let tmp = dir.join(format!("workflow-{}.json.tmp", self.workflow_id));
        let body = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(&tmp, body)?;
        std::fs::rename(tmp, target)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    /// Hasn't run yet, or has been reset for a re-entry.
    Pending,
    /// `run_if` was false (or `skip_if` true).
    Skipped,
    /// Eval passed, or no eval configured and the step settled.
    Done,
    /// Eval exhausted with `on_exhausted: continue` — kept on the
    /// graph so dependents can still progress.
    DoneWithFailure,
    /// `branch_to` jumped away from this step before it settled.
    BranchedAway,
}

/// A driver returns the outputs a step would emit. Tests script
/// outcomes by `(step_id, attempt)`; production drivers (see
/// [`crate::workflow_runner::LlmDriver`]) call the kres-llm client.
///
/// Async because real drivers do network I/O. Returning `Err`
/// terminates the workflow with [`WorkflowStatus::Failure`] — the
/// LLM client itself handles transient retries (HTTP 429,
/// transport blips) before giving up, so an error here means the
/// driver has decided the step cannot be retried within this run.
#[async_trait]
pub trait Driver: Sync {
    /// Run one step instance. `lens` is `Some` for one of N parallel
    /// calls when the step has a `lenses` fan-out; `None` for plain
    /// steps. The driver should bind `{{lens.<field>}}` from the
    /// passed-in lens in any prompt it builds.
    async fn run(
        &self,
        step: &Step,
        attempt: u32,
        ctx: &ExecContext<'_>,
        lens: Option<&Lens>,
    ) -> Result<Map<String, Value>, String>;

    /// Run a step's `post_actions` after its eval has passed (or
    /// after a no-eval step has settled). Default is a no-op so
    /// scripted tests don't have to implement it. The
    /// [`crate::workflow_runner::LlmDriver`] override executes typed
    /// git / make / publish-fix actions in the workspace.
    async fn run_post_actions(
        &self,
        _step: &Step,
        _ctx: &ExecContext<'_>,
    ) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }

    /// Run an N+1 consolidate call for a lensed step whose
    /// `aggregate` is `Consolidate`. `per_lens` is the list of
    /// `(lens_id, outputs)` from the fan-out. The implementation
    /// is expected to send these to an LLM with the step's
    /// `consolidate.prompt` plus an OUTPUT SCHEMA tail and parse
    /// the response into the step's declared outputs.
    ///
    /// Default impl errors so a workflow that asks for
    /// `aggregate: consolidate` against a driver that doesn't
    /// implement it fails loudly instead of silently dropping
    /// findings.
    async fn consolidate(
        &self,
        _step: &Step,
        _ctx: &ExecContext<'_>,
        _per_lens: &[(String, Map<String, Value>)],
    ) -> Result<Map<String, Value>, String> {
        Err(
            "driver does not implement consolidate; use aggregate=concat or supply an LlmDriver"
                .into(),
        )
    }

    /// Run an LLM-judged eval. The driver builds a prompt with the
    /// step's outputs as JSON + the step's `eval.judge_prompt`, asks
    /// for `{pass: bool, reason: string}`. Returns `(pass, reason)`.
    /// Default impl errors so a workflow asking for `judge_llm`
    /// eval against a non-LLM driver fails loudly.
    async fn judge(&self, _step: &Step, _ctx: &ExecContext<'_>) -> Result<(bool, String), String> {
        Err(
            "driver does not implement judge; use eval.type=field_check or supply an LlmDriver"
                .into(),
        )
    }

    /// Optimised dispatch for a lensed step whose `aggregate` is
    /// `Consolidate`. Drivers that wire an Orchestrator +
    /// ConsolidatorClient can override this to gather ONCE and fan
    /// out N parallel slow calls (`Orchestrator::run_with_lenses`).
    /// Default returns Err so the
    /// executor falls back to the per-lens path: N independent
    /// gather loops + N slow calls + a separate
    /// `driver.consolidate()` call.
    async fn lens_fan_out_consolidate(
        &self,
        _step: &Step,
        _ctx: &ExecContext<'_>,
    ) -> Result<Map<String, Value>, String> {
        Err("driver does not implement lens_fan_out_consolidate".into())
    }
}

/// Read-only view exposed to `Driver::run`. Has the workflow inputs
/// and every step's current state so a driver can decide what to
/// emit based on prior outputs.
pub struct ExecContext<'a> {
    pub workflow_inputs: &'a Map<String, Value>,
    pub steps: &'a HashMap<String, StepState>,
}

pub fn active_lenses<'a>(step: &'a Step, ctx: &ExecContext<'_>) -> Result<Vec<&'a Lens>, String> {
    step.lenses
        .iter()
        .filter_map(|lens| match lens_is_active(lens, ctx, Some(&step.id)) {
            Ok(true) => Some(Ok(lens)),
            Ok(false) => None,
            Err(e) => Some(Err(format!("lens '{}' condition: {e}", lens.id))),
        })
        .collect()
}

fn lens_is_active(
    lens: &Lens,
    ctx: &ExecContext<'_>,
    current_step: Option<&str>,
) -> Result<bool, String> {
    if let Some(expr) = lens.run_if.as_deref() {
        if !expr::eval(expr, ctx, current_step)? {
            return Ok(false);
        }
    }
    if let Some(expr) = lens.skip_if.as_deref() {
        if expr::eval(expr, ctx, current_step)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Final disposition of a workflow run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowStatus {
    /// All steps settled, no terminal short-circuit, no exit_failure.
    Success,
    /// A step with `terminal_on_success: true` settled — the
    /// workflow ends there and downstream steps don't run.
    TerminalSuccess(String),
    /// Exit triggered by `on_fail.action: exit_failure` or by an
    /// `on_exhausted: exit_failure` after max_attempts.
    Failure(String),
    /// The executor's own iteration cap fired. Indicates a workflow
    /// that loops forever — almost always an authoring bug.
    IterationCap(usize),
}

/// Trace event — the executor emits one or more of these per step
/// iteration so tests can assert on the path the run actually took.
#[derive(Debug, Clone)]
pub enum TraceEvent {
    StepStarted {
        id: String,
        attempt: u32,
    },
    StepSkipped {
        id: String,
        reason: String,
    },
    StepProduced {
        id: String,
        attempt: u32,
        outputs: Map<String, Value>,
    },
    EvalPassed {
        id: String,
        attempt: u32,
    },
    EvalFailed {
        id: String,
        attempt: u32,
        action: String,
        eval_failures: u32,
    },
    BranchedTo {
        from: String,
        to: String,
    },
    RerunChain {
        from: String,
        ids: Vec<String>,
    },
    Exhausted {
        id: String,
        on_exhausted: String,
        attempts: u32,
    },
    PostAction {
        id: String,
        log: Vec<String>,
    },
    /// A lensed step is about to fan out N concurrent driver calls.
    FanOut {
        id: String,
        attempt: u32,
        lens_ids: Vec<String>,
    },
    /// One lens of a fan-out completed (or errored) and produced
    /// outputs. Order is completion order, not lens-array order.
    LensProduced {
        id: String,
        attempt: u32,
        lens_id: String,
        outputs: Map<String, Value>,
    },
    /// All lenses settled; an N+1 consolidate LLM call is about
    /// to run with the per-lens outputs. Only emitted when the
    /// step's aggregate strategy is `consolidate`.
    Consolidating {
        id: String,
        attempt: u32,
        lens_count: usize,
    },
    /// All lenses settled; outputs aggregated per the step's
    /// `aggregate` strategy. The aggregated map is what the
    /// step's eval and downstream interpolation see.
    FanIn {
        id: String,
        attempt: u32,
        aggregated: Map<String, Value>,
        strategy: String,
    },
    Terminated {
        status: WorkflowStatus,
    },
}

/// Trace of a workflow run.
pub struct Trace {
    pub events: Vec<TraceEvent>,
    pub status: WorkflowStatus,
    pub final_state: HashMap<String, StepState>,
}

/// One-line pretty-print of a single TraceEvent. Same formatting
/// rules as `Trace::pretty()` but exposed for streaming consumers
/// (the REPL's async_println, a TUI). Trailing newline is included.
pub fn format_event(ev: &TraceEvent) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    match ev {
        TraceEvent::StepStarted { id, attempt } => {
            let _ = writeln!(out, "→ start  {id} (attempt {attempt})");
        }
        TraceEvent::StepSkipped { id, reason } => {
            let _ = writeln!(out, "· skip   {id} — {reason}");
        }
        TraceEvent::StepProduced {
            id,
            attempt,
            outputs,
        } => {
            let kv = format_output_map(outputs);
            let _ = writeln!(out, "✓ produced {id} (attempt {attempt}) — {{{kv}}}");
        }
        TraceEvent::EvalPassed { id, attempt } => {
            let _ = writeln!(out, "✓ eval ok {id} (attempt {attempt})");
        }
        TraceEvent::EvalFailed {
            id,
            attempt,
            action,
            eval_failures,
        } => {
            let _ = writeln!(
                out,
                "✗ eval fail {id} (attempt {attempt}, total fails {eval_failures}) → {action}"
            );
        }
        TraceEvent::BranchedTo { from, to } => {
            let _ = writeln!(out, "⇢ branch {from} → {to}");
        }
        TraceEvent::RerunChain { from, ids } => {
            let _ = writeln!(out, "↻ rerun  {from} → {}", ids.join(","));
        }
        TraceEvent::Exhausted {
            id,
            on_exhausted,
            attempts,
        } => {
            let _ = writeln!(
                out,
                "! exhausted {id} after {attempts} attempts → {on_exhausted}"
            );
        }
        TraceEvent::PostAction { id, log } => {
            let _ = writeln!(out, "➤ post   {id}");
            for line in log {
                let _ = writeln!(out, "    {line}");
            }
        }
        TraceEvent::FanOut {
            id,
            attempt,
            lens_ids,
        } => {
            let _ = writeln!(
                out,
                "⇉ fan-out {id} (attempt {attempt}) → {}",
                lens_ids.join(",")
            );
        }
        TraceEvent::LensProduced {
            id,
            attempt,
            lens_id,
            outputs,
        } => {
            let kv = format_output_map(outputs);
            let _ = writeln!(
                out,
                "  · lens {lens_id}@{id} (attempt {attempt}) — {{{kv}}}"
            );
        }
        TraceEvent::Consolidating {
            id,
            attempt,
            lens_count,
        } => {
            let _ = writeln!(
                out,
                "⊕ consolidate {id} (attempt {attempt}, {lens_count} lens outputs)"
            );
        }
        TraceEvent::FanIn {
            id,
            attempt,
            aggregated,
            strategy,
        } => {
            let kv = format_output_map(aggregated);
            let _ = writeln!(
                out,
                "⇇ fan-in  {id} (attempt {attempt}, {strategy}) — {{{kv}}}"
            );
        }
        TraceEvent::Terminated { status } => {
            let _ = writeln!(out, "□ terminated: {:?}", status);
        }
    }
    out
}

impl Trace {
    /// Pretty single-line-per-event dump. Use with
    /// `cargo test -- --nocapture` to watch the iteration land.
    pub fn pretty(&self) -> String {
        let mut out = String::new();
        for ev in &self.events {
            match ev {
                TraceEvent::StepStarted { id, attempt } => {
                    out.push_str(&format!("→ start  {id} (attempt {attempt})\n"));
                }
                TraceEvent::StepSkipped { id, reason } => {
                    out.push_str(&format!("· skip   {id} — {reason}\n"));
                }
                TraceEvent::StepProduced {
                    id,
                    attempt,
                    outputs,
                } => {
                    let kv = format_output_map(outputs);
                    out.push_str(&format!("✓ produced {id} (attempt {attempt}) — {{{kv}}}\n"));
                }
                TraceEvent::EvalPassed { id, attempt } => {
                    out.push_str(&format!("✓ eval ok {id} (attempt {attempt})\n"));
                }
                TraceEvent::EvalFailed {
                    id,
                    attempt,
                    action,
                    eval_failures,
                } => {
                    out.push_str(&format!(
                        "✗ eval fail {id} (attempt {attempt}, total fails {eval_failures}) → {action}\n"
                    ));
                }
                TraceEvent::BranchedTo { from, to } => {
                    out.push_str(&format!("⇢ branch {from} → {to}\n"));
                }
                TraceEvent::RerunChain { from, ids } => {
                    out.push_str(&format!("↻ rerun  {from} → {}\n", ids.join(",")));
                }
                TraceEvent::Exhausted {
                    id,
                    on_exhausted,
                    attempts,
                } => {
                    out.push_str(&format!(
                        "! exhausted {id} after {attempts} attempts → {on_exhausted}\n"
                    ));
                }
                TraceEvent::PostAction { id, log } => {
                    out.push_str(&format!("➤ post   {id}\n"));
                    for line in log {
                        out.push_str(&format!("    {line}\n"));
                    }
                }
                TraceEvent::FanOut {
                    id,
                    attempt,
                    lens_ids,
                } => {
                    out.push_str(&format!(
                        "⇉ fan-out {id} (attempt {attempt}) → {}\n",
                        lens_ids.join(",")
                    ));
                }
                TraceEvent::LensProduced {
                    id,
                    attempt,
                    lens_id,
                    outputs,
                } => {
                    let kv = format_output_map(outputs);
                    out.push_str(&format!(
                        "  · lens {lens_id}@{id} (attempt {attempt}) — {{{kv}}}\n"
                    ));
                }
                TraceEvent::Consolidating {
                    id,
                    attempt,
                    lens_count,
                } => {
                    out.push_str(&format!(
                        "⊕ consolidate {id} (attempt {attempt}, {lens_count} lens outputs)\n"
                    ));
                }
                TraceEvent::FanIn {
                    id,
                    attempt,
                    aggregated,
                    strategy,
                } => {
                    let kv = format_output_map(aggregated);
                    out.push_str(&format!(
                        "⇇ fan-in  {id} (attempt {attempt}, {strategy}) — {{{kv}}}\n"
                    ));
                }
                TraceEvent::Terminated { status } => {
                    out.push_str(&format!("□ terminated: {:?}\n", status));
                }
            }
        }
        out
    }
}

fn format_output_map(outputs: &Map<String, Value>) -> String {
    outputs
        .iter()
        .filter(|(k, _)| !k.starts_with('_'))
        .map(|(k, v)| format!("{k}={}", format_output_value(k, v)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_output_value(key: &str, value: &Value) -> String {
    match value {
        Value::String(s) => {
            let max = if key == "analysis" { 240 } else { 120 };
            format!("{:?}", truncate_chars(&clean_ws(s), max))
        }
        Value::Array(items) => format_output_array(items),
        Value::Object(obj) => {
            if obj.is_empty() {
                "{}".to_string()
            } else {
                let compact = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
                truncate_chars(&compact, 160)
            }
        }
        Value::Null => "null".to_string(),
        Value::Bool(_) | Value::Number(_) => value.to_string(),
    }
}

fn format_output_array(items: &[Value]) -> String {
    if items.is_empty() {
        return "[]".to_string();
    }

    let labels = items
        .iter()
        .take(3)
        .map(format_array_item)
        .collect::<Vec<_>>();
    let tail = if items.len() > labels.len() {
        format!(", +{} more", items.len() - labels.len())
    } else {
        String::new()
    };
    format!("[{} item(s): {}{tail}]", items.len(), labels.join(", "))
}

fn format_array_item(value: &Value) -> String {
    match value {
        Value::String(s) => format!("{:?}", truncate_chars(&clean_ws(s), 60)),
        Value::Object(obj) => {
            for key in [
                "file",
                "path",
                "file_path",
                "what",
                "summary",
                "title",
                "id",
            ] {
                if let Some(s) = obj.get(key).and_then(|v| v.as_str()) {
                    return format!("{key}={:?}", truncate_chars(&clean_ws(s), 60));
                }
            }
            let compact = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
            truncate_chars(&compact, 80)
        }
        _ => truncate_chars(&value.to_string(), 80),
    }
}

fn clean_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{}...", truncated.trim_end())
    } else {
        truncated
    }
}

const DEFAULT_ITERATION_CAP: usize = 200;
const DEFAULT_MAX_ATTEMPTS: u32 = 3;

/// Run a workflow with the given driver and workflow-input map.
/// Iteration is capped at [`DEFAULT_ITERATION_CAP`] step executions
/// to keep authoring bugs from spinning forever.
/// Type alias for the optional event observer passed to the
/// streaming variants of the executor. Fired after each
/// [`TraceEvent`] is recorded so callers (the REPL, a TUI) can
/// stream progress instead of waiting for the final
/// `Trace::pretty()` dump.
pub type EventObserver = Box<dyn Fn(&TraceEvent) + Send + Sync>;

/// Push `ev` into `events` AND, if an observer is wired, fire it
/// against the just-recorded event. Replaces every direct
/// `events.push(...)` call site so streaming and persistence share
/// one recording path.
fn record(events: &mut Vec<TraceEvent>, observer: &Option<EventObserver>, ev: TraceEvent) {
    if let Some(o) = observer {
        o(&ev);
    }
    events.push(ev);
}

pub async fn run<D: Driver + ?Sized + Send>(
    workflow: &Workflow,
    driver: &mut D,
    inputs: Map<String, Value>,
) -> Trace {
    run_with_cap(workflow, driver, inputs, DEFAULT_ITERATION_CAP).await
}

/// Same as [`run_with_cap`] but fires `observer` after every
/// trace event is recorded. Use to stream progress to a UI.
pub async fn run_with_observer<D: Driver + ?Sized + Send>(
    workflow: &Workflow,
    driver: &mut D,
    inputs: Map<String, Value>,
    iteration_cap: usize,
    observer: EventObserver,
) -> Trace {
    run_internal(
        workflow,
        driver,
        inputs,
        iteration_cap,
        None,
        None,
        Some(observer),
    )
    .await
}

/// Resume from a previously persisted [`WorkflowSnapshot`]. The
/// snapshot's `inputs` and `steps` seed the executor's state, so
/// already-Done steps are skipped over by the topo walker.
/// In-flight steps are reset to Pending so the run picks up there.
pub async fn run_resume<D: Driver + ?Sized + Send>(
    workflow: &Workflow,
    driver: &mut D,
    snapshot: WorkflowSnapshot,
    snapshot_dir: Option<std::path::PathBuf>,
    iteration_cap: usize,
) -> Trace {
    run_internal(
        workflow,
        driver,
        snapshot.inputs,
        iteration_cap,
        Some(snapshot.steps),
        snapshot_dir,
        None,
    )
    .await
}

pub async fn run_with_cap<D: Driver + ?Sized + Send>(
    workflow: &Workflow,
    driver: &mut D,
    inputs: Map<String, Value>,
    iteration_cap: usize,
) -> Trace {
    run_internal(workflow, driver, inputs, iteration_cap, None, None, None).await
}

/// Variant of run_with_cap that writes a snapshot file to
/// `snapshot_dir/workflow-<id>.json` after every state change.
pub async fn run_with_persistence<D: Driver + ?Sized + Send>(
    workflow: &Workflow,
    driver: &mut D,
    inputs: Map<String, Value>,
    iteration_cap: usize,
    snapshot_dir: std::path::PathBuf,
) -> Trace {
    run_internal(
        workflow,
        driver,
        inputs,
        iteration_cap,
        None,
        Some(snapshot_dir),
        None,
    )
    .await
}

async fn run_internal<D: Driver + ?Sized + Send>(
    workflow: &Workflow,
    driver: &mut D,
    inputs: Map<String, Value>,
    iteration_cap: usize,
    seed_states: Option<Vec<StepState>>,
    snapshot_dir: Option<std::path::PathBuf>,
    observer: Option<EventObserver>,
) -> Trace {
    let mut state: HashMap<String, StepState> = workflow
        .steps
        .iter()
        .map(|s| {
            (
                s.id.clone(),
                StepState {
                    id: s.id.clone(),
                    status: StepStatus::Pending,
                    attempt: 0,
                    eval_failures: 0,
                    outputs: Map::new(),
                    preserved_outputs_on_skip: Map::new(),
                    lens_outputs: Map::new(),
                },
            )
        })
        .collect();

    // Seed from a resume snapshot if present. Steps not in the
    // snapshot keep their fresh-Pending state; existing steps that
    // had never settled (Pending / BranchedAway from an
    // interrupted run) get reset to Pending so the run picks them
    // up again.
    if let Some(seeds) = seed_states {
        for s in seeds {
            if let Some(slot) = state.get_mut(&s.id) {
                let resumed_status = match s.status {
                    StepStatus::Pending | StepStatus::BranchedAway => StepStatus::Pending,
                    other => other,
                };
                *slot = StepState {
                    status: resumed_status,
                    ..s
                };
            }
        }
    }

    let mut events: Vec<TraceEvent> = Vec::new();
    let mut iterations: usize = 0;
    let mut status = WorkflowStatus::Success;

    // Helper closure: write a snapshot to disk if a dir was wired.
    // Closures over `&state` so the executor calls this after
    // every state change. Errors are logged but don't kill the
    // run — losing a tick of persistence is recoverable.
    let snapshot_save = |state: &HashMap<String, StepState>, events_count: usize| {
        let Some(dir) = snapshot_dir.as_ref() else {
            return;
        };
        let snap = WorkflowSnapshot {
            schema_version: 1,
            workflow_id: workflow.id.clone(),
            inputs: inputs.clone(),
            steps: workflow
                .steps
                .iter()
                .filter_map(|s| state.get(&s.id).cloned())
                .collect(),
            events_count,
        };
        if let Err(e) = snap.save(dir) {
            tracing::warn!(target: "kres_agents::workflow_exec", "snapshot save failed: {e}");
        }
    };

    // Initial snapshot — useful when --resume picks up a workflow
    // that hasn't dispatched any step yet.
    snapshot_save(&state, events.len());

    loop {
        iterations += 1;
        if iterations > iteration_cap {
            status = WorkflowStatus::IterationCap(iteration_cap);
            record(
                &mut events,
                &observer,
                TraceEvent::Terminated {
                    status: status.clone(),
                },
            );
            break;
        }

        // Save the latest state at the top of every iteration —
        // captures every status transition before the next step
        // dispatches.
        snapshot_save(&state, events.len());

        // Check Workflow.completion expressions before scheduling
        // the next step. failure_when_any wins over success_when_any
        // so a half-finished pipeline that's already in trouble
        // doesn't get reported as Success because some lower-priority
        // success expression evaluated true at the same time.
        if let Some(c) = &workflow.completion {
            let cctx = ExecContext {
                workflow_inputs: &inputs,
                steps: &state,
            };
            for expr in &c.failure_when_any {
                if let Ok(true) = expr::eval(expr, &cctx, None) {
                    status = WorkflowStatus::Failure(format!(
                        "completion.failure_when_any matched: {expr}"
                    ));
                    record(
                        &mut events,
                        &observer,
                        TraceEvent::Terminated {
                            status: status.clone(),
                        },
                    );
                    return Trace {
                        events,
                        status,
                        final_state: state,
                    };
                }
            }
            for expr in &c.success_when_any {
                if let Ok(true) = expr::eval(expr, &cctx, None) {
                    status = WorkflowStatus::TerminalSuccess(format!(
                        "completion.success_when_any matched: {expr}"
                    ));
                    record(
                        &mut events,
                        &observer,
                        TraceEvent::Terminated {
                            status: status.clone(),
                        },
                    );
                    return Trace {
                        events,
                        status,
                        final_state: state,
                    };
                }
            }
        }

        let next_idx = workflow.steps.iter().position(|s| {
            matches!(state[&s.id].status, StepStatus::Pending)
                && s.depends_on.iter().all(|d| {
                    matches!(
                        state.get(d).map(|st| st.status),
                        Some(StepStatus::Done)
                            | Some(StepStatus::Skipped)
                            | Some(StepStatus::DoneWithFailure)
                    )
                })
        });
        let Some(idx) = next_idx else {
            // Nothing left to schedule. If anything is still
            // Pending, it's blocked — that's a deadlock from bad
            // depends_on, treat as failure for visibility.
            if state.values().any(|s| s.status == StepStatus::Pending) {
                let stuck: Vec<String> = state
                    .values()
                    .filter(|s| s.status == StepStatus::Pending)
                    .map(|s| s.id.clone())
                    .collect();
                status = WorkflowStatus::Failure(format!("deadlocked, pending steps: {stuck:?}"));
            }
            break;
        };
        let step = &workflow.steps[idx];

        // Conditional skip — run_if false OR skip_if true.
        let ctx_for_skip = ExecContext {
            workflow_inputs: &inputs,
            steps: &state,
        };
        if let Some(rf) = &step.run_if {
            match expr::eval(rf, &ctx_for_skip, Some(&step.id)) {
                Ok(false) => {
                    record(
                        &mut events,
                        &observer,
                        TraceEvent::StepSkipped {
                            id: step.id.clone(),
                            reason: format!("run_if `{rf}` was false"),
                        },
                    );
                    let st = state.get_mut(&step.id).unwrap();
                    st.status = StepStatus::Skipped;
                    if step.preserve_outputs_on_skip {
                        if st.outputs.is_empty() && !st.preserved_outputs_on_skip.is_empty() {
                            st.outputs = std::mem::take(&mut st.preserved_outputs_on_skip);
                        }
                    } else {
                        st.outputs.clear();
                        st.preserved_outputs_on_skip.clear();
                    }
                    continue;
                }
                Ok(true) => {}
                Err(e) => {
                    status = WorkflowStatus::Failure(format!(
                        "step '{}' run_if eval error: {e}",
                        step.id
                    ));
                    break;
                }
            }
        }
        if let Some(sf) = &step.skip_if {
            match expr::eval(sf, &ctx_for_skip, Some(&step.id)) {
                Ok(true) => {
                    record(
                        &mut events,
                        &observer,
                        TraceEvent::StepSkipped {
                            id: step.id.clone(),
                            reason: format!("skip_if `{sf}` was true"),
                        },
                    );
                    let st = state.get_mut(&step.id).unwrap();
                    st.status = StepStatus::Skipped;
                    if step.preserve_outputs_on_skip {
                        if st.outputs.is_empty() && !st.preserved_outputs_on_skip.is_empty() {
                            st.outputs = std::mem::take(&mut st.preserved_outputs_on_skip);
                        }
                    } else {
                        st.outputs.clear();
                        st.preserved_outputs_on_skip.clear();
                    }
                    continue;
                }
                Ok(false) => {}
                Err(e) => {
                    status = WorkflowStatus::Failure(format!(
                        "step '{}' skip_if eval error: {e}",
                        step.id
                    ));
                    break;
                }
            }
        }

        // Run the step.
        let attempt = state[&step.id].attempt + 1;
        state.get_mut(&step.id).unwrap().attempt = attempt;
        record(
            &mut events,
            &observer,
            TraceEvent::StepStarted {
                id: step.id.clone(),
                attempt,
            },
        );
        let driver_ctx = ExecContext {
            workflow_inputs: &inputs,
            steps: &state,
        };

        let outputs = if step.lenses.is_empty() {
            // Plain step: one driver call.
            match driver.run(step, attempt, &driver_ctx, None).await {
                Ok(o) => o,
                Err(e) => {
                    if retry_driver_error(
                        &mut state,
                        &mut events,
                        &observer,
                        step,
                        attempt,
                        &format!("driver error: {e}"),
                    ) {
                        continue;
                    }
                    status = WorkflowStatus::Failure(format!(
                        "step '{}' driver error on attempt {attempt}: {e}",
                        step.id
                    ));
                    record(
                        &mut events,
                        &observer,
                        TraceEvent::Terminated {
                            status: status.clone(),
                        },
                    );
                    return Trace {
                        events,
                        status,
                        final_state: state,
                    };
                }
            }
        } else {
            let active = match active_lenses(step, &driver_ctx) {
                Ok(active) => active,
                Err(e) => {
                    status = WorkflowStatus::Failure(format!(
                        "step '{}' lens condition error on attempt {attempt}: {e}",
                        step.id
                    ));
                    record(
                        &mut events,
                        &observer,
                        TraceEvent::Terminated {
                            status: status.clone(),
                        },
                    );
                    return Trace {
                        events,
                        status,
                        final_state: state,
                    };
                }
            };
            let mut active_step = step.clone();
            active_step.lenses = active.iter().map(|lens| (*lens).clone()).collect();
            // Lensed step. Emit FanOut once up-front; both the
            // optimised path and the per-lens fall-back share it.
            let lens_ids: Vec<String> = active_step.lenses.iter().map(|l| l.id.clone()).collect();
            record(
                &mut events,
                &observer,
                TraceEvent::FanOut {
                    id: step.id.clone(),
                    attempt,
                    lens_ids: lens_ids.clone(),
                },
            );
            // Try the optimised single-gather + fan-out + consolidate
            // path first when aggregate=Consolidate AND the driver
            // can handle it. On success, return the aggregated map
            // out of the lens block so the standard eval +
            // post_actions code below runs. On Err, fall through to
            // the per-lens loop. Fix #7: collapses N independent
            // gathers into 1 when the configuration supports it.
            let mut optimised: Option<Map<String, Value>> = None;
            if matches!(
                active_step.aggregate.unwrap_or_default(),
                Aggregate::Consolidate
            ) {
                if let Ok(aggregated) = driver
                    .lens_fan_out_consolidate(&active_step, &driver_ctx)
                    .await
                {
                    record(
                        &mut events,
                        &observer,
                        TraceEvent::Consolidating {
                            id: step.id.clone(),
                            attempt,
                            lens_count: active_step.lenses.len(),
                        },
                    );
                    record(
                        &mut events,
                        &observer,
                        TraceEvent::FanIn {
                            id: step.id.clone(),
                            attempt,
                            aggregated: aggregated.clone(),
                            strategy: "Consolidate".into(),
                        },
                    );
                    optimised = Some(aggregated);
                }
                // Optimised path returned Err — fall through to the
                // per-lens loop. The FanOut event was already
                // recorded, so the per-lens path skips its own.
            }
            // If the optimised path produced an aggregate, return
            // it out of the lens block so the standard eval +
            // post_actions code below runs. The per-lens fall-back
            // only fires when optimised is None.
            if let Some(aggregated) = optimised {
                aggregated
            } else {
                // Per-lens fall-back path. Fan out N concurrent driver
                // calls, aggregate when they all settle.
                // Fix #10: skip lenses that already have a saved result
                // from a prior interrupted run. Resume reads
                // step.lens_outputs out of the snapshot; lenses with
                // an entry there don't re-run.
                let already_done: std::collections::BTreeSet<String> =
                    state[&step.id].lens_outputs.keys().cloned().collect();
                let lenses_to_run: Vec<&Lens> = active_step
                    .lenses
                    .iter()
                    .filter(|l| !already_done.contains(&l.id))
                    .collect();
                let futures: Vec<_> = lenses_to_run
                    .iter()
                    .map(|lens| {
                        let driver_ref = &driver;
                        let ctx_ref = &driver_ctx;
                        let step_ref = &active_step;
                        let lens_ref = *lens;
                        async move {
                            let r = driver_ref
                                .run(step_ref, attempt, ctx_ref, Some(lens_ref))
                                .await;
                            (lens_ref.id.clone(), r)
                        }
                    })
                    .collect();
                type LensResult = (String, Result<Map<String, Value>, String>);
                let lens_results: Vec<LensResult> = futures::future::join_all(futures).await;
                // Seed per_lens with already-saved entries first, then
                // fold in fresh results.
                let mut per_lens: Vec<(String, Map<String, Value>)> = active_step
                    .lenses
                    .iter()
                    .filter_map(|l| {
                        state[&step.id]
                            .lens_outputs
                            .get(&l.id)
                            .and_then(|v| v.as_object().cloned())
                            .map(|m| (l.id.clone(), m))
                    })
                    .collect();
                let mut first_err: Option<(String, String)> = None;
                for (lens_id, r) in lens_results {
                    match r {
                        Ok(m) => {
                            record(
                                &mut events,
                                &observer,
                                TraceEvent::LensProduced {
                                    id: step.id.clone(),
                                    attempt,
                                    lens_id: lens_id.clone(),
                                    outputs: m.clone(),
                                },
                            );
                            // Persist the per-lens result before the
                            // next snapshot tick (Fix #10).
                            state
                                .get_mut(&step.id)
                                .unwrap()
                                .lens_outputs
                                .insert(lens_id.clone(), Value::Object(m.clone()));
                            per_lens.push((lens_id, m));
                        }
                        Err(e) => {
                            if first_err.is_none() {
                                first_err = Some((lens_id, e));
                            }
                        }
                    }
                }
                // Order per_lens to match step.lenses ordering for
                // determinism (matters for concat aggregator).
                let lens_order: std::collections::HashMap<&str, usize> = active_step
                    .lenses
                    .iter()
                    .enumerate()
                    .map(|(i, l)| (l.id.as_str(), i))
                    .collect();
                per_lens.sort_by_key(|(id, _)| *lens_order.get(id.as_str()).unwrap_or(&usize::MAX));
                if let Some((lens_id, e)) = first_err {
                    if retry_driver_error(
                        &mut state,
                        &mut events,
                        &observer,
                        step,
                        attempt,
                        &format!("lens '{lens_id}' driver error: {e}"),
                    ) {
                        snapshot_save(&state, events.len());
                        continue;
                    }
                    status = WorkflowStatus::Failure(format!(
                        "step '{}' lens '{lens_id}' attempt {attempt} driver error: {e}",
                        step.id
                    ));
                    record(
                        &mut events,
                        &observer,
                        TraceEvent::Terminated {
                            status: status.clone(),
                        },
                    );
                    // Fix #10: persist partial per-lens outputs before
                    // terminating so resume can pick up from the
                    // failing lens without re-running the OK ones.
                    snapshot_save(&state, events.len());
                    return Trace {
                        events,
                        status,
                        final_state: state,
                    };
                }
                let strategy = step.aggregate.unwrap_or_default();
                let aggregated = match strategy {
                    Aggregate::Concat | Aggregate::ByLens => {
                        aggregate_lens_outputs(&active_step, &per_lens, strategy)
                    }
                    Aggregate::Consolidate => {
                        record(
                            &mut events,
                            &observer,
                            TraceEvent::Consolidating {
                                id: step.id.clone(),
                                attempt,
                                lens_count: per_lens.len(),
                            },
                        );
                        let cctx = ExecContext {
                            workflow_inputs: &inputs,
                            steps: &state,
                        };
                        match driver.consolidate(&active_step, &cctx, &per_lens).await {
                            Ok(m) => m,
                            Err(e) => {
                                // A consolidate failure means the fan-in could not
                                // trust the per-lens outputs it just merged. Clear
                                // cached lens outputs before retrying so the next
                                // attempt gets fresh lens contexts instead of
                                // replaying the same inconsistent maps.
                                state.get_mut(&step.id).unwrap().lens_outputs.clear();
                                if retry_driver_error(
                                    &mut state,
                                    &mut events,
                                    &observer,
                                    step,
                                    attempt,
                                    &format!("consolidate error: {e}"),
                                ) {
                                    snapshot_save(&state, events.len());
                                    continue;
                                }
                                status = WorkflowStatus::Failure(format!(
                                    "step '{}' consolidate (attempt {attempt}) failed: {e}",
                                    step.id
                                ));
                                record(
                                    &mut events,
                                    &observer,
                                    TraceEvent::Terminated {
                                        status: status.clone(),
                                    },
                                );
                                return Trace {
                                    events,
                                    status,
                                    final_state: state,
                                };
                            }
                        }
                    }
                };
                record(
                    &mut events,
                    &observer,
                    TraceEvent::FanIn {
                        id: step.id.clone(),
                        attempt,
                        aggregated: aggregated.clone(),
                        strategy: format!("{strategy:?}"),
                    },
                );
                // Aggregation's done — discard the partial per-lens
                // map so the next attempt (after an eval-fail repeat)
                // starts fresh. Without this a re-attempt would skip
                // every lens since they're all "already saved".
                state.get_mut(&step.id).unwrap().lens_outputs.clear();
                aggregated
            } // else: per-lens fall-back close
        };

        state.get_mut(&step.id).unwrap().outputs = outputs.clone();
        state
            .get_mut(&step.id)
            .unwrap()
            .preserved_outputs_on_skip
            .clear();
        record(
            &mut events,
            &observer,
            TraceEvent::StepProduced {
                id: step.id.clone(),
                attempt,
                outputs,
            },
        );

        // Eval, if configured.
        let Some(eval) = &step.eval else {
            // No eval: settle, run post_actions, then move on.
            if !step.post_actions.is_empty() {
                let pa_ctx = ExecContext {
                    workflow_inputs: &inputs,
                    steps: &state,
                };
                match driver.run_post_actions(step, &pa_ctx).await {
                    Ok(log) => record(
                        &mut events,
                        &observer,
                        TraceEvent::PostAction {
                            id: step.id.clone(),
                            log,
                        },
                    ),
                    Err(e) => {
                        status = WorkflowStatus::Failure(format!(
                            "step '{}' post_actions failed: {e}",
                            step.id
                        ));
                        break;
                    }
                }
            }
            state.get_mut(&step.id).unwrap().status = StepStatus::Done;
            if step.terminal_on_success {
                status = WorkflowStatus::TerminalSuccess(step.id.clone());
                record(
                    &mut events,
                    &observer,
                    TraceEvent::Terminated {
                        status: status.clone(),
                    },
                );
                return Trace {
                    events,
                    status,
                    final_state: state,
                };
            }
            continue;
        };
        let ctx_for_eval = ExecContext {
            workflow_inputs: &inputs,
            steps: &state,
        };
        let passed = match eval.kind {
            crate::workflow::EvalKind::FieldCheck => {
                let expr_str = match eval.expr.as_deref() {
                    Some(s) => s,
                    None => {
                        status = WorkflowStatus::Failure(format!(
                            "step '{}' field_check eval missing expr",
                            step.id
                        ));
                        break;
                    }
                };
                match expr::eval(expr_str, &ctx_for_eval, Some(&step.id)) {
                    Ok(b) => b,
                    Err(e) => {
                        status = WorkflowStatus::Failure(format!(
                            "step '{}' eval expr error: {e}",
                            step.id
                        ));
                        break;
                    }
                }
            }
            crate::workflow::EvalKind::JudgeLlm => match driver.judge(step, &ctx_for_eval).await {
                Ok((p, _reason)) => p,
                Err(e) => {
                    status = WorkflowStatus::Failure(format!(
                        "step '{}' judge_llm eval error: {e}",
                        step.id
                    ));
                    break;
                }
            },
        };

        if passed {
            record(
                &mut events,
                &observer,
                TraceEvent::EvalPassed {
                    id: step.id.clone(),
                    attempt,
                },
            );
            if !step.post_actions.is_empty() {
                let pa_ctx = ExecContext {
                    workflow_inputs: &inputs,
                    steps: &state,
                };
                match driver.run_post_actions(step, &pa_ctx).await {
                    Ok(log) => record(
                        &mut events,
                        &observer,
                        TraceEvent::PostAction {
                            id: step.id.clone(),
                            log,
                        },
                    ),
                    Err(e) => {
                        status = WorkflowStatus::Failure(format!(
                            "step '{}' post_actions failed: {e}",
                            step.id
                        ));
                        break;
                    }
                }
            }
            state.get_mut(&step.id).unwrap().status = StepStatus::Done;
            if step.terminal_on_success {
                status = WorkflowStatus::TerminalSuccess(step.id.clone());
                record(
                    &mut events,
                    &observer,
                    TraceEvent::Terminated {
                        status: status.clone(),
                    },
                );
                return Trace {
                    events,
                    status,
                    final_state: state,
                };
            }
            continue;
        }

        // Eval failed.
        let st = state.get_mut(&step.id).unwrap();
        st.eval_failures += 1;
        let eval_failures = st.eval_failures;
        let max = eval.on_fail.max_attempts.unwrap_or(DEFAULT_MAX_ATTEMPTS);
        let exhausted = attempt >= max;
        let action_label = format!("{:?}", eval.on_fail.action);
        record(
            &mut events,
            &observer,
            TraceEvent::EvalFailed {
                id: step.id.clone(),
                attempt,
                action: action_label,
                eval_failures,
            },
        );

        if exhausted {
            let on_exhausted = eval
                .on_fail
                .on_exhausted
                .unwrap_or(OnExhausted::ExitFailure);
            record(
                &mut events,
                &observer,
                TraceEvent::Exhausted {
                    id: step.id.clone(),
                    on_exhausted: format!("{:?}", on_exhausted),
                    attempts: attempt,
                },
            );
            match on_exhausted {
                OnExhausted::ExitFailure => {
                    status = WorkflowStatus::Failure(format!(
                        "step '{}' exhausted after {attempt} attempts",
                        step.id
                    ));
                    break;
                }
                OnExhausted::Continue => {
                    state.get_mut(&step.id).unwrap().status = StepStatus::DoneWithFailure;
                    continue;
                }
                OnExhausted::BranchTo => {
                    let target = match resolve_branch_target(&eval.on_fail, &state, &step.id) {
                        Ok(target) => target,
                        Err(e) => {
                            status = WorkflowStatus::Failure(format!(
                                "step '{}' on_exhausted branch_to failed: {e}",
                                step.id
                            ));
                            break;
                        }
                    };
                    if !state.contains_key(&target) {
                        status = WorkflowStatus::Failure(format!(
                            "step '{}' on_exhausted branch_to unknown step '{target}'",
                            step.id
                        ));
                        break;
                    }
                    record(
                        &mut events,
                        &observer,
                        TraceEvent::BranchedTo {
                            from: step.id.clone(),
                            to: target.clone(),
                        },
                    );
                    // Reset the target + every transitive dependent
                    // (which includes the branching step itself,
                    // since the branching step's depends_on chain
                    // leads back to the target). The topo walker
                    // picks them up once the target re-completes.
                    reset_for_reentry(&mut state, &target);
                    reset_dependents_preserving(workflow, &mut state, &target, Some(&step.id));
                    // Defensive — if the branching step is NOT in
                    // the dependents chain (workflow author wired
                    // them up loosely), reset it explicitly too.
                    reset_for_reentry_preserve_outputs(&mut state, &step.id);
                }
            }
            continue;
        }

        // Not exhausted — dispatch the action.
        match eval.on_fail.action {
            OnFailAction::Repeat => {
                state.get_mut(&step.id).unwrap().status = StepStatus::Pending;
            }
            OnFailAction::RerunChain => {
                let ids = eval.on_fail.rerun.clone();
                record(
                    &mut events,
                    &observer,
                    TraceEvent::RerunChain {
                        from: step.id.clone(),
                        ids: ids.clone(),
                    },
                );
                for r in &ids {
                    if state.contains_key(r) {
                        reset_for_reentry(&mut state, r);
                    }
                }
                // Make sure the current step itself is reschedulable.
                state.get_mut(&step.id).unwrap().status = StepStatus::Pending;
            }
            OnFailAction::BranchTo => {
                let target = match resolve_branch_target(&eval.on_fail, &state, &step.id) {
                    Ok(target) => target,
                    Err(e) => {
                        status = WorkflowStatus::Failure(format!(
                            "step '{}' on_fail branch_to failed: {e}",
                            step.id
                        ));
                        break;
                    }
                };
                if !state.contains_key(&target) {
                    status = WorkflowStatus::Failure(format!(
                        "step '{}' on_fail branch_to unknown step '{target}'",
                        step.id
                    ));
                    break;
                }
                record(
                    &mut events,
                    &observer,
                    TraceEvent::BranchedTo {
                        from: step.id.clone(),
                        to: target.clone(),
                    },
                );
                reset_for_reentry(&mut state, &target);
                reset_dependents_preserving(workflow, &mut state, &target, Some(&step.id));
                reset_for_reentry_preserve_outputs(&mut state, &step.id);
            }
            OnFailAction::Continue => {
                state.get_mut(&step.id).unwrap().status = StepStatus::DoneWithFailure;
            }
            OnFailAction::ExitFailure => {
                status = WorkflowStatus::Failure(format!(
                    "step '{}' on_fail.action = exit_failure",
                    step.id
                ));
                break;
            }
        }
    }

    if !matches!(events.last(), Some(TraceEvent::Terminated { .. })) {
        record(
            &mut events,
            &observer,
            TraceEvent::Terminated {
                status: status.clone(),
            },
        );
    }

    // Final snapshot on terminal exit so --resume of a finished
    // workflow shows the right end state.
    snapshot_save(&state, events.len());

    Trace {
        events,
        status,
        final_state: state,
    }
}

/// Reset a step to Pending so the topo walker picks it up again.
/// Attempt counter is preserved so eval expressions like
/// `step.attempt <= 3` keep ticking across re-entries.
fn reset_for_reentry(state: &mut HashMap<String, StepState>, id: &str) {
    if let Some(st) = state.get_mut(id) {
        st.status = StepStatus::Pending;
        st.outputs.clear();
        st.preserved_outputs_on_skip.clear();
    }
}

fn reset_for_reentry_preserve_outputs(state: &mut HashMap<String, StepState>, id: &str) {
    if let Some(st) = state.get_mut(id) {
        st.status = StepStatus::Pending;
        st.preserved_outputs_on_skip.clear();
    }
}

fn resolve_branch_target(
    on_fail: &crate::workflow::OnFail,
    state: &HashMap<String, StepState>,
    current_step: &str,
) -> Result<String, String> {
    if let Some(target) = on_fail.branch_to.as_deref() {
        return Ok(target.to_string());
    }

    let Some(output_key) = on_fail.branch_to_output.as_deref() else {
        return Err("missing branch_to or branch_to_output".to_string());
    };
    let step = state
        .get(current_step)
        .ok_or_else(|| format!("current step '{current_step}' not found"))?;
    let value = step
        .outputs
        .get(output_key)
        .ok_or_else(|| format!("branch_to_output '{output_key}' is missing from step outputs"))?;
    let Some(target) = value.as_str() else {
        return Err(format!(
            "branch_to_output '{output_key}' must be a string, got {value}"
        ));
    };
    if target.is_empty() {
        return Err(format!("branch_to_output '{output_key}' is empty"));
    }
    Ok(target.to_string())
}

fn retry_driver_error(
    state: &mut HashMap<String, StepState>,
    events: &mut Vec<TraceEvent>,
    observer: &Option<EventObserver>,
    step: &Step,
    attempt: u32,
    detail: &str,
) -> bool {
    let Some(eval) = &step.eval else {
        return false;
    };
    let max = eval.on_fail.max_attempts.unwrap_or(DEFAULT_MAX_ATTEMPTS);
    if attempt >= max {
        return false;
    }
    let st = state.get_mut(&step.id).unwrap();
    st.eval_failures += 1;
    st.status = StepStatus::Pending;
    record(
        events,
        observer,
        TraceEvent::EvalFailed {
            id: step.id.clone(),
            attempt,
            action: format!("Repeat ({detail})"),
            eval_failures: st.eval_failures,
        },
    );
    true
}

/// Combine per-lens output maps into one aggregated map per the
/// step's `aggregate` strategy.
///
/// `Concat` (default): for each declared output key, if every lens
/// produced an array, concatenate them in lens order. Otherwise
/// build `[{lens, value}, ...]` so callers don't lose which lens
/// said what.
///
/// `ByLens`: every declared key maps to `{lens_id_1: value_1,
/// lens_id_2: value_2, ...}`.
///
/// Lens iteration order matches `step.lenses` order to keep
/// downstream consumers deterministic.
fn aggregate_lens_outputs(
    step: &Step,
    per_lens: &[(String, Map<String, Value>)],
    strategy: Aggregate,
) -> Map<String, Value> {
    // Consolidate is dispatched via `Driver::consolidate` before
    // this function gets called. Treat it as Concat so a future
    // refactor calling this with Consolidate doesn't silently drop
    // data.
    let strategy = if matches!(strategy, Aggregate::Consolidate) {
        Aggregate::Concat
    } else {
        strategy
    };
    let mut out = Map::new();
    for key in step.outputs.keys() {
        match strategy {
            Aggregate::Concat => {
                let all_arrays = per_lens
                    .iter()
                    .all(|(_, m)| matches!(m.get(key), Some(Value::Array(_))));
                if all_arrays && !per_lens.is_empty() {
                    let mut combined: Vec<Value> = Vec::new();
                    for (lens_id, m) in per_lens {
                        if let Some(Value::Array(a)) = m.get(key) {
                            for v in a {
                                // Tag each item with its source
                                // lens so downstream consumers can
                                // group/dedupe. Mirrors the kres
                                // findings model where every
                                // finding carries a `lens` field.
                                let tagged = match v {
                                    Value::Object(o) => {
                                        let mut o = o.clone();
                                        o.entry("lens".to_string())
                                            .or_insert_with(|| Value::String(lens_id.clone()));
                                        Value::Object(o)
                                    }
                                    other => Value::Object({
                                        let mut o = Map::new();
                                        o.insert("lens".into(), Value::String(lens_id.clone()));
                                        o.insert("value".into(), other.clone());
                                        o
                                    }),
                                };
                                combined.push(tagged);
                            }
                        }
                    }
                    out.insert(key.clone(), Value::Array(combined));
                } else {
                    let mut pairs: Vec<Value> = Vec::new();
                    for (lens_id, m) in per_lens {
                        let mut o = Map::new();
                        o.insert("lens".into(), Value::String(lens_id.clone()));
                        o.insert("value".into(), m.get(key).cloned().unwrap_or(Value::Null));
                        pairs.push(Value::Object(o));
                    }
                    out.insert(key.clone(), Value::Array(pairs));
                }
            }
            Aggregate::ByLens => {
                let mut by = Map::new();
                for (lens_id, m) in per_lens {
                    by.insert(lens_id.clone(), m.get(key).cloned().unwrap_or(Value::Null));
                }
                out.insert(key.clone(), Value::Object(by));
            }
            Aggregate::Consolidate => unreachable!("normalised to Concat above"),
        }
    }
    out
}

/// Reset every step that depends_on `target` (transitively) so the
/// graph re-runs them after the target re-completes.
fn reset_dependents_preserving(
    workflow: &Workflow,
    state: &mut HashMap<String, StepState>,
    target: &str,
    preserve_outputs_for: Option<&str>,
) {
    let mut to_reset: Vec<String> = vec![target.to_string()];
    let mut i = 0;
    while i < to_reset.len() {
        let cur = to_reset[i].clone();
        i += 1;
        for s in &workflow.steps {
            if s.depends_on.iter().any(|d| d == &cur) && !to_reset.iter().any(|x| x == &s.id) {
                to_reset.push(s.id.clone());
            }
        }
    }
    for id in to_reset {
        if id == target {
            continue;
        }
        if let Some(st) = state.get_mut(&id) {
            // Only reset settled steps; don't disturb something
            // that's still Pending/BranchedAway in flight.
            if matches!(
                st.status,
                StepStatus::Done | StepStatus::Skipped | StepStatus::DoneWithFailure
            ) {
                st.status = StepStatus::Pending;
                let preserve_for_skip = workflow
                    .steps
                    .iter()
                    .any(|s| s.id == id && s.preserve_outputs_on_skip);
                if preserve_outputs_for == Some(id.as_str()) {
                    st.preserved_outputs_on_skip.clear();
                } else if preserve_for_skip {
                    st.preserved_outputs_on_skip = std::mem::take(&mut st.outputs);
                } else {
                    st.outputs.clear();
                    st.preserved_outputs_on_skip.clear();
                }
            }
        }
    }
}

/// Scripted driver: returns outputs from a `(step_id, attempt)` →
/// outputs table. Use `with(step, attempt, json!({...}))` to build.
#[derive(Default)]
pub struct ScriptedDriver {
    table: BTreeMap<(String, u32), Map<String, Value>>,
    fallback: Map<String, Value>,
}

impl ScriptedDriver {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with(mut self, step_id: &str, attempt: u32, outputs: Value) -> Self {
        let map = match outputs {
            Value::Object(m) => m,
            _ => panic!("ScriptedDriver outputs must be a JSON object"),
        };
        self.table.insert((step_id.to_string(), attempt), map);
        self
    }
    /// Outputs returned for any (step, attempt) not in the table.
    /// Default is an empty object — most steps without an eval just
    /// need _something_ non-erroring.
    pub fn default_output(mut self, outputs: Value) -> Self {
        match outputs {
            Value::Object(m) => self.fallback = m,
            _ => panic!("default_output must be an object"),
        }
        self
    }
}

#[async_trait]
impl Driver for ScriptedDriver {
    async fn run(
        &self,
        step: &Step,
        attempt: u32,
        _ctx: &ExecContext<'_>,
        lens: Option<&Lens>,
    ) -> Result<Map<String, Value>, String> {
        // Lensed lookup: tests script outputs by
        // `(step_id|lens_id, attempt)` so each lens variant can
        // produce its own outputs per attempt. Plain steps fall
        // back to the bare `step_id` key.
        let key_id = match lens {
            Some(l) => format!("{}|{}", step.id, l.id),
            None => step.id.clone(),
        };
        Ok(self
            .table
            .get(&(key_id, attempt))
            .cloned()
            .unwrap_or_else(|| self.fallback.clone()))
    }

    async fn consolidate(
        &self,
        step: &Step,
        ctx: &ExecContext<'_>,
        _per_lens: &[(String, Map<String, Value>)],
    ) -> Result<Map<String, Value>, String> {
        // Consolidate phase scripted under `<step_id>|@consolidate`.
        // Tests verify the executor invoked this method; the actual
        // dedup logic is exercised separately in the LlmDriver
        // tests.
        let key = format!("{}|@consolidate", step.id);
        let attempt = ctx.steps.get(&step.id).map(|s| s.attempt).unwrap_or(1);
        Ok(self
            .table
            .get(&(key, attempt))
            .cloned()
            .unwrap_or_else(|| self.fallback.clone()))
    }

    async fn judge(&self, _step: &Step, _ctx: &ExecContext<'_>) -> Result<(bool, String), String> {
        Ok((true, "scripted judge accepts".into()))
    }
}

// ---------------------------------------------------------------------------
// Expression evaluator
// ---------------------------------------------------------------------------

pub mod expr {
    use super::ExecContext;
    use serde_json::Value;

    /// Evaluate `src` against `ctx`. `current_step` is the step whose
    /// outputs are visible via bare identifiers (no dot). Returns
    /// the boolean result or a parser/runtime error string.
    pub fn eval(src: &str, ctx: &ExecContext, current_step: Option<&str>) -> Result<bool, String> {
        let toks = tokenize(src)?;
        let mut p = Parser { toks, pos: 0 };
        let expr = p.parse_or()?;
        if p.pos != p.toks.len() {
            return Err(format!("trailing tokens at pos {}", p.pos));
        }
        eval_expr(&expr, ctx, current_step)
    }

    #[derive(Debug, Clone, PartialEq)]
    enum Token {
        Ident(String),
        Dot,
        Op(Op),
        And,
        Or,
        LParen,
        RParen,
        Str(String),
        Int(i64),
        Bool(bool),
    }

    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Op {
        Eq,
        Neq,
        Lt,
        Le,
        Gt,
        Ge,
    }

    fn tokenize(s: &str) -> Result<Vec<Token>, String> {
        let mut out = Vec::new();
        let mut chars = s.chars().peekable();
        while let Some(&c) = chars.peek() {
            if c.is_whitespace() {
                chars.next();
                continue;
            }
            if c.is_ascii_alphabetic() || c == '_' {
                let mut s = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                        s.push(c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                match s.as_str() {
                    "true" => out.push(Token::Bool(true)),
                    "false" => out.push(Token::Bool(false)),
                    _ => out.push(Token::Ident(s)),
                }
                continue;
            }
            if c.is_ascii_digit() {
                let mut s = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_ascii_digit() {
                        s.push(c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                out.push(Token::Int(s.parse().map_err(|e| format!("int: {e}"))?));
                continue;
            }
            if c == '\'' {
                chars.next();
                let mut s = String::new();
                let mut closed = false;
                while let Some(&c) = chars.peek() {
                    if c == '\'' {
                        chars.next();
                        closed = true;
                        break;
                    }
                    s.push(c);
                    chars.next();
                }
                if !closed {
                    return Err("unterminated string".into());
                }
                out.push(Token::Str(s));
                continue;
            }
            match c {
                '.' => {
                    chars.next();
                    out.push(Token::Dot);
                }
                '(' => {
                    chars.next();
                    out.push(Token::LParen);
                }
                ')' => {
                    chars.next();
                    out.push(Token::RParen);
                }
                '&' => {
                    chars.next();
                    if chars.next() != Some('&') {
                        return Err("expected '&&'".into());
                    }
                    out.push(Token::And);
                }
                '|' => {
                    chars.next();
                    if chars.next() != Some('|') {
                        return Err("expected '||'".into());
                    }
                    out.push(Token::Or);
                }
                '=' => {
                    chars.next();
                    if chars.next() != Some('=') {
                        return Err("expected '=='".into());
                    }
                    out.push(Token::Op(Op::Eq));
                }
                '!' => {
                    chars.next();
                    if chars.next() != Some('=') {
                        return Err("expected '!='".into());
                    }
                    out.push(Token::Op(Op::Neq));
                }
                '<' => {
                    chars.next();
                    if chars.peek() == Some(&'=') {
                        chars.next();
                        out.push(Token::Op(Op::Le));
                    } else {
                        out.push(Token::Op(Op::Lt));
                    }
                }
                '>' => {
                    chars.next();
                    if chars.peek() == Some(&'=') {
                        chars.next();
                        out.push(Token::Op(Op::Ge));
                    } else {
                        out.push(Token::Op(Op::Gt));
                    }
                }
                _ => return Err(format!("unexpected char '{c}'")),
            }
        }
        Ok(out)
    }

    #[derive(Debug, Clone)]
    enum Expr {
        Or(Box<Expr>, Box<Expr>),
        And(Box<Expr>, Box<Expr>),
        Cmp(Path, Op, Atom),
    }

    #[derive(Debug, Clone)]
    struct Path(Vec<String>);

    #[derive(Debug, Clone)]
    enum Atom {
        Path(Path),
        Str(String),
        Int(i64),
        Bool(bool),
    }

    struct Parser {
        toks: Vec<Token>,
        pos: usize,
    }

    impl Parser {
        fn peek(&self) -> Option<&Token> {
            self.toks.get(self.pos)
        }
        fn next(&mut self) -> Option<Token> {
            let t = self.toks.get(self.pos).cloned();
            if t.is_some() {
                self.pos += 1;
            }
            t
        }

        fn parse_or(&mut self) -> Result<Expr, String> {
            let mut e = self.parse_and()?;
            while matches!(self.peek(), Some(Token::Or)) {
                self.next();
                let r = self.parse_and()?;
                e = Expr::Or(Box::new(e), Box::new(r));
            }
            Ok(e)
        }
        fn parse_and(&mut self) -> Result<Expr, String> {
            let mut e = self.parse_cmp()?;
            while matches!(self.peek(), Some(Token::And)) {
                self.next();
                let r = self.parse_cmp()?;
                e = Expr::And(Box::new(e), Box::new(r));
            }
            Ok(e)
        }
        fn parse_cmp(&mut self) -> Result<Expr, String> {
            if matches!(self.peek(), Some(Token::LParen)) {
                self.next();
                let inner = self.parse_or()?;
                if !matches!(self.next(), Some(Token::RParen)) {
                    return Err("expected ')'".into());
                }
                return Ok(inner);
            }
            let path = self.parse_path()?;
            let op = match self.next() {
                Some(Token::Op(o)) => o,
                Some(t) => return Err(format!("expected comparison op, got {t:?}")),
                None => return Err("expected comparison op".into()),
            };
            let rhs = self.parse_atom()?;
            Ok(Expr::Cmp(path, op, rhs))
        }
        fn parse_path(&mut self) -> Result<Path, String> {
            let mut parts = Vec::new();
            match self.next() {
                Some(Token::Ident(s)) => parts.push(s),
                t => return Err(format!("expected ident, got {t:?}")),
            }
            while matches!(self.peek(), Some(Token::Dot)) {
                self.next();
                match self.next() {
                    Some(Token::Ident(s)) => parts.push(s),
                    t => return Err(format!("expected ident after '.', got {t:?}")),
                }
            }
            Ok(Path(parts))
        }
        fn parse_atom(&mut self) -> Result<Atom, String> {
            match self.next() {
                Some(Token::Str(s)) => Ok(Atom::Str(s)),
                Some(Token::Int(n)) => Ok(Atom::Int(n)),
                Some(Token::Bool(b)) => Ok(Atom::Bool(b)),
                Some(Token::Ident(s)) => {
                    let mut parts = vec![s];
                    while matches!(self.peek(), Some(Token::Dot)) {
                        self.next();
                        match self.next() {
                            Some(Token::Ident(s)) => parts.push(s),
                            t => return Err(format!("expected ident after '.', got {t:?}")),
                        }
                    }
                    Ok(Atom::Path(Path(parts)))
                }
                t => Err(format!("expected literal or path, got {t:?}")),
            }
        }
    }

    fn eval_expr(e: &Expr, ctx: &ExecContext, current: Option<&str>) -> Result<bool, String> {
        match e {
            Expr::Or(a, b) => Ok(eval_expr(a, ctx, current)? || eval_expr(b, ctx, current)?),
            Expr::And(a, b) => Ok(eval_expr(a, ctx, current)? && eval_expr(b, ctx, current)?),
            Expr::Cmp(path, op, rhs) => {
                let l = resolve(path, ctx, current)?;
                let r = match rhs {
                    Atom::Path(p) => resolve(p, ctx, current)?,
                    Atom::Str(s) => Value::String(s.clone()),
                    Atom::Int(n) => Value::Number((*n).into()),
                    Atom::Bool(b) => Value::Bool(*b),
                };
                cmp(&l, *op, &r)
            }
        }
    }

    fn cmp(a: &Value, op: Op, b: &Value) -> Result<bool, String> {
        match op {
            Op::Eq => Ok(values_eq(a, b)),
            Op::Neq => Ok(!values_eq(a, b)),
            Op::Lt | Op::Le | Op::Gt | Op::Ge => {
                let ai = a
                    .as_i64()
                    .ok_or_else(|| format!("ordering needs int, got {a:?}"))?;
                let bi = b
                    .as_i64()
                    .ok_or_else(|| format!("ordering needs int, got {b:?}"))?;
                Ok(match op {
                    Op::Lt => ai < bi,
                    Op::Le => ai <= bi,
                    Op::Gt => ai > bi,
                    Op::Ge => ai >= bi,
                    _ => unreachable!(),
                })
            }
        }
    }

    fn values_eq(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::String(x), Value::String(y)) => x == y,
            (Value::Bool(x), Value::Bool(y)) => x == y,
            (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
            (Value::Null, Value::Null) => true,
            _ => false,
        }
    }

    fn resolve(path: &Path, ctx: &ExecContext, current: Option<&str>) -> Result<Value, String> {
        let parts = &path.0;
        if parts.is_empty() {
            return Err("empty path".into());
        }
        if parts.len() == 1 {
            // Bare ident — current step's output, or a workflow input.
            let name = &parts[0];
            if let Some(cur) = current {
                if let Some(st) = ctx.steps.get(cur) {
                    if name == "attempt" {
                        return Ok(Value::Number(st.attempt.into()));
                    }
                    if name == "eval_failures" {
                        return Ok(Value::Number(st.eval_failures.into()));
                    }
                    if let Some(v) = st.outputs.get(name) {
                        return Ok(v.clone());
                    }
                }
            }
            if let Some(v) = ctx.workflow_inputs.get(name) {
                return Ok(v.clone());
            }
            return Err(format!(
                "name '{name}' not bound in current step or workflow"
            ));
        }
        if parts[0] == "workflow" {
            let mut cur = Value::Object(ctx.workflow_inputs.clone());
            for p in &parts[1..] {
                cur = cur
                    .get(p)
                    .cloned()
                    .ok_or_else(|| format!("workflow.{p} not found"))?;
            }
            return Ok(cur);
        }
        let st = ctx
            .steps
            .get(&parts[0])
            .ok_or_else(|| format!("step '{}' not in context", parts[0]))?;
        if parts.len() == 2 && parts[1] == "attempt" {
            return Ok(Value::Number(st.attempt.into()));
        }
        if parts.len() == 2 && parts[1] == "eval_failures" {
            return Ok(Value::Number(st.eval_failures.into()));
        }
        let mut cur = st
            .outputs
            .get(&parts[1])
            .cloned()
            .ok_or_else(|| format!("{}.{} not in outputs", parts[0], parts[1]))?;
        for p in &parts[2..] {
            cur = cur
                .get(p)
                .cloned()
                .ok_or_else(|| format!("path beyond {}.{}", parts[0], parts[1]))?;
        }
        Ok(cur)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::parse_workflow;
    use serde_json::json;

    fn fix_workflow() -> Workflow {
        parse_workflow(include_str!("../../configs/workflows/fix.json")).unwrap()
    }

    fn review_workflow() -> Workflow {
        parse_workflow(include_str!("../../configs/workflows/review.json")).unwrap()
    }

    fn lensed_review_workflow() -> Workflow {
        let wf_json = serde_json::json!({
            "$schema_version": 1,
            "id": "lensed-review-test",
            "steps": [{
                "id": "investigate",
                "agent": "slow",
                "prompt": "lens={{lens.id}}",
                "lenses": [
                    {"id": "lifetime"},
                    {"id": "memory"},
                    {"id": "bounds"},
                    {"id": "races"},
                    {"id": "general"}
                ],
                "aggregate": "consolidate",
                "consolidate": {"agent": "fast", "prompt": "merge"},
                "outputs": {"findings": {"type": "array<object>"}}
            }]
        });
        parse_workflow(&wf_json.to_string()).unwrap()
    }

    fn target_inputs() -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("target".into(), json!("drivers/example/example.c"));
        m.insert("target_is_commit".into(), json!(false));
        m
    }

    #[test]
    fn format_event_summarizes_long_outputs() {
        let mut outputs = Map::new();
        outputs.insert(
            "analysis".into(),
            json!("The patch adds the missing cleanup call. This pairs object lifetime correctly. Review is clean. The reviewer also checked cleanup ordering, synchronous completion, reference handling, commit message wording, error paths, lock coverage, and NULL handling. Extra detail should not flood the terminal."),
        );
        outputs.insert("clean".into(), json!(true));
        outputs.insert("defects".into(), json!([]));

        let line = format_event(&TraceEvent::StepProduced {
            id: "review".into(),
            attempt: 1,
            outputs,
        });

        assert!(line.contains("✓ produced review (attempt 1)"));
        assert!(line.contains("analysis=\"The patch adds the missing cleanup call."));
        assert!(line.contains("clean=true"));
        assert!(line.contains("defects=[]"));
        assert!(!line.contains("Extra detail should not flood"));
    }

    #[tokio::test]
    async fn shipped_review_workflow_uses_parallel_lenses() {
        let wf = review_workflow();
        assert_eq!(wf.steps.len(), 1);
        assert_eq!(wf.steps[0].id, "investigate");
        assert_eq!(wf.steps[0].lenses.len(), 6);
        assert!(wf.steps[0].consolidate.is_some());
        assert!(wf.steps[0].outputs.contains_key("analysis"));
        assert_eq!(
            wf.steps[0].eval.as_ref().and_then(|e| e.expr.as_deref()),
            Some("analysis != ''")
        );
        let mut driver = ScriptedDriver::new()
            .with(
                "investigate|lifetime",
                1,
                json!({"analysis": "checked lifetime", "findings": []}),
            )
            .with(
                "investigate|memory",
                1,
                json!({"analysis": "checked memory", "findings": []}),
            )
            .with(
                "investigate|bounds",
                1,
                json!({"analysis": "checked bounds", "findings": []}),
            )
            .with(
                "investigate|races",
                1,
                json!({"analysis": "checked races", "findings": []}),
            )
            .with(
                "investigate|general",
                1,
                json!({"analysis": "checked general", "findings": []}),
            )
            .with(
                "investigate|@consolidate",
                1,
                json!({
                    "analysis": "clean review",
                    "findings": [],
                    "followups": [],
                    "followups_empty": true
                }),
            );
        let trace = run(&wf, &mut driver, target_inputs()).await;
        assert_eq!(trace.status, WorkflowStatus::Success);
        assert!(trace.events.iter().any(|e| matches!(
            e,
            TraceEvent::FanOut { id, lens_ids, .. } if id == "investigate" && lens_ids.len() == 5
        )));
        assert!(trace.events.iter().any(|e| matches!(
            e,
            TraceEvent::Consolidating { id, lens_count, .. } if id == "investigate" && *lens_count == 5
        )));
    }

    #[test]
    fn shipped_review_workflow_requests_full_findings() {
        let wf = review_workflow();
        let step = &wf.steps[0];
        let prompt = step.prompt.as_deref().unwrap();
        let review_intro = wf
            .globals
            .get("review_intro")
            .and_then(|v| v.as_str())
            .unwrap();
        let contract_trace = wf
            .globals
            .get("contract_trace")
            .and_then(|v| v.as_str())
            .unwrap();
        let schema = wf
            .globals
            .get("finding_schema")
            .and_then(|v| v.as_str())
            .unwrap();
        let consolidate = step.consolidate.as_ref().unwrap().prompt.as_str();
        let eval = step.eval.as_ref().unwrap();

        assert!(schema.contains("full Finding records"));
        assert!(schema.contains("relevant_symbols"));
        assert!(schema.contains("\"followups\""));
        assert!(review_intro.contains("Find every concrete correctness bug"));
        assert!(review_intro.contains("Do not stop after the first issue"));
        assert!(review_intro.contains("A clean result is valid only"));
        assert!(review_intro.contains("concrete unresolved suspicion is not a clean result"));
        assert!(review_intro.contains("chains of events"));
        assert!(contract_trace.contains("semantic change, not just the edited lines"));
        assert!(contract_trace.contains("callback target"));
        assert!(contract_trace.contains("Negative coverage claims require concrete evidence"));
        assert!(contract_trace.contains("source/type/search/callgraph/history context"));
        assert!(contract_trace.contains("Do not hardcode"));
        assert!(step.include.iter().any(|i| i.contains("contract_trace")));
        assert!(step.include.iter().any(|i| i.contains("finding_schema")));
        assert!(step.outputs.contains_key("analysis"));
        assert!(step.outputs.contains_key("findings"));
        assert!(step.outputs.contains_key("followups"));
        assert!(step.outputs.contains_key("followups_empty"));
        assert_eq!(
            step.outputs["findings"]
                .get("type")
                .and_then(|v| v.as_str()),
            Some("array<Finding>")
        );
        assert_eq!(
            step.outputs["followups"]
                .get("type")
                .and_then(|v| v.as_str()),
            Some("array<Followup>")
        );
        assert_eq!(eval.kind, crate::workflow::EvalKind::FieldCheck);
        assert_eq!(eval.expr.as_deref(), Some("analysis != ''"));
        assert_eq!(eval.on_fail.action, crate::workflow::OnFailAction::Repeat);
        assert_eq!(eval.on_fail.max_attempts, Some(3));
        assert!(!wf
            .completion
            .as_ref()
            .unwrap()
            .failure_when_any
            .iter()
            .any(|expr| expr == "investigate.followups_empty == false"));
        assert!(prompt.contains("full Finding record"));
        assert!(prompt.contains("do not emit simplified"));
        assert!(prompt.contains("Find every bug you can involving the target"));
        assert!(prompt.contains("Followups are the normal next frontier"));
        assert!(prompt.contains("trace the semantic contracts changed by the diff"));
        assert!(prompt.contains("callers, callees, callbacks"));
        assert!(prompt.contains("old path unreachable"));
        assert!(prompt.contains("caller/callee"));
        assert!(prompt.contains("specific concern is plausible"));
        assert!(prompt.contains("strong-suspect Finding"));
        assert!(
            prompt.contains("represented by a Finding with open_questions or by a typed followup")
        );
        assert!(!prompt.contains("file: file:line citation"));
        assert!(consolidate.contains("full Finding records"));
        assert!(consolidate.contains("concrete unresolved suspicion is not droppable"));
        assert!(consolidate.contains("strong-suspect Finding with open_questions"));
        assert!(consolidate.contains("`followups`"));
        assert!(consolidate.contains("entry naming the exact"));
        assert!(consolidate.contains("unsupported negative coverage claims"));
        assert!(consolidate.contains("`followups_empty: true`"));
        assert!(!consolidate.contains("set `lenses`"));
    }

    #[tokio::test]
    async fn shipped_review_workflow_preserves_followups_for_outer_loop() {
        let wf = review_workflow();
        let first_followup = json!([{
            "type": "read",
            "name": "drivers/example/example.c:1+40",
            "reason": "need surrounding source to prove the concern"
        }]);
        let mut driver = ScriptedDriver::new()
            .with(
                "investigate|lifetime",
                1,
                json!({"analysis": "needs more source", "findings": [], "followups": []}),
            )
            .with(
                "investigate|memory",
                1,
                json!({"analysis": "needs more source", "findings": [], "followups": []}),
            )
            .with(
                "investigate|bounds",
                1,
                json!({"analysis": "needs more source", "findings": [], "followups": []}),
            )
            .with(
                "investigate|races",
                1,
                json!({"analysis": "needs more source", "findings": [], "followups": []}),
            )
            .with(
                "investigate|general",
                1,
                json!({"analysis": "needs more source", "findings": [], "followups": []}),
            )
            .with(
                "investigate|@consolidate",
                1,
                json!({
                    "analysis": "not done; followup needed",
                    "findings": [],
                    "followups": first_followup,
                    "followups_empty": false
                }),
            );

        let trace = run_with_cap(&wf, &mut driver, target_inputs(), 20).await;

        assert!(matches!(
            trace.status,
            WorkflowStatus::Success | WorkflowStatus::TerminalSuccess(_)
        ));
        let attempts: Vec<u32> = trace
            .events
            .iter()
            .filter_map(|e| match e {
                TraceEvent::StepStarted { id, attempt } if id == "investigate" => Some(*attempt),
                _ => None,
            })
            .collect();
        assert_eq!(attempts, vec![1]);
        assert!(!trace.events.iter().any(|e| matches!(
            e,
            TraceEvent::EvalFailed { id, .. } if id == "investigate"
        )));
        let followups = trace
            .final_state
            .get("investigate")
            .and_then(|s| s.outputs.get("followups"))
            .and_then(|v| v.as_array())
            .expect("followups output");
        assert_eq!(followups.len(), 1);
    }

    #[tokio::test]
    async fn shipped_review_workflow_repeats_for_empty_analysis() {
        let wf = review_workflow();
        let mut driver = ScriptedDriver::new()
            .with(
                "investigate|lifetime",
                1,
                json!({"analysis": "", "findings": [], "followups": []}),
            )
            .with(
                "investigate|memory",
                1,
                json!({"analysis": "", "findings": [], "followups": []}),
            )
            .with(
                "investigate|bounds",
                1,
                json!({"analysis": "", "findings": [], "followups": []}),
            )
            .with(
                "investigate|races",
                1,
                json!({"analysis": "", "findings": [], "followups": []}),
            )
            .with(
                "investigate|general",
                1,
                json!({"analysis": "", "findings": [], "followups": []}),
            )
            .with(
                "investigate|@consolidate",
                1,
                json!({
                    "analysis": "",
                    "findings": [],
                    "followups": [],
                    "followups_empty": true
                }),
            )
            .with(
                "investigate|lifetime",
                2,
                json!({"analysis": "checked lifetime", "findings": [], "followups": []}),
            )
            .with(
                "investigate|memory",
                2,
                json!({"analysis": "checked memory", "findings": [], "followups": []}),
            )
            .with(
                "investigate|bounds",
                2,
                json!({"analysis": "checked bounds", "findings": [], "followups": []}),
            )
            .with(
                "investigate|races",
                2,
                json!({"analysis": "checked races", "findings": [], "followups": []}),
            )
            .with(
                "investigate|general",
                2,
                json!({"analysis": "checked general", "findings": [], "followups": []}),
            )
            .with(
                "investigate|@consolidate",
                2,
                json!({
                    "analysis": "clean review after a real pass",
                    "findings": [],
                    "followups": [],
                    "followups_empty": true
                }),
            );

        let trace = run_with_cap(&wf, &mut driver, target_inputs(), 20).await;

        assert!(matches!(
            trace.status,
            WorkflowStatus::Success | WorkflowStatus::TerminalSuccess(_)
        ));
        assert!(trace.events.iter().any(|e| matches!(
            e,
            TraceEvent::EvalFailed { id, attempt: 1, .. } if id == "investigate"
        )));
        assert!(trace.events.iter().any(|e| matches!(
            e,
            TraceEvent::EvalPassed { id, attempt: 2 } if id == "investigate"
        )));
    }

    /// Lensed step fans out, every lens returns a finding, aggregator
    /// concatenates the per-lens `findings` arrays and tags each entry
    /// with its source lens. Uses an inline `aggregate: concat`
    /// workflow so the executor feature is tested independently of
    /// the shipped review workflow.
    #[tokio::test]
    async fn lens_fan_out_concat_aggregates_findings() {
        let wf_json = serde_json::json!({
            "$schema_version": 1,
            "id": "concat-test",
            "steps": [{
                "id": "investigate",
                "agent": "slow",
                "prompt": "lens={{lens.id}}",
                "lenses": [
                    {"id": "lifetime"},
                    {"id": "memory"},
                    {"id": "bounds"},
                    {"id": "races"},
                    {"id": "general"}
                ],
                "aggregate": "concat",
                "outputs": {
                    "findings": {"type": "array<object>"}
                }
            }]
        });
        let wf = parse_workflow(&wf_json.to_string()).unwrap();
        let mut driver = ScriptedDriver::new()
            .with(
                "investigate|lifetime",
                1,
                json!({"findings": [{"file": "x.c:1", "what": "leak", "severity": "high"}]}),
            )
            .with(
                "investigate|memory",
                1,
                json!({"findings": [{"file": "x.c:2", "what": "uaf", "severity": "high"}]}),
            )
            .with(
                "investigate|bounds",
                1,
                json!({"findings": [{"file": "x.c:3", "what": "ovf", "severity": "medium"}]}),
            )
            .with("investigate|races", 1, json!({"findings": []}))
            .with(
                "investigate|general",
                1,
                json!({"findings": [{"file": "x.c:4", "what": "null", "severity": "low"}]}),
            );
        let trace = run(&wf, &mut driver, target_inputs()).await;
        eprintln!("{}", trace.pretty());
        assert_eq!(trace.status, WorkflowStatus::Success);

        // FanOut event names every lens.
        let fan_out = trace
            .events
            .iter()
            .find_map(|e| match e {
                TraceEvent::FanOut { id, lens_ids, .. } if id == "investigate" => {
                    Some(lens_ids.clone())
                }
                _ => None,
            })
            .expect("FanOut for investigate");
        assert_eq!(
            fan_out,
            vec!["lifetime", "memory", "bounds", "races", "general"]
        );

        // Five LensProduced events, one per lens.
        let lens_count = trace
            .events
            .iter()
            .filter(|e| matches!(e, TraceEvent::LensProduced { id, .. } if id == "investigate"))
            .count();
        assert_eq!(lens_count, 5);

        // FanIn aggregated findings — 4 (one lens returned []), each
        // tagged with its source lens.
        let aggregated = trace
            .events
            .iter()
            .find_map(|e| match e {
                TraceEvent::FanIn { id, aggregated, .. } if id == "investigate" => {
                    Some(aggregated.clone())
                }
                _ => None,
            })
            .expect("FanIn for investigate");
        let findings = aggregated.get("findings").unwrap().as_array().unwrap();
        assert_eq!(findings.len(), 4, "4 non-empty lens findings");
        let lenses: Vec<String> = findings
            .iter()
            .map(|f| f.get("lens").unwrap().as_str().unwrap().to_string())
            .collect();
        assert!(lenses.contains(&"lifetime".to_string()));
        assert!(lenses.contains(&"memory".to_string()));
        assert!(lenses.contains(&"bounds".to_string()));
        assert!(lenses.contains(&"general".to_string()));
        assert!(!lenses.contains(&"races".to_string()));
    }

    /// One lens errors → workflow fails with a message naming the
    /// lens. Verifies the fan-out's error-aggregation surfaces the
    /// right step + lens.
    #[tokio::test]
    async fn lens_error_terminates_workflow() {
        // Custom driver: returns an Err for the "races" lens, OK for
        // the rest.
        struct PartiallyFailingDriver;
        #[async_trait]
        impl Driver for PartiallyFailingDriver {
            async fn run(
                &self,
                step: &Step,
                _attempt: u32,
                _ctx: &ExecContext<'_>,
                lens: Option<&Lens>,
            ) -> Result<Map<String, Value>, String> {
                if step.id == "investigate" {
                    if lens.map(|l| l.id.as_str()) == Some("races") {
                        return Err("racy lens blew up".into());
                    }
                    return Ok(serde_json::from_value(json!({"findings": []})).unwrap());
                }
                Ok(Map::new())
            }
        }

        let wf = lensed_review_workflow();
        let mut driver = PartiallyFailingDriver;
        let trace = run(&wf, &mut driver, target_inputs()).await;
        eprintln!("{}", trace.pretty());
        match &trace.status {
            WorkflowStatus::Failure(msg) => {
                assert!(msg.contains("investigate"), "got: {msg}");
                assert!(msg.contains("races"), "got: {msg}");
            }
            other => panic!("expected Failure, got {other:?}"),
        }
    }

    /// `aggregate: consolidate` runs an N+1 LLM call after the
    /// fan-out. The scripted driver returns scripted per-lens
    /// outputs, then a scripted consolidate response (looked up by
    /// `<step_id>|@consolidate`). The aggregated outputs in the
    /// FanIn event come from the consolidate response, not from
    /// concat'ing the lens outputs.
    #[tokio::test]
    async fn lens_consolidate_runs_n_plus_one_call() {
        let wf = lensed_review_workflow();
        let mut driver = ScriptedDriver::new()
            // Per-lens outputs — three lenses report the same bug
            // in slightly different words; two report distinct ones.
            .with(
                "investigate|lifetime",
                1,
                json!({"findings": [{"file": "x.c:42", "what": "leak in foo()", "severity": "high"}]}),
            )
            .with(
                "investigate|memory",
                1,
                json!({"findings": [{"file": "x.c:42", "what": "foo() forgets to release ref", "severity": "high"}]}),
            )
            .with(
                "investigate|bounds",
                1,
                json!({"findings": [{"file": "y.c:7", "what": "ovf", "severity": "medium"}]}),
            )
            .with("investigate|races", 1, json!({"findings": []}))
            .with(
                "investigate|general",
                1,
                json!({"findings": [{"file": "z.c:1", "what": "null deref", "severity": "low"}]}),
            )
            // Scripted consolidate response — the LLM "merged" the
            // two leak findings into one tagged with both lenses.
            .with(
                "investigate|@consolidate",
                1,
                json!({
                    "findings": [
                        {"file": "x.c:42", "what": "leak in foo()", "severity": "high",
                         "lenses": ["lifetime", "memory"]},
                        {"file": "y.c:7",  "what": "ovf",            "severity": "medium",
                         "lenses": ["bounds"]},
                        {"file": "z.c:1",  "what": "null deref",     "severity": "low",
                         "lenses": ["general"]}
                    ]
                }),
            );

        let trace = run(&wf, &mut driver, target_inputs()).await;
        eprintln!("{}", trace.pretty());
        assert_eq!(trace.status, WorkflowStatus::Success);

        // Consolidating event records the call.
        let consolidating = trace
            .events
            .iter()
            .find(|e| matches!(e, TraceEvent::Consolidating { id, .. } if id == "investigate"));
        assert!(consolidating.is_some(), "expected Consolidating event");
        if let Some(TraceEvent::Consolidating { lens_count, .. }) = consolidating {
            assert_eq!(*lens_count, 5);
        }

        // FanIn carries the consolidate response (3 findings, not 4
        // tagged-by-lens entries from a concat path).
        let aggregated = trace
            .events
            .iter()
            .find_map(|e| match e {
                TraceEvent::FanIn {
                    aggregated,
                    strategy,
                    ..
                } if strategy == "Consolidate" => Some(aggregated.clone()),
                _ => None,
            })
            .expect("FanIn with Consolidate strategy");
        let findings = aggregated.get("findings").unwrap().as_array().unwrap();
        assert_eq!(findings.len(), 3, "consolidate merged 4 raw → 3");
        // First entry has merged `lenses` (no singular `lens` field).
        let first = &findings[0];
        let lenses = first.get("lenses").unwrap().as_array().unwrap();
        assert_eq!(
            lenses
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["lifetime", "memory"]
        );
    }

    /// When the driver doesn't implement consolidate, the workflow
    /// fails loudly instead of silently dropping findings.
    #[tokio::test]
    async fn consolidate_without_driver_support_fails_loudly() {
        struct NoConsolidate;
        #[async_trait]
        impl Driver for NoConsolidate {
            async fn run(
                &self,
                _step: &Step,
                _attempt: u32,
                _ctx: &ExecContext<'_>,
                _lens: Option<&Lens>,
            ) -> Result<Map<String, Value>, String> {
                Ok(serde_json::from_value(json!({"findings": []})).unwrap())
            }
            // run_post_actions + consolidate fall back to defaults;
            // the default consolidate impl returns Err.
        }
        let wf = lensed_review_workflow();
        let mut driver = NoConsolidate;
        let trace = run(&wf, &mut driver, target_inputs()).await;
        eprintln!("{}", trace.pretty());
        match &trace.status {
            WorkflowStatus::Failure(msg) => {
                assert!(msg.contains("investigate"), "got: {msg}");
                assert!(
                    msg.contains("consolidate") || msg.contains("does not implement"),
                    "got: {msg}"
                );
            }
            other => panic!("expected Failure, got {other:?}"),
        }
    }

    /// judge_llm eval consults the driver's judge() method; when
    /// the judge returns false, on_fail's repeat re-runs the step.
    #[tokio::test]
    async fn judge_llm_eval_dispatches_to_driver() {
        struct CountingJudge {
            calls: std::sync::Arc<std::sync::Mutex<u32>>,
        }
        #[async_trait]
        impl Driver for CountingJudge {
            async fn run(
                &self,
                _step: &Step,
                _attempt: u32,
                _ctx: &ExecContext<'_>,
                _lens: Option<&Lens>,
            ) -> Result<Map<String, Value>, String> {
                Ok(serde_json::from_value(json!({"x": 1})).unwrap())
            }
            async fn judge(
                &self,
                _step: &Step,
                _ctx: &ExecContext<'_>,
            ) -> Result<(bool, String), String> {
                let mut n = self.calls.lock().unwrap();
                *n += 1;
                // Pass on the second call so the workflow terminates.
                Ok((*n >= 2, format!("call {n}")))
            }
        }

        let wf_json = serde_json::json!({
            "$schema_version": 1,
            "id": "judge-test",
            "steps": [{
                "id": "s",
                "agent": "fast",
                "prompt": "p",
                "outputs": {"x": {"type": "integer"}},
                "eval": {
                    "type": "judge_llm",
                    "judge_prompt": "Is x positive?",
                    "on_fail": {"action": "repeat", "max_attempts": 3}
                }
            }]
        });
        let wf = parse_workflow(&wf_json.to_string()).unwrap();
        let calls = std::sync::Arc::new(std::sync::Mutex::new(0));
        let mut driver = CountingJudge {
            calls: calls.clone(),
        };
        let trace = run(&wf, &mut driver, Map::new()).await;
        eprintln!("{}", trace.pretty());
        assert_eq!(trace.status, WorkflowStatus::Success);
        assert_eq!(*calls.lock().unwrap(), 2, "judge ran twice");
    }

    /// Snapshot loader rejects mismatched schema_version with a
    /// warning instead of crashing on a struct shape that has since
    /// changed.
    #[test]
    fn snapshot_load_rejects_unknown_schema_version() {
        let tmp = tempfile::tempdir().unwrap();
        let body = serde_json::json!({
            "schema_version": 99,
            "workflow_id": "x",
            "inputs": {},
            "steps": [],
            "events_count": 0,
        });
        std::fs::write(
            tmp.path().join("workflow-x.json"),
            serde_json::to_string(&body).unwrap(),
        )
        .unwrap();
        let loaded = WorkflowSnapshot::load(tmp.path(), "x");
        assert!(loaded.is_none(), "expected None for future schema_version");
    }

    #[test]
    fn snapshot_load_accepts_current_schema_version() {
        let tmp = tempfile::tempdir().unwrap();
        let snap = WorkflowSnapshot {
            schema_version: WorkflowSnapshot::SCHEMA_VERSION,
            workflow_id: "x".into(),
            inputs: Map::new(),
            steps: vec![],
            events_count: 0,
        };
        snap.save(tmp.path()).unwrap();
        let loaded = WorkflowSnapshot::load(tmp.path(), "x").expect("current version loads");
        assert_eq!(loaded.workflow_id, "x");
    }

    /// Fix #10: when a lensed step is killed mid-fan-out, the
    /// snapshot captures completed lenses. Resume picks up from
    /// the missing lenses; the already-done ones don't re-run.
    #[tokio::test]
    async fn lens_persistence_skips_completed_lenses_on_resume() {
        use crate::workflow_exec::{run_with_persistence, WorkflowSnapshot};
        let tmp = tempfile::tempdir().unwrap();
        let wf_json = serde_json::json!({
            "$schema_version": 1,
            "id": "lens-persist",
            "steps": [{
                "id": "fan",
                "agent": "fast",
                "prompt": "p",
                "lenses": [
                    {"id": "a"},
                    {"id": "b"},
                    {"id": "c"}
                ],
                "aggregate": "concat",
                "outputs": {"findings": {"type": "array<object>"}}
            }]
        });
        let wf = parse_workflow(&wf_json.to_string()).unwrap();

        // Driver A: lens 'a' OK, 'b' OK, 'c' errors. Workflow
        // fails with partial state captured.
        struct ABFail;
        #[async_trait]
        impl Driver for ABFail {
            async fn run(
                &self,
                _step: &Step,
                _attempt: u32,
                _ctx: &ExecContext<'_>,
                lens: Option<&Lens>,
            ) -> Result<Map<String, Value>, String> {
                let id = lens.unwrap().id.clone();
                if id == "c" {
                    Err("c blew up".into())
                } else {
                    Ok(serde_json::from_value(json!({
                        "findings": [{"file": format!("{id}.c:1"), "what": id}]
                    }))
                    .unwrap())
                }
            }
        }
        let mut driver = ABFail;
        let trace =
            run_with_persistence(&wf, &mut driver, Map::new(), 50, tmp.path().to_path_buf()).await;
        assert!(matches!(trace.status, WorkflowStatus::Failure(_)));

        let snap = WorkflowSnapshot::load(tmp.path(), "lens-persist").unwrap();
        let fan_state = snap.steps.iter().find(|s| s.id == "fan").unwrap();
        // 'a' and 'b' should be saved; 'c' missing.
        assert!(fan_state.lens_outputs.contains_key("a"));
        assert!(fan_state.lens_outputs.contains_key("b"));
        assert!(!fan_state.lens_outputs.contains_key("c"));

        // Driver B: 'c' OK now. Track which lenses driver.run is
        // called for — should be only 'c'.
        struct CountCalls {
            calls: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        }
        #[async_trait]
        impl Driver for CountCalls {
            async fn run(
                &self,
                _step: &Step,
                _attempt: u32,
                _ctx: &ExecContext<'_>,
                lens: Option<&Lens>,
            ) -> Result<Map<String, Value>, String> {
                let id = lens.unwrap().id.clone();
                self.calls.lock().unwrap().push(id.clone());
                Ok(serde_json::from_value(json!({
                    "findings": [{"file": format!("{id}.c:1"), "what": id}]
                }))
                .unwrap())
            }
        }
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let mut driver2 = CountCalls {
            calls: calls.clone(),
        };
        let _trace2 = run_resume(&wf, &mut driver2, snap, Some(tmp.path().to_path_buf()), 50).await;
        let called = calls.lock().unwrap().clone();
        assert_eq!(called, vec!["c".to_string()], "only 'c' should re-run");
    }

    #[tokio::test]
    async fn consolidate_retry_clears_saved_lens_outputs() {
        struct RetryConsolidate {
            lens_calls: std::sync::Arc<std::sync::Mutex<Vec<(String, u32)>>>,
            consolidate_calls: std::sync::Arc<std::sync::Mutex<u32>>,
        }

        #[async_trait]
        impl Driver for RetryConsolidate {
            async fn run(
                &self,
                _step: &Step,
                attempt: u32,
                _ctx: &ExecContext<'_>,
                lens: Option<&Lens>,
            ) -> Result<Map<String, Value>, String> {
                let id = lens.expect("lensed step").id.clone();
                self.lens_calls.lock().unwrap().push((id, attempt));
                Ok(serde_json::from_value(json!({
                    "clean": true,
                    "defects": [],
                    "analysis": "lens clean",
                    "correction_step": "write-patch"
                }))
                .unwrap())
            }

            async fn consolidate(
                &self,
                _step: &Step,
                _ctx: &ExecContext<'_>,
                _per_lens: &[(String, Map<String, Value>)],
            ) -> Result<Map<String, Value>, String> {
                let mut calls = self.consolidate_calls.lock().unwrap();
                *calls += 1;
                if *calls == 1 {
                    return Err("inconsistent lens output".into());
                }
                Ok(serde_json::from_value(json!({
                    "clean": true,
                    "defects": [],
                    "analysis": "retry clean",
                    "correction_step": "write-patch"
                }))
                .unwrap())
            }
        }

        let wf_json = serde_json::json!({
            "$schema_version": 1,
            "id": "consolidate-retry",
            "steps": [{
                "id": "review",
                "agent": "slow",
                "prompt": "lens={{lens.id}}",
                "lenses": [{"id": "a"}, {"id": "b"}],
                "aggregate": "consolidate",
                "consolidate": {"prompt": "merge"},
                "outputs": {
                    "clean": {"type": "boolean"},
                    "defects": {"type": "array<object>"},
                    "analysis": {"type": "string"},
                    "correction_step": {"type": "string"}
                },
                "eval": {
                    "type": "field_check",
                    "expr": "clean == true",
                    "on_fail": {"action": "repeat", "max_attempts": 3}
                }
            }]
        });
        let wf = parse_workflow(&wf_json.to_string()).unwrap();
        let lens_calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let consolidate_calls = std::sync::Arc::new(std::sync::Mutex::new(0));
        let mut driver = RetryConsolidate {
            lens_calls: lens_calls.clone(),
            consolidate_calls: consolidate_calls.clone(),
        };

        let trace = run(&wf, &mut driver, Map::new()).await;

        assert_eq!(trace.status, WorkflowStatus::Success);
        assert_eq!(*consolidate_calls.lock().unwrap(), 2);
        assert_eq!(
            lens_calls.lock().unwrap().clone(),
            vec![
                ("a".to_string(), 1),
                ("b".to_string(), 1),
                ("a".to_string(), 2),
                ("b".to_string(), 2),
            ],
            "consolidate retry must re-run every lens with fresh context"
        );
        assert!(trace.final_state["review"].lens_outputs.is_empty());
    }

    /// run_with_persistence writes a snapshot file; run_resume reads
    /// it and the second run picks up where the first left off.
    #[tokio::test]
    async fn persistence_save_and_resume_roundtrip() {
        use crate::workflow_exec::{run_resume, run_with_persistence, WorkflowSnapshot};

        let tmp = tempfile::tempdir().unwrap();
        let wf_json = serde_json::json!({
            "$schema_version": 1,
            "id": "persist-test",
            "steps": [
                {"id": "first",  "agent": "fast", "prompt": "p",
                 "outputs": {"v": {"type": "integer"}}},
                {"id": "second", "agent": "fast", "prompt": "p",
                 "depends_on": ["first"]}
            ]
        });
        let wf = parse_workflow(&wf_json.to_string()).unwrap();

        // Driver A: errors on `second`, so `first` settles but
        // `second` doesn't and the snapshot captures the partial
        // state.
        struct ErrOnSecond;
        #[async_trait]
        impl Driver for ErrOnSecond {
            async fn run(
                &self,
                step: &Step,
                _attempt: u32,
                _ctx: &ExecContext<'_>,
                _lens: Option<&Lens>,
            ) -> Result<Map<String, Value>, String> {
                if step.id == "second" {
                    return Err("simulated outage".into());
                }
                Ok(serde_json::from_value(json!({"v": 42})).unwrap())
            }
        }
        let mut driver = ErrOnSecond;
        let trace =
            run_with_persistence(&wf, &mut driver, Map::new(), 50, tmp.path().to_path_buf()).await;
        assert!(matches!(trace.status, WorkflowStatus::Failure(_)));

        // Snapshot exists and shows `first` Done, `second` still Pending.
        let snap = WorkflowSnapshot::load(tmp.path(), "persist-test").unwrap();
        let first = snap.steps.iter().find(|s| s.id == "first").unwrap();
        assert_eq!(first.status, StepStatus::Done);
        let second = snap.steps.iter().find(|s| s.id == "second").unwrap();
        assert_eq!(second.status, StepStatus::Pending);

        // Driver B: succeeds on `second`. Resuming picks up there
        // — `first` is already Done so its driver.run is NOT
        // called.
        struct CountFirst {
            first_calls: std::sync::Arc<std::sync::Mutex<u32>>,
        }
        #[async_trait]
        impl Driver for CountFirst {
            async fn run(
                &self,
                step: &Step,
                _attempt: u32,
                _ctx: &ExecContext<'_>,
                _lens: Option<&Lens>,
            ) -> Result<Map<String, Value>, String> {
                if step.id == "first" {
                    *self.first_calls.lock().unwrap() += 1;
                }
                Ok(serde_json::from_value(json!({"ok": true})).unwrap())
            }
        }
        let first_calls = std::sync::Arc::new(std::sync::Mutex::new(0));
        let mut driver2 = CountFirst {
            first_calls: first_calls.clone(),
        };
        let trace2 = run_resume(&wf, &mut driver2, snap, Some(tmp.path().to_path_buf()), 50).await;
        assert_eq!(trace2.status, WorkflowStatus::Success);
        assert_eq!(*first_calls.lock().unwrap(), 0, "first must NOT re-run");
    }

    /// Workflow.completion.success_when_any short-circuits the run.
    #[tokio::test]
    async fn completion_success_expression_short_circuits() {
        let wf_json = serde_json::json!({
            "$schema_version": 1,
            "id": "comp-test",
            "steps": [
                {"id": "first", "agent": "fast", "prompt": "p",
                 "outputs": {"flag": {"type": "boolean"}}},
                {"id": "second", "agent": "fast", "prompt": "p",
                 "depends_on": ["first"]}
            ],
            "completion": {
                "success_when_any": ["first.flag == true"]
            }
        });
        let wf = parse_workflow(&wf_json.to_string()).unwrap();
        let mut driver = ScriptedDriver::new().with("first", 1, json!({"flag": true}));
        let trace = run(&wf, &mut driver, Map::new()).await;
        match &trace.status {
            WorkflowStatus::TerminalSuccess(reason) => {
                assert!(reason.contains("first.flag == true"), "got: {reason}");
            }
            other => panic!("expected TerminalSuccess, got {other:?}"),
        }
        // Second step never ran.
        assert!(!trace
            .events
            .iter()
            .any(|e| matches!(e, TraceEvent::StepProduced { id, .. } if id == "second")));
    }

    #[tokio::test]
    async fn completion_failure_expression_terminates() {
        let wf_json = serde_json::json!({
            "$schema_version": 1,
            "id": "comp-fail",
            "steps": [
                {"id": "first", "agent": "fast", "prompt": "p",
                 "outputs": {"bad": {"type": "boolean"}}},
                {"id": "second", "agent": "fast", "prompt": "p",
                 "depends_on": ["first"]}
            ],
            "completion": {
                "failure_when_any": ["first.bad == true"]
            }
        });
        let wf = parse_workflow(&wf_json.to_string()).unwrap();
        let mut driver = ScriptedDriver::new().with("first", 1, json!({"bad": true}));
        let trace = run(&wf, &mut driver, Map::new()).await;
        match &trace.status {
            WorkflowStatus::Failure(reason) => {
                assert!(reason.contains("failure_when_any"), "got: {reason}");
            }
            other => panic!("expected Failure, got {other:?}"),
        }
    }

    /// `aggregate: by_lens` keys every declared output by lens id.
    #[tokio::test]
    async fn lens_fan_out_by_lens_aggregation() {
        // Build a minimal lensed workflow inline.
        let wf_json = serde_json::json!({
            "$schema_version": 1,
            "id": "by-lens-test",
            "steps": [{
                "id": "scan",
                "agent": "fast",
                "prompt": "lens={{lens.id}}",
                "lenses": [
                    {"id": "a"},
                    {"id": "b"}
                ],
                "aggregate": "by_lens",
                "outputs": {
                    "result": {"type": "string"}
                }
            }]
        });
        let wf = parse_workflow(&wf_json.to_string()).unwrap();
        let mut driver = ScriptedDriver::new()
            .with("scan|a", 1, json!({"result": "alpha"}))
            .with("scan|b", 1, json!({"result": "bravo"}));
        let trace = run(&wf, &mut driver, Map::new()).await;
        eprintln!("{}", trace.pretty());
        assert_eq!(trace.status, WorkflowStatus::Success);

        let agg = trace
            .events
            .iter()
            .find_map(|e| match e {
                TraceEvent::FanIn { aggregated, .. } => Some(aggregated.clone()),
                _ => None,
            })
            .unwrap();
        let by = agg.get("result").unwrap().as_object().unwrap();
        assert_eq!(by.get("a"), Some(&json!("alpha")));
        assert_eq!(by.get("b"), Some(&json!("bravo")));
    }

    /// Inputs with target_kind = finding_dir so all branches that
    /// gate on it (invalidate, publish) are reachable when their
    /// run_if conditions hold.
    fn finding_dir_inputs() -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("target".into(), json!("/tmp/finding"));
        m.insert("target_kind".into(), json!("finding_dir"));
        m.insert("target_artifact_dir".into(), json!("/tmp/finding"));
        m
    }

    fn prose_inputs() -> Map<String, Value> {
        let mut m = Map::new();
        m.insert(
            "target".into(),
            json!("missing cleanup after synchronous operation"),
        );
        m.insert("target_kind".into(), json!("prose"));
        m.insert("target_artifact_dir".into(), json!(""));
        m
    }

    /// Outputs every "successful" research call returns. The patch
    /// is valid; provenance is handled by fixes-tag-search.
    fn ok_research() -> Value {
        json!({
            "research_status": "confirmed",
            "valid": true,
            "invalid_evidence": "",
            "invalid_evidence_kind": "none",
            "affected_files": ["drivers/example/example.c"],
            "affected_symbols": ["example_lookup"],
            "research_decision": {
                "bug_proven": true,
                "fix_contract_proven": true,
                "invalidity_proven": false,
                "needs_more_audit": false
            },
            "analysis": "Fix example_lookup in drivers/example/example.c."
        })
    }

    fn ok_fixes_tag_search() -> Value {
        json!({
            "fixes_sha": "abc123def456",
            "fixes_subject": "subsystem: original buggy commit",
            "fixes_evidence": "The preimage lacks the bug and the postimage has it.",
            "unproven_fixes_candidates": [],
            "analysis": "Checked blame, --follow, pickaxe, and candidate diffs."
        })
    }

    fn ok_write_patch() -> Value {
        json!({
            "build_target": "drivers/example/example.o",
            "code_changes_emitted": true,
            "affected_files_changed": true,
            "review_dispute": "",
            "review_dispute_allowed": false
        })
    }

    fn ok_header_only_write_patch() -> Value {
        json!({
            "build_target": "",
            "code_changes_emitted": true,
            "affected_files_changed": true,
            "review_dispute": "",
            "review_dispute_allowed": false
        })
    }

    fn no_op_write_patch() -> Value {
        json!({
            "build_target": "drivers/example/example.o",
            "code_changes_emitted": false,
            "affected_files_changed": false,
            "review_dispute": "",
            "review_dispute_allowed": false
        })
    }

    fn disputed_review_write_patch() -> Value {
        json!({
            "build_target": "",
            "code_changes_emitted": false,
            "affected_files_changed": false,
            "review_dispute": "The review claimed the mismatch path jumps to no_split, but the current patch uses goto out and out calls folio_put(folio2).",
            "review_dispute_allowed": true
        })
    }

    fn illicit_review_dispute_write_patch() -> Value {
        json!({
            "build_target": "",
            "code_changes_emitted": false,
            "affected_files_changed": false,
            "review_dispute": "There is nothing to patch.",
            "review_dispute_allowed": false
        })
    }

    fn ok_commit_message() -> Value {
        json!({"commit_message_written": true})
    }

    fn ok_commit() -> Value {
        json!({"commit_sha": "abc123def4567890"})
    }

    fn ok_build_clean() -> Value {
        json!({
            "result": "clean",
            "build_target": "drivers/example/example.o",
            "exit_code": 0,
            "stdout": "",
            "stderr": ""
        })
    }

    fn ok_empty_build_clean() -> Value {
        json!({
            "result": "clean",
            "build_target": "",
            "exit_code": 0,
            "stdout": "no build targets derived; skipping compile",
            "stderr": ""
        })
    }

    fn build_failed() -> Value {
        json!({
            "result": "failed",
            "build_target": "drivers/example/example.o",
            "exit_code": 2,
            "stdout": "",
            "stderr": "drivers/example/example.c:42: error: nope"
        })
    }

    fn patch_error_triage() -> Value {
        json!({"result": "patch_error", "analysis": "ctree.c:42 is caused by the patch"})
    }

    fn ok_review_clean() -> Value {
        json!({
            "clean": true,
            "defects": [],
            "analysis": "review clean",
            "correction_step": "write-patch"
        })
    }

    fn with_fix_review_attempt(
        mut driver: ScriptedDriver,
        wf: &Workflow,
        attempt: u32,
        consolidated: Value,
    ) -> ScriptedDriver {
        let review = wf.steps.iter().find(|s| s.id == "review").unwrap();
        let clean_lens = ok_review_clean();
        for lens in &review.lenses {
            driver = driver.with(&format!("review|{}", lens.id), attempt, clean_lens.clone());
        }
        driver.with("review|@consolidate", attempt, consolidated)
    }

    /// Regression: shutdown propagates through the AgentEnv-fallback
    /// path. Earlier version had self.shutdown wired only into the
    /// orchestrator path; client.messages on the AgentEnv path
    /// blocked forever on a stuck server. Now every client.messages
    /// site is wrapped in `tokio::select! { _ = shutdown.cancelled()
    /// => Err, r = ... }`.
    #[tokio::test]
    async fn shutdown_cancels_agentenv_path() {
        use crate::workflow_runner::{AgentEnv, LlmDriver};
        use kres_llm::client::Client;
        use std::sync::Arc;

        // TCP listener that accepts but never replies, so
        // client.messages would block forever without a guard.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    use tokio::io::AsyncReadExt;
                    let mut buf = vec![0u8; 4096];
                    loop {
                        if sock.read(&mut buf).await.unwrap_or(0) == 0 {
                            return;
                        }
                    }
                });
            }
        });

        let wf_json = serde_json::json!({
            "$schema_version": 1,
            "id": "shutdown-test",
            "steps": [{
                "id": "stuck",
                "agent": "fast",
                "prompt": "p",
                "outputs": {"x": {"type": "string"}}
            }]
        });
        let wf = parse_workflow(&wf_json.to_string()).unwrap();

        let client = Client::builder("test-key")
            .base_url(format!("http://127.0.0.1:{port}"))
            .no_proxy()
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .unwrap();
        let env = AgentEnv::new(Arc::new(client), "claude-haiku-4-5-20251001", 4096, None);
        let shutdown = kres_core::Shutdown::new();
        let mut driver = LlmDriver::new(std::env::temp_dir(), wf.clone())
            .with_fast(env)
            .with_shutdown(shutdown.clone());

        // Cancel after 200ms — without the shutdown guard the test
        // would hang on client.messages until the 60s timeout.
        let canceller = shutdown.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            canceller.cancel();
        });

        let started = std::time::Instant::now();
        let trace = run(&wf, &mut driver, Map::new()).await;
        let elapsed = started.elapsed();

        match &trace.status {
            WorkflowStatus::Failure(msg) => {
                assert!(
                    msg.contains("cancelled before LLM call returned"),
                    "expected shutdown-driven failure, got: {msg}"
                );
            }
            other => panic!("expected Failure from cancelled, got {other:?}"),
        }
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "shutdown didn't cancel promptly: took {elapsed:?}"
        );
    }

    /// Regression: the optimised lens path must NOT skip eval or
    /// post_actions. Found during a self-review pass — earlier
    /// version called `continue` after StepProduced and bypassed
    /// the eval block entirely. A step with eval that fails MUST
    /// retry (or fail the workflow per its on_fail), not silently
    /// settle as Done.
    #[tokio::test]
    async fn optimised_lens_path_runs_eval() {
        struct OptimisedDriver {
            calls: std::sync::Arc<std::sync::Mutex<u32>>,
        }
        #[async_trait]
        impl Driver for OptimisedDriver {
            async fn run(
                &self,
                _step: &Step,
                _attempt: u32,
                _ctx: &ExecContext<'_>,
                _lens: Option<&Lens>,
            ) -> Result<Map<String, Value>, String> {
                panic!("per-lens run() must not fire on optimised path")
            }
            async fn lens_fan_out_consolidate(
                &self,
                _step: &Step,
                _ctx: &ExecContext<'_>,
            ) -> Result<Map<String, Value>, String> {
                let mut n = self.calls.lock().unwrap();
                *n += 1;
                // Pass on the second attempt so the workflow ends
                // Success after a retry — proves eval ran.
                let pass = *n >= 2;
                Ok(serde_json::from_value(json!({"clean": pass})).unwrap())
            }
        }
        let wf_json = serde_json::json!({
            "$schema_version": 1,
            "id": "opt-eval",
            "steps": [{
                "id": "lensed",
                "agent": "slow",
                "prompt": "p",
                "lenses": [{"id": "a"}, {"id": "b"}],
                "aggregate": "consolidate",
                "consolidate": {"prompt": "merge"},
                "outputs": {"clean": {"type": "boolean"}},
                "eval": {
                    "type": "field_check",
                    "expr": "clean == true",
                    "on_fail": {"action": "repeat", "max_attempts": 3}
                }
            }]
        });
        let wf = parse_workflow(&wf_json.to_string()).unwrap();
        let calls = std::sync::Arc::new(std::sync::Mutex::new(0));
        let mut driver = OptimisedDriver {
            calls: calls.clone(),
        };
        let trace = run(&wf, &mut driver, Map::new()).await;
        assert_eq!(trace.status, WorkflowStatus::Success);
        assert_eq!(*calls.lock().unwrap(), 2, "eval must trigger a retry");
        // Trace shows EvalFailed on attempt 1 and EvalPassed on
        // attempt 2.
        let eval_failed = trace
            .events
            .iter()
            .filter(|e| matches!(e, TraceEvent::EvalFailed { .. }))
            .count();
        let eval_passed = trace
            .events
            .iter()
            .filter(|e| matches!(e, TraceEvent::EvalPassed { .. }))
            .count();
        assert_eq!(eval_failed, 1);
        assert_eq!(eval_passed, 1);
    }

    /// Driver-side output mapping failures, including malformed JSON
    /// or missing required workflow outputs in the production LLM
    /// driver, should consume the step's eval retry budget when the
    /// step has an eval block. This is the control-flow guard that
    /// lets research recover from bad structured output instead of
    /// terminating before a second full attempt.
    #[tokio::test]
    async fn driver_error_retries_when_step_has_eval_budget() {
        struct FlakyMappingDriver {
            calls: std::sync::Arc<std::sync::Mutex<u32>>,
        }

        #[async_trait]
        impl Driver for FlakyMappingDriver {
            async fn run(
                &self,
                _step: &Step,
                attempt: u32,
                _ctx: &ExecContext<'_>,
                _lens: Option<&Lens>,
            ) -> Result<Map<String, Value>, String> {
                *self.calls.lock().unwrap() += 1;
                if attempt == 1 {
                    return Err("missing required output(s): research_status".into());
                }
                Ok(serde_json::from_value(json!({
                    "research_status": "confirmed",
                    "valid": true,
                    "invalid_evidence": "",
                    "invalid_evidence_kind": "none"
                }))
                .unwrap())
            }
        }

        let wf_json = serde_json::json!({
            "$schema_version": 1,
            "id": "research-retry",
            "steps": [{
                "id": "research",
                "agent": "fast",
                "prompt": "p",
                "outputs": {
                    "research_status": {"type": "string"},
                    "valid": {"type": "boolean"},
                    "invalid_evidence": {"type": "string"},
                    "invalid_evidence_kind": {"type": "string"}
                },
                "eval": {
                    "type": "field_check",
                    "expr": "research_status == 'confirmed' && valid == true && invalid_evidence == '' && invalid_evidence_kind == 'none'",
                    "on_fail": {"action": "repeat", "max_attempts": 3}
                }
            }]
        });
        let wf = parse_workflow(&wf_json.to_string()).unwrap();
        let calls = std::sync::Arc::new(std::sync::Mutex::new(0));
        let mut driver = FlakyMappingDriver {
            calls: calls.clone(),
        };
        let trace = run(&wf, &mut driver, Map::new()).await;

        assert_eq!(trace.status, WorkflowStatus::Success);
        assert_eq!(*calls.lock().unwrap(), 2);
        assert!(trace.events.iter().any(|e| matches!(
            e,
            TraceEvent::EvalFailed {
                id,
                action,
                eval_failures: 1,
                ..
            } if id == "research" && action.contains("driver error")
        )));
    }

    /// When the driver overrides `lens_fan_out_consolidate` and
    /// returns Ok, the executor takes the optimised path: emits
    /// FanOut → Consolidating → FanIn → StepProduced and skips
    /// the per-lens loop entirely (no LensProduced events).
    #[tokio::test]
    async fn optimised_lens_fan_out_skips_per_lens_loop() {
        struct OptimisedDriver;
        #[async_trait]
        impl Driver for OptimisedDriver {
            async fn run(
                &self,
                _step: &Step,
                _attempt: u32,
                _ctx: &ExecContext<'_>,
                _lens: Option<&Lens>,
            ) -> Result<Map<String, Value>, String> {
                panic!("per-lens run() must NOT be called when optimised path is taken")
            }
            async fn lens_fan_out_consolidate(
                &self,
                _step: &Step,
                _ctx: &ExecContext<'_>,
            ) -> Result<Map<String, Value>, String> {
                Ok(serde_json::from_value(json!({"findings": []})).unwrap())
            }
        }
        let wf = lensed_review_workflow();
        let mut driver = OptimisedDriver;
        let trace = run(&wf, &mut driver, target_inputs()).await;
        // Sanity: the workflow finished.
        assert!(matches!(
            trace.status,
            WorkflowStatus::Success | WorkflowStatus::TerminalSuccess(_)
        ));
        // No LensProduced events — the per-lens loop didn't fire.
        let lens_produced = trace
            .events
            .iter()
            .any(|e| matches!(e, TraceEvent::LensProduced { .. }));
        assert!(
            !lens_produced,
            "optimised path should skip the per-lens loop"
        );
        // FanIn carries strategy=Consolidate.
        let fan_in = trace.events.iter().find_map(|e| match e {
            TraceEvent::FanIn { strategy, .. } => Some(strategy.clone()),
            _ => None,
        });
        assert_eq!(fan_in.as_deref(), Some("Consolidate"));
    }

    /// When `lens_fan_out_consolidate` returns Err, the executor
    /// falls back to the per-lens loop used by ScriptedDriver tests.
    #[tokio::test]
    async fn fallback_lens_fan_out_uses_per_lens_loop() {
        // ScriptedDriver doesn't override lens_fan_out_consolidate
        // so it returns Err → executor falls back.
        let wf = lensed_review_workflow();
        let mut driver = ScriptedDriver::new()
            .with("investigate|lifetime", 1, json!({"findings": []}))
            .with("investigate|memory", 1, json!({"findings": []}))
            .with("investigate|bounds", 1, json!({"findings": []}))
            .with("investigate|races", 1, json!({"findings": []}))
            .with("investigate|general", 1, json!({"findings": []}))
            .with("investigate|@consolidate", 1, json!({"findings": []}));
        let trace = run(&wf, &mut driver, target_inputs()).await;
        assert!(matches!(
            trace.status,
            WorkflowStatus::Success | WorkflowStatus::TerminalSuccess(_)
        ));
        // Per-lens loop fired — LensProduced events present.
        let lens_count = trace
            .events
            .iter()
            .filter(|e| matches!(e, TraceEvent::LensProduced { .. }))
            .count();
        assert_eq!(lens_count, 5);
    }

    #[tokio::test]
    async fn observer_streams_events_live() {
        let wf = fix_workflow();
        let mut driver = ScriptedDriver::new()
            .with("research", 1, ok_research())
            .with("write-patch", 1, ok_write_patch())
            .with("fixes-tag-search", 1, ok_fixes_tag_search())
            .with("write-commit-message", 1, ok_commit_message())
            .with("commit", 1, ok_commit())
            .with("build", 1, ok_build_clean())
            .with("publish", 1, json!({"patch_path": "/tmp/p.diff"}));
        driver = with_fix_review_attempt(driver, &wf, 1, ok_review_clean());
        let received = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let r2 = received.clone();
        let observer: EventObserver = Box::new(move |ev| {
            // Tag each event with its kind for the assertion.
            let tag = match ev {
                TraceEvent::StepStarted { id, .. } => format!("start:{id}"),
                TraceEvent::StepProduced { id, .. } => format!("produced:{id}"),
                TraceEvent::EvalPassed { id, .. } => format!("evalok:{id}"),
                TraceEvent::Terminated { .. } => "terminated".into(),
                _ => "other".into(),
            };
            r2.lock().unwrap().push(tag);
        });
        let _trace = run_with_observer(&wf, &mut driver, finding_dir_inputs(), 200, observer).await;
        let r = received.lock().unwrap();
        // The first observed event is start:research (NOT terminated).
        assert_eq!(
            r[0], "start:research",
            "events should arrive live, not all at the end"
        );
        assert!(r.contains(&"terminated".into()));
    }

    #[tokio::test]
    async fn happy_path_passes_in_one_go() {
        let wf = fix_workflow();
        let mut driver = ScriptedDriver::new()
            .with("research", 1, ok_research())
            .with("write-patch", 1, ok_write_patch())
            .with("fixes-tag-search", 1, ok_fixes_tag_search())
            .with("write-commit-message", 1, ok_commit_message())
            .with("commit", 1, ok_commit())
            .with("build", 1, ok_build_clean())
            .with("publish", 1, json!({"patch_path": "/tmp/finding/fix.diff"}));
        driver = with_fix_review_attempt(driver, &wf, 1, ok_review_clean());
        let trace = run(&wf, &mut driver, finding_dir_inputs()).await;
        eprintln!("{}", trace.pretty());
        assert_eq!(trace.status, WorkflowStatus::Success);
        // Status update should be skipped for confirmed research.
        assert!(trace.events.iter().any(|e| matches!(
            e,
            TraceEvent::StepSkipped { id, .. } if id == "invalidate"
        )));
        // Publish ran exactly once.
        let publish_runs = trace
            .events
            .iter()
            .filter(|e| matches!(e, TraceEvent::StepProduced { id, .. } if id == "publish"))
            .count();
        assert_eq!(publish_runs, 1);
        let write_patch_outputs = trace
            .final_state
            .get("write-patch")
            .expect("write-patch state")
            .outputs
            .clone();
        assert_eq!(
            write_patch_outputs.get("build_target"),
            Some(&json!("drivers/example/example.o"))
        );
        assert!(
            !write_patch_outputs.contains_key("commit_sha"),
            "write-patch must not require or carry a model-guessed commit_sha"
        );
        // No EvalFailed events.
        assert!(!trace
            .events
            .iter()
            .any(|e| matches!(e, TraceEvent::EvalFailed { .. })));
    }

    #[tokio::test]
    async fn write_patch_allows_non_object_changes_without_build_target() {
        let wf = fix_workflow();
        let mut driver = ScriptedDriver::new()
            .with("research", 1, ok_research())
            .with("write-patch", 1, ok_header_only_write_patch())
            .with("fixes-tag-search", 1, ok_fixes_tag_search())
            .with("write-commit-message", 1, ok_commit_message())
            .with("commit", 1, ok_commit())
            .with("build", 1, ok_empty_build_clean());
        driver = with_fix_review_attempt(driver, &wf, 1, ok_review_clean());

        let trace = run(&wf, &mut driver, prose_inputs()).await;
        eprintln!("{}", trace.pretty());
        assert!(matches!(
            trace.status,
            WorkflowStatus::Success | WorkflowStatus::TerminalSuccess(_)
        ));
        assert!(!trace.events.iter().any(|e| matches!(
            e,
            TraceEvent::EvalFailed { id, .. } if id == "write-patch"
        )));
        assert_eq!(
            trace.final_state["write-patch"].outputs.get("build_target"),
            Some(&json!(""))
        );
        assert!(trace.events.iter().any(|e| matches!(
            e,
            TraceEvent::StepProduced { id, .. } if id == "build"
        )));
    }

    #[tokio::test]
    async fn write_patch_noop_does_not_pass_eval() {
        let wf = fix_workflow();
        let mut driver = ScriptedDriver::new()
            .with("research", 1, ok_research())
            .with("write-patch", 1, no_op_write_patch())
            .with("write-patch", 2, no_op_write_patch())
            .with("write-patch", 3, no_op_write_patch())
            .with("write-patch", 4, no_op_write_patch())
            .with("write-patch", 5, no_op_write_patch())
            .with("write-patch", 6, no_op_write_patch());
        let trace = run(&wf, &mut driver, finding_dir_inputs()).await;
        eprintln!("{}", trace.pretty());
        assert!(matches!(trace.status, WorkflowStatus::Failure(_)));
        let write_patch_eval_fails = trace
            .events
            .iter()
            .filter(|e| matches!(e, TraceEvent::EvalFailed { id, .. } if id == "write-patch"))
            .count();
        assert_eq!(write_patch_eval_fails, 6);
        assert!(!trace
            .events
            .iter()
            .any(|e| { matches!(e, TraceEvent::PostAction { id, .. } if id == "write-patch") }));
    }

    #[tokio::test]
    async fn write_patch_dispute_requires_prior_review_source_defect() {
        let wf = fix_workflow();
        let mut driver = ScriptedDriver::new()
            .with("research", 1, ok_research())
            .with("write-patch", 1, illicit_review_dispute_write_patch())
            .with("write-patch", 2, illicit_review_dispute_write_patch())
            .with("write-patch", 3, illicit_review_dispute_write_patch())
            .with("write-patch", 4, illicit_review_dispute_write_patch())
            .with("write-patch", 5, illicit_review_dispute_write_patch())
            .with("write-patch", 6, illicit_review_dispute_write_patch());
        let trace = run(&wf, &mut driver, finding_dir_inputs()).await;
        eprintln!("{}", trace.pretty());
        assert!(matches!(trace.status, WorkflowStatus::Failure(_)));
        let write_patch_eval_fails = trace
            .events
            .iter()
            .filter(|e| matches!(e, TraceEvent::EvalFailed { id, .. } if id == "write-patch"))
            .count();
        assert_eq!(write_patch_eval_fails, 6);
        assert!(!trace
            .events
            .iter()
            .any(|e| matches!(e, TraceEvent::StepProduced { id, .. } if id == "fixes-tag-search")));
    }

    /// A deterministic build failure is classified by compile-triage.
    /// Patch-caused failures branch back to write-patch; the next
    /// patch attempt can then build clean and proceed to review.
    #[tokio::test]
    async fn compile_triage_patch_error_branches_to_write_patch() {
        let wf = fix_workflow();
        let mut driver = ScriptedDriver::new()
            .with("research", 1, ok_research())
            .with("write-patch", 1, ok_write_patch())
            .with("write-patch", 2, ok_write_patch())
            .with("fixes-tag-search", 1, ok_fixes_tag_search())
            .with("fixes-tag-search", 2, ok_fixes_tag_search())
            .with("write-commit-message", 1, ok_commit_message())
            .with("write-commit-message", 2, ok_commit_message())
            .with("commit", 1, ok_commit())
            .with("commit", 2, ok_commit())
            .with("build", 1, build_failed())
            .with("build", 2, ok_build_clean())
            .with("compile-triage", 1, patch_error_triage())
            .with("publish", 1, json!({"patch_path": "/tmp/p.diff"}));
        driver = with_fix_review_attempt(driver, &wf, 1, ok_review_clean());
        let trace = run(&wf, &mut driver, finding_dir_inputs()).await;
        eprintln!("{}", trace.pretty());
        assert_eq!(trace.status, WorkflowStatus::Success);

        assert!(trace.events.iter().any(|e| matches!(
            e,
            TraceEvent::BranchedTo { from, to } if from == "compile-triage" && to == "write-patch"
        )));
        let write_patch_attempts: Vec<u32> = trace
            .events
            .iter()
            .filter_map(|e| match e {
                TraceEvent::StepStarted { id, attempt } if id == "write-patch" => Some(*attempt),
                _ => None,
            })
            .collect();
        assert_eq!(write_patch_attempts, vec![1, 2]);
    }

    /// Source-code review defects branch back to patch writing. The
    /// review step reports defects only; it does not edit or amend.
    #[tokio::test]
    async fn review_defect_branches_to_write_patch() {
        let wf = fix_workflow();
        let bad_review = json!({"clean": false, "defects": [{"lens": "bounds",
                                  "where": "ctree.c:42", "what": "off-by-one"}],
                                  "analysis": "off-by-one in patched bounds",
                                  "correction_step": "write-patch"});
        let mut driver = ScriptedDriver::new()
            .with("research", 1, ok_research())
            .with("write-patch", 1, ok_write_patch())
            .with("write-patch", 2, ok_write_patch())
            .with("fixes-tag-search", 1, ok_fixes_tag_search())
            .with("fixes-tag-search", 2, ok_fixes_tag_search())
            .with("write-commit-message", 1, ok_commit_message())
            .with("write-commit-message", 2, ok_commit_message())
            .with("commit", 1, ok_commit())
            .with("commit", 2, ok_commit())
            .with("build", 1, ok_build_clean())
            .with("build", 2, ok_build_clean())
            .with("publish", 1, json!({"patch_path": "/tmp/p.diff"}));
        driver = with_fix_review_attempt(driver, &wf, 1, bad_review.clone());
        driver = with_fix_review_attempt(driver, &wf, 2, ok_review_clean());
        let trace = run(&wf, &mut driver, finding_dir_inputs()).await;
        eprintln!("{}", trace.pretty());
        assert_eq!(trace.status, WorkflowStatus::Success);

        let review_attempts: Vec<u32> = trace
            .events
            .iter()
            .filter_map(|e| match e {
                TraceEvent::StepStarted { id, attempt } if id == "review" => Some(*attempt),
                _ => None,
            })
            .collect();
        assert_eq!(review_attempts, vec![1, 2]);
        assert!(trace.events.iter().any(|e| matches!(
            e,
            TraceEvent::BranchedTo { from, to } if from == "review" && to == "write-patch"
        )));
    }

    /// Patch writing can dispute an incorrect source review without
    /// manufacturing a source edit. The committed patch is unchanged,
    /// so provenance/commit/build are skipped on the dispute pass and
    /// the existing review step adjudicates the dispute with preserved
    /// provenance from the original patch pass.
    #[tokio::test]
    async fn write_patch_dispute_routes_back_to_review_without_recommit() {
        let wf = fix_workflow();
        let bad_review = json!({
            "clean": false,
            "defects": [{
                "lens": "lifetime",
                "where": "mm/truncate.c",
                "what": "mismatch path leaks folio2 if it jumps to no_split"
            }],
            "source_defects": [{
                "lens": "lifetime",
                "where": "mm/truncate.c",
                "what": "mismatch path leaks folio2 if it jumps to no_split"
            }],
            "commit_message_defects": [],
            "analysis": "review thinks the mismatch path bypasses folio_put",
            "correction_step": "write-patch"
        });
        let mut driver = ScriptedDriver::new()
            .with("research", 1, ok_research())
            .with("write-patch", 1, ok_write_patch())
            .with("write-patch", 2, disputed_review_write_patch())
            .with("fixes-tag-search", 1, ok_fixes_tag_search())
            .with("write-commit-message", 1, ok_commit_message())
            .with("commit", 1, ok_commit())
            .with("build", 1, ok_build_clean())
            .with("publish", 1, json!({"patch_path": "/tmp/p.diff"}));
        driver = with_fix_review_attempt(driver, &wf, 1, bad_review);
        driver = with_fix_review_attempt(driver, &wf, 2, ok_review_clean());
        let trace = run(&wf, &mut driver, finding_dir_inputs()).await;
        eprintln!("{}", trace.pretty());
        assert_eq!(trace.status, WorkflowStatus::Success);

        let attempts = |step_id: &str| -> Vec<u32> {
            trace
                .events
                .iter()
                .filter_map(|e| match e {
                    TraceEvent::StepStarted { id, attempt } if id == step_id => Some(*attempt),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(attempts("write-patch"), vec![1, 2]);
        assert_eq!(attempts("review"), vec![1, 2]);
        assert_eq!(attempts("fixes-tag-search"), vec![1]);
        assert_eq!(attempts("write-commit-message"), vec![1]);
        assert_eq!(attempts("commit"), vec![1]);
        assert_eq!(attempts("build"), vec![1]);
        assert_eq!(
            trace.final_state["fixes-tag-search"]
                .outputs
                .get("fixes_sha"),
            Some(&json!("abc123def456")),
            "no-op review disputes must preserve prior provenance outputs"
        );
    }

    /// Commit-message-only review defects must not loop through
    /// write-patch. They branch directly to write-commit-message so
    /// the deterministic commit step can amend the existing patch.
    #[tokio::test]
    async fn review_commit_message_defect_branches_to_commit_message() {
        let wf = fix_workflow();
        let bad_review = json!({
            "clean": false,
            "defects": [{
                "lens": "assertions",
                "where": "commit message",
                "what": "message overstates which entry points bypass check_ops_safe"
            }],
            "analysis": "code is correct; commit message needs correction",
            "correction_step": "write-commit-message"
        });
        let mut driver = ScriptedDriver::new()
            .with("research", 1, ok_research())
            .with("write-patch", 1, ok_write_patch())
            .with("fixes-tag-search", 1, ok_fixes_tag_search())
            .with("write-commit-message", 1, ok_commit_message())
            .with("write-commit-message", 2, ok_commit_message())
            .with("commit", 1, ok_commit())
            .with("commit", 2, ok_commit())
            .with("build", 1, ok_build_clean())
            .with("build", 2, ok_build_clean())
            .with("publish", 1, json!({"patch_path": "/tmp/p.diff"}));
        driver = with_fix_review_attempt(driver, &wf, 1, bad_review);
        driver = with_fix_review_attempt(driver, &wf, 2, ok_review_clean());
        let trace = run(&wf, &mut driver, finding_dir_inputs()).await;
        eprintln!("{}", trace.pretty());
        assert_eq!(trace.status, WorkflowStatus::Success);

        assert!(trace.events.iter().any(|e| matches!(
            e,
            TraceEvent::BranchedTo { from, to }
                if from == "review" && to == "write-commit-message"
        )));

        let write_patch_attempts: Vec<u32> = trace
            .events
            .iter()
            .filter_map(|e| match e {
                TraceEvent::StepStarted { id, attempt } if id == "write-patch" => Some(*attempt),
                _ => None,
            })
            .collect();
        assert_eq!(write_patch_attempts, vec![1]);

        let write_commit_message_attempts: Vec<u32> = trace
            .events
            .iter()
            .filter_map(|e| match e {
                TraceEvent::StepStarted { id, attempt } if id == "write-commit-message" => {
                    Some(*attempt)
                }
                _ => None,
            })
            .collect();
        assert_eq!(write_commit_message_attempts, vec![1, 2]);
    }

    /// Compile triage stops after repeated patch-caused build
    /// failures instead of looping forever.
    #[tokio::test]
    async fn compile_triage_exhausts_after_repeated_patch_errors() {
        let wf = fix_workflow();
        let mut driver = ScriptedDriver::new()
            .with("research", 1, ok_research())
            .with("write-patch", 1, ok_write_patch())
            .with("write-patch", 2, ok_write_patch())
            .with("write-patch", 3, ok_write_patch())
            .with("write-patch", 4, ok_write_patch())
            .with("write-patch", 5, ok_write_patch())
            .with("write-patch", 6, ok_write_patch())
            .with("write-patch", 7, ok_write_patch())
            .with("write-patch", 8, ok_write_patch())
            .with("write-patch", 9, ok_write_patch())
            .with("write-patch", 10, ok_write_patch())
            .with("fixes-tag-search", 1, ok_fixes_tag_search())
            .with("fixes-tag-search", 2, ok_fixes_tag_search())
            .with("fixes-tag-search", 3, ok_fixes_tag_search())
            .with("fixes-tag-search", 4, ok_fixes_tag_search())
            .with("fixes-tag-search", 5, ok_fixes_tag_search())
            .with("fixes-tag-search", 6, ok_fixes_tag_search())
            .with("fixes-tag-search", 7, ok_fixes_tag_search())
            .with("fixes-tag-search", 8, ok_fixes_tag_search())
            .with("fixes-tag-search", 9, ok_fixes_tag_search())
            .with("fixes-tag-search", 10, ok_fixes_tag_search())
            .with("write-commit-message", 1, ok_commit_message())
            .with("write-commit-message", 2, ok_commit_message())
            .with("write-commit-message", 3, ok_commit_message())
            .with("write-commit-message", 4, ok_commit_message())
            .with("write-commit-message", 5, ok_commit_message())
            .with("write-commit-message", 6, ok_commit_message())
            .with("write-commit-message", 7, ok_commit_message())
            .with("write-commit-message", 8, ok_commit_message())
            .with("write-commit-message", 9, ok_commit_message())
            .with("write-commit-message", 10, ok_commit_message())
            .with("commit", 1, ok_commit())
            .with("commit", 2, ok_commit())
            .with("commit", 3, ok_commit())
            .with("commit", 4, ok_commit())
            .with("commit", 5, ok_commit())
            .with("commit", 6, ok_commit())
            .with("commit", 7, ok_commit())
            .with("commit", 8, ok_commit())
            .with("commit", 9, ok_commit())
            .with("commit", 10, ok_commit())
            .with("build", 1, build_failed())
            .with("build", 2, build_failed())
            .with("build", 3, build_failed())
            .with("build", 4, build_failed())
            .with("build", 5, build_failed())
            .with("build", 6, build_failed())
            .with("build", 7, build_failed())
            .with("build", 8, build_failed())
            .with("build", 9, build_failed())
            .with("build", 10, build_failed())
            .with("compile-triage", 1, patch_error_triage())
            .with("compile-triage", 2, patch_error_triage())
            .with("compile-triage", 3, patch_error_triage())
            .with("compile-triage", 4, patch_error_triage())
            .with("compile-triage", 5, patch_error_triage())
            .with("compile-triage", 6, patch_error_triage())
            .with("compile-triage", 7, patch_error_triage())
            .with("compile-triage", 8, patch_error_triage())
            .with("compile-triage", 9, patch_error_triage())
            .with("compile-triage", 10, patch_error_triage());
        let trace = run(&wf, &mut driver, finding_dir_inputs()).await;
        eprintln!("{}", trace.pretty());
        assert!(matches!(trace.status, WorkflowStatus::Failure(_)));

        let triage_attempts: Vec<u32> = trace
            .events
            .iter()
            .filter_map(|e| match e {
                TraceEvent::StepStarted { id, attempt } if id == "compile-triage" => Some(*attempt),
                _ => None,
            })
            .collect();
        assert_eq!(triage_attempts, vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);

        assert!(trace
            .events
            .iter()
            .any(|e| matches!(e, TraceEvent::Exhausted { id, .. } if id == "compile-triage")));
    }

    /// Research declares the bug invalid. The invalidate step has
    /// terminal_on_success=true, so the workflow ends there;
    /// write-patch / commit / build / review / publish must NOT run.
    #[tokio::test]
    async fn research_invalid_terminal_short_circuit() {
        let wf = fix_workflow();
        let invalid_research = json!({
            "research_status": "invalid",
            "valid": false,
            "invalid_evidence": "fs/foo.c:120 already null-checks p",
            "invalid_evidence_kind": "source_or_commit_evidence",
            "affected_files": [],
            "affected_symbols": [],
            "research_decision": {
                "bug_proven": false,
                "fix_contract_proven": false,
                "invalidity_proven": true,
                "needs_more_audit": false
            }
        });
        let mut driver = ScriptedDriver::new()
            .with("research", 1, invalid_research)
            .with(
                "invalidate",
                1,
                json!({"files_updated": ["metadata.yaml", "FINDING.md"]}),
            );
        let trace = run(&wf, &mut driver, finding_dir_inputs()).await;
        eprintln!("{}", trace.pretty());
        assert_eq!(
            trace.status,
            WorkflowStatus::TerminalSuccess("invalidate".to_string())
        );

        // Steps that must NOT have run.
        for forbidden in [
            "write-patch",
            "fixes-tag-search",
            "write-commit-message",
            "commit",
            "build",
            "compile-triage",
            "review",
            "publish",
        ] {
            assert!(
                !trace
                    .events
                    .iter()
                    .any(|e| matches!(e, TraceEvent::StepProduced { id, .. } if id == forbidden)),
                "step '{forbidden}' should not have run after [INVALID]"
            );
        }
    }

    #[tokio::test]
    async fn research_invalid_without_structured_evidence_fails_before_invalidate() {
        let wf = fix_workflow();
        let invalid_research = json!({
            "research_status": "invalid",
            "valid": false,
            "invalid_evidence": "",
            "invalid_evidence_kind": "none",
            "affected_files": [],
            "affected_symbols": [],
            "research_decision": {
                "bug_proven": false,
                "fix_contract_proven": false,
                "invalidity_proven": false,
                "needs_more_audit": true
            },
            "analysis": "Could not prove the finding from gathered context."
        });
        let mut driver = ScriptedDriver::new()
            .with("research", 1, invalid_research)
            .with(
                "invalidate",
                1,
                json!({"files_updated": ["metadata.yaml", "FINDING.md"]}),
            );
        let trace = run(&wf, &mut driver, finding_dir_inputs()).await;
        eprintln!("{}", trace.pretty());
        assert!(matches!(trace.status, WorkflowStatus::Failure(_)));
        assert!(!trace
            .events
            .iter()
            .any(|e| matches!(e, TraceEvent::StepProduced { id, .. } if id == "invalidate")));
        assert!(!trace
            .events
            .iter()
            .any(|e| matches!(e, TraceEvent::StepProduced { id, .. } if id == "write-patch")));
    }

    #[tokio::test]
    async fn research_unconfirmed_with_proven_decision_fails_eval() {
        let wf = fix_workflow();
        let contradictory_research = json!({
            "research_status": "unconfirmed",
            "valid": false,
            "invalid_evidence": "",
            "invalid_evidence_kind": "none",
            "affected_files": ["mm/memory.c"],
            "affected_symbols": ["do_swap_page"],
            "research_decision": {
                "bug_proven": true,
                "fix_contract_proven": true,
                "invalidity_proven": false,
                "needs_more_audit": false
            },
            "analysis": "Confirmed at workspace HEAD but marking unconfirmed."
        });
        let mut driver = ScriptedDriver::new()
            .with("research", 1, contradictory_research.clone())
            .with("research", 2, contradictory_research.clone())
            .with("research", 3, contradictory_research);
        let trace = run(&wf, &mut driver, finding_dir_inputs()).await;
        eprintln!("{}", trace.pretty());
        assert!(matches!(trace.status, WorkflowStatus::Failure(_)));
        assert!(trace
            .events
            .iter()
            .any(|e| matches!(e, TraceEvent::Exhausted { id, .. } if id == "research")));
        assert!(!trace
            .events
            .iter()
            .any(|e| matches!(e, TraceEvent::StepProduced { id, .. } if id == "unconfirm")));
        assert!(!trace
            .events
            .iter()
            .any(|e| matches!(e, TraceEvent::StepProduced { id, .. } if id == "write-patch")));
    }

    #[tokio::test]
    async fn research_unconfirmed_marks_finding_without_patch() {
        let wf = fix_workflow();
        let unconfirmed_research = json!({
            "research_status": "unconfirmed",
            "valid": false,
            "invalid_evidence": "",
            "invalid_evidence_kind": "none",
            "affected_files": ["mm/memory.c"],
            "affected_symbols": ["do_swap_page"],
            "research_decision": {
                "bug_proven": false,
                "fix_contract_proven": false,
                "invalidity_proven": false,
                "needs_more_audit": true
            },
            "analysis": "Could not prove or disprove the finding from gathered context."
        });
        let mut driver = ScriptedDriver::new()
            .with("research", 1, unconfirmed_research)
            .with(
                "unconfirm",
                1,
                json!({"files_updated": ["metadata.yaml", "FINDING.md"], "status": "unconfirmed"}),
            );
        let trace = run(&wf, &mut driver, finding_dir_inputs()).await;
        eprintln!("{}", trace.pretty());
        assert_eq!(
            trace.status,
            WorkflowStatus::TerminalSuccess("unconfirm".to_string())
        );
        assert!(trace
            .events
            .iter()
            .any(|e| matches!(e, TraceEvent::StepProduced { id, .. } if id == "unconfirm")));
        assert!(!trace
            .events
            .iter()
            .any(|e| matches!(e, TraceEvent::StepProduced { id, .. } if id == "write-patch")));
    }

    #[tokio::test]
    async fn research_invalid_prose_short_circuits_before_commit() {
        let wf = fix_workflow();
        let invalid_research = json!({
            "research_status": "invalid",
            "valid": false,
            "invalid_evidence": "drivers/example/foo.c already releases the reference on completion",
            "invalid_evidence_kind": "source_or_commit_evidence",
            "affected_files": [],
            "affected_symbols": [],
            "research_decision": {
                "bug_proven": false,
                "fix_contract_proven": false,
                "invalidity_proven": true,
                "needs_more_audit": false
            },
            "analysis": "Not a bug at workspace HEAD."
        });
        let mut driver = ScriptedDriver::new()
            .with("research", 1, invalid_research)
            .default_output(json!({"commit_message_written": true}));
        let trace = run(&wf, &mut driver, prose_inputs()).await;
        eprintln!("{}", trace.pretty());
        assert!(matches!(trace.status, WorkflowStatus::TerminalSuccess(_)));

        for forbidden in [
            "write-patch",
            "fixes-tag-search",
            "write-commit-message",
            "commit",
            "build",
            "compile-triage",
            "review",
            "publish",
        ] {
            assert!(
                !trace
                    .events
                    .iter()
                    .any(|e| matches!(e, TraceEvent::StepProduced { id, .. } if id == forbidden)),
                "step '{forbidden}' should not have run after invalid prose research"
            );
        }
    }

    // ----- expression evaluator ---------------------------------------

    fn ctx_with(steps: &[(&str, u32, u32, Value)]) -> HashMap<String, StepState> {
        steps
            .iter()
            .map(|(id, attempt, fails, outs)| {
                let outs = match outs {
                    Value::Object(m) => m.clone(),
                    _ => panic!("outputs must be an object"),
                };
                (
                    id.to_string(),
                    StepState {
                        id: id.to_string(),
                        status: StepStatus::Done,
                        attempt: *attempt,
                        eval_failures: *fails,
                        outputs: outs,
                        preserved_outputs_on_skip: Map::new(),
                        lens_outputs: Map::new(),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn expr_bare_ident_resolves_against_current_step() {
        let inputs = Map::new();
        let states = ctx_with(&[("review", 3, 0, json!({"clean": true}))]);
        let ctx = ExecContext {
            workflow_inputs: &inputs,
            steps: &states,
        };
        assert!(expr::eval("clean == true", &ctx, Some("review")).unwrap());
        assert!(!expr::eval("clean == false", &ctx, Some("review")).unwrap());
    }

    #[test]
    fn expr_dotted_path_resolves_named_step() {
        let inputs = Map::new();
        let states = ctx_with(&[("research", 1, 0, json!({"valid": true}))]);
        let ctx = ExecContext {
            workflow_inputs: &inputs,
            steps: &states,
        };
        assert!(expr::eval("research.valid == true", &ctx, None).unwrap());
    }

    #[test]
    fn expr_workflow_input_lookup() {
        let mut inputs = Map::new();
        inputs.insert("target_kind".into(), json!("finding_dir"));
        let states: HashMap<String, StepState> = HashMap::new();
        let ctx = ExecContext {
            workflow_inputs: &inputs,
            steps: &states,
        };
        assert!(expr::eval("workflow.target_kind == 'finding_dir'", &ctx, None).unwrap());
    }

    #[test]
    fn expr_attempt_counter() {
        let inputs = Map::new();
        let states = ctx_with(&[("write-patch", 4, 0, json!({}))]);
        let ctx = ExecContext {
            workflow_inputs: &inputs,
            steps: &states,
        };
        assert!(!expr::eval("write-patch.attempt <= 3", &ctx, None).unwrap());
        assert!(expr::eval("write-patch.attempt <= 5", &ctx, None).unwrap());
    }

    #[test]
    fn expr_and_or_precedence() {
        let inputs = Map::new();
        let states = ctx_with(&[("s", 1, 0, json!({"a": true, "b": false}))]);
        let ctx = ExecContext {
            workflow_inputs: &inputs,
            steps: &states,
        };
        // a == true || a == false && b == true → a==true || (false &&
        // anything) → true.
        assert!(expr::eval("a == true || a == false && b == true", &ctx, Some("s")).unwrap());
        // (a == true || a == false) && b == true → true && false →
        // false.
        assert!(!expr::eval("(a == true || a == false) && b == true", &ctx, Some("s")).unwrap());
    }
}
