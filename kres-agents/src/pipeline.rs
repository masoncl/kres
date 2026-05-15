//! Single-task AgentRunner.
//!
//! Wires fast agent → main agent (data fetch) → slow agent in order.
//! Shutdown-aware: every
//! await inside the loop is inside `tokio::select!` with the task's
//! Shutdown, so /stop / /clear / --turns reaches the loop immediately.
//!
//! Lensed review paths gather source once, fan out slow-agent lenses
//! over the same context, then consolidate or return structured
//! per-lens results. Main-agent data fetch is backed by a trait, so
//! kres-repl can inject the real semcode/grep/read backend without
//! kres-agents depending on kres-mcp.

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::future::join_all;
use serde_json::{json, Value};

use kres_core::cost::UsageTracker;
use kres_core::findings::Finding;
use kres_core::lens::LensSpec;
use kres_core::log::{LoggedUsage, TurnLogger};
use kres_core::shrink::{shrink_findings_to_budget, shrink_json_list_to_budget};
use kres_core::shutdown::Shutdown;
use kres_llm::{
    client::Client, config::CallConfig, model::ThinkingBudget, request::Message, Model,
};

use crate::{
    consolidate::{consolidate_lenses_with_logger, LensOutput},
    error::AgentError,
    followup::Followup,
    prompt::CodePrompt,
    response::{parse_code_response, CodeResponse, ParseStrategy},
};

const GENERIC_LENS_REPAIR_RETRIES: usize = 1;

/// CodePrompt fields that go into the cached-prefix block.
///
/// Scope the prefix to fields that are BYTE-IDENTICAL ACROSS TASKS
/// within a session, not just across rounds within a task. The
/// Anthropic prompt cache is keyed on the exact prefix bytes, so
/// anything task-specific in here (the `question`, the
/// `previous_findings` list that grows as the session progresses,
/// the per-task `parallel_lenses`) forces every task to write a
/// fresh prefix cache that nothing else will ever read. Session
/// A large historical review run burned 14.5M tokens of
/// cache_creation on code.jsonl for only 2.25M tokens of
/// cache_read because `question` + `previous_findings` sat in the
/// prefix and mutated per task.
///
/// `skills` is the only fat field that actually stays byte-stable
/// across tasks (typically 20-80k chars of skill bodies). Fast-agent
/// gather rounds still use this prefix cache because they can make
/// multiple round trips before slow handoff.
///
/// Everything other than `skills` goes in the volatile tail —
/// including `plan_rewrite_allowed`, which is `Option<bool>` with
/// `skip_serializing_if=None`. Having it in the prefix meant the
/// prefix JSON key set varied (slow-first-call had it, fast calls
/// didn't). `to_cached_split_json` round-trips through
/// `serde_json::Value` whose Map sorts keys alphabetically, so
/// `plan_rewrite_allowed` sorted before `skills` — two prefix
/// shapes on the wire that shared only 5 bytes. Session
/// `c5843f10-…` confirmed: i=2 (fast) and i=4 (slow) had a common
/// prefix of 5 chars, cache_read=0.
///
/// For fast-agent gather rounds the tail still cache-hits on round
/// 2+ via the `Message::cache` flag. Non-lensed slow-agent calls do
/// not use this cache at all; review lenses use the separate
/// `LENS_SHARED_CACHE_FIELDS` prefix below.
const CACHED_PREFIX_FIELDS: &[&str] = &["skills"];
const LENS_SHARED_CACHE_FIELDS: &[&str] = &[
    "question",
    "symbols",
    "context",
    "previous_findings",
    "skills",
    "plan",
];

/// Abstraction over the main-agent's data-fetch capability.
/// Implementations route followups to MCP tools, grep, read, git.
#[async_trait]
pub trait DataFetcher: Send + Sync {
    /// Fetch the requested data. Returns (symbols, context) as opaque
    /// JSON chunks to feed to the fast agent's next round.
    ///
    /// `plan` is the operator's current plan (or None when no plan
    /// is in play). Callers pass it per-call so a concurrent task
    /// with a different plan does not clobber the value via a
    /// shared-slot write in between. Implementations forward the
    /// plan into the main-agent user JSON; NullFetcher ignores it.
    async fn fetch(
        &self,
        followups: &[Followup],
        plan: Option<&kres_core::Plan>,
    ) -> Result<FetchResult, AgentError>;
}

#[derive(Debug, Default, Clone)]
pub struct FetchResult {
    pub symbols: Vec<Value>,
    pub context: Vec<Value>,
}

#[derive(Debug, Clone)]
pub struct LensRunOutput {
    pub lens_id: String,
    pub lens: Value,
    pub slow_model: Option<String>,
    pub raw_response: String,
    pub parsed: CodeResponse,
}

#[derive(Debug, Clone)]
pub struct LensFanoutOutput {
    pub outputs: Vec<LensRunOutput>,
    pub failures: Vec<LensRunFailure>,
    pub fast_rounds: u8,
    pub attempted: usize,
    pub slow_variant_count: usize,
}

#[derive(Debug, Clone)]
pub struct LensRunFailure {
    pub lens_id: String,
    pub slow_model: Option<String>,
    pub error: String,
}

impl LensRunFailure {
    pub fn summary(&self) -> String {
        let model = self.slow_model.as_deref().unwrap_or("unknown-model");
        format!("{} ({model}): {}", self.lens_id, self.error)
    }
}

impl LensFanoutOutput {
    pub fn failure_summary(&self) -> String {
        self.failures
            .iter()
            .map(LensRunFailure::summary)
            .collect::<Vec<_>>()
            .join("; ")
    }
}

pub struct LensRepairPolicy<'a> {
    pub max_retries: usize,
    pub repair_instruction: &'a str,
}

struct PreparedLensFanout {
    prompt: String,
    shared_prefix: String,
    slow_variants: Vec<SlowAgentVariant>,
    trimmed_symbols: Vec<Value>,
    trimmed_context: Vec<Value>,
    trimmed_prev: Vec<Finding>,
    live_skills: Option<Value>,
    fast_rounds: u8,
}

type RawLensSuccess = (String, Value, String, String, CodeResponse);
type RawLensFailure = (String, String, String);
type RawLensResult = Result<RawLensSuccess, RawLensFailure>;

#[derive(Clone)]
struct LensCallSpec {
    lens_id: String,
    lens_value: Value,
    lens_name: String,
    lens_suffix: String,
}

/// Per-fanout, lens-independent state threaded through every
/// `build_lens_call_future` invocation. Keeps the call-builder
/// signature narrow.
struct LensCallContext {
    shared_prefix: String,
    task_brief: String,
    shutdown: Shutdown,
    usage: Option<Arc<UsageTracker>>,
    logger: Option<Arc<TurnLogger>>,
}

#[derive(Clone, Copy)]
enum CacheMode {
    /// Try a sequential seed call with `cache_control` to prime the
    /// shared prefix, then fan out the rest with `cache_control`. If
    /// the seed call fails, rerun every lens in parallel with no
    /// `cache_control` so a single transient error can't stall the
    /// whole fan-out.
    PrimeThenParallel,
    /// Skip cache priming entirely: run every lens in parallel with
    /// no `cache_control`. Used by the repair retry path.
    Parallel,
}

/// Per-invocation arguments for `AgentRunner::run_prepared_lens_fanout`.
/// `lenses` is the subset to run this pass (e.g. just the failing
/// lens-ids on a repair retry); `all_lenses` is the full slate, used
/// to populate each lens's `parallel_lenses.other_lenses` descriptor.
struct LensFanoutCall<'a> {
    lenses: &'a [LensSpec],
    all_lenses: &'a [LensSpec],
    extra_lens_instruction: Option<&'a str>,
    cache_mode: CacheMode,
}

/// No-op fetcher used in tests. It returns empty results regardless
/// of input — good enough to prove the agent-runner plumbing without
/// hitting any real backend.
pub struct NullFetcher;

#[async_trait]
impl DataFetcher for NullFetcher {
    async fn fetch(
        &self,
        _followups: &[Followup],
        _plan: Option<&kres_core::Plan>,
    ) -> Result<FetchResult, AgentError> {
        Ok(FetchResult::default())
    }
}

/// Per-Task-turn fast-gather + slow-synthesis engine.
///
/// Holds the fast and slow LLM clients, a data fetcher (gating
/// followups to a per-step allowlist when wired by
/// `workflow_runner::agent_runner_with_gated_fetcher`), the loaded
/// skills payload, and per-call accounting. Built once per REPL
/// session by `kres-repl::session::build_agent_runner`.
///
/// Not to be confused with the `orchestrator` step in
/// `configs/workflows/fix.json` — that is a workflow step whose
/// inference call happens to run through this struct, but it is a
/// separate concept (the LLM-driven routing decision step inside the
/// fix workflow). This struct is the inference engine; the step is a
/// JSON-described worker that uses it.
pub struct AgentRunner {
    pub fast_client: Arc<Client>,
    pub fast_model: Model,
    pub fast_system: Option<String>,
    pub fast_max_tokens: u32,
    pub fast_max_input_tokens: Option<u32>,
    pub fast_thinking: Option<ThinkingBudget>,

    pub slow_client: Arc<Client>,
    pub slow_model: Model,
    pub slow_system: Option<String>,
    pub slow_max_tokens: u32,
    pub slow_max_input_tokens: Option<u32>,
    pub slow_thinking: Option<ThinkingBudget>,

    /// Additional slow-agent variants used by review comparison
    /// mode. The primary slow_* fields above are kept for the
    /// historical single-model path; this list contains all variants,
    /// including the primary one, when comparison is enabled.
    pub slow_variants: Vec<SlowAgentVariant>,

    /// Optional `<results>/comparison.json` destination. Review
    /// comparison appends one entry per completed lensed task.
    pub comparison_path: Option<PathBuf>,
    pub comparison_lock: Arc<Mutex<()>>,

    /// Slow-agent system prompt used when a task runs in
    /// `TaskMode::Coding`. The session loads
    /// `configs/prompts/slow-code-agent-coding.system.md` (or its
    /// `~/.kres/prompts/` override) into this field at startup. When
    /// `None`, a coding task falls back to the normal `slow_system`
    /// — cheap compatibility, but the slow agent will still try to
    /// emit findings-shaped output, which the coding path ignores.
    pub slow_coding_system: Option<String>,

    /// Slow-agent system prompt used when a task runs in
    /// `TaskMode::Generic`. Loaded from
    /// `configs/prompts/slow-code-agent-generic.system.md`. Unlike
    /// the audit prompt it doesn't force the "you are a deep
    /// defect-analysis agent, emit findings" stance — generic
    /// tasks answer the operator's question directly and can emit
    /// `bash` followups for execution-style prompts. When `None`,
    /// generic tasks fall back to `slow_system` (the operator gets
    /// audit-flavoured behaviour, which is usually fine but not
    /// ideal for free-form questions).
    pub slow_generic_system: Option<String>,

    /// System prompt for fast-routed synthesis calls — workflow
    /// steps tagged `agent: fast` in fix.json (orchestrator,
    /// research, lore-search, fixes-tag-search, compile-triage).
    /// Loaded from `configs/prompts/routing-agent.system.md`. Says
    /// "you are a routing/decision agent; the user message is
    /// authoritative; emit only the JSON it specifies." Replaces
    /// the fast-gather system prompt (which is written for the
    /// gather role and tells the model to emit followups and set
    /// `ready_for_slow=true` — wrong shape for a synthesis call
    /// that needs typed JSON). When `None`, falls back to
    /// `fast_system`.
    pub routing_system: Option<String>,

    pub fetcher: Arc<dyn DataFetcher>,

    /// Max rounds of fast↔main before forcing the slow agent.
    pub max_fast_rounds: u8,

    /// Pre-loaded skills (already filtered to the
    /// `invocation_policy: automatic` set). Attached to every fast
    /// prompt as the `skills` JSON field.
    pub skills: Option<Value>,

    /// Optional accounting sink for per-call token usage. Counts are
    /// recorded under ("fast"|"slow"|"main", model_id) keys.
    pub usage: Option<Arc<UsageTracker>>,

    /// Optional per-session turn logger. When set, every fast/slow
    /// round-trip appends a user+assistant entry to code.jsonl.
    pub logger: Option<Arc<TurnLogger>>,
}

#[derive(Clone)]
pub struct SlowAgentVariant {
    pub client: Arc<Client>,
    pub model: Model,
    pub system: Option<String>,
    pub max_tokens: u32,
    pub max_input_tokens: Option<u32>,
    pub thinking: Option<ThinkingBudget>,
    pub label: String,
}

/// Inputs to one run that vary per-task. Separated from AgentRunner
/// so a single AgentRunner can run many tasks in parallel, each with
/// its own previous_findings / task_brief / original_prompt.
#[derive(Debug, Clone, Default)]
pub struct RunContext {
    /// Findings from prior turns; attached to the slow-agent prompt
    /// via CodePrompt::with_previous_findings.
    pub previous_findings: Vec<Finding>,
    /// Short human label for logs.
    pub task_brief: String,
    /// Top-level prompt that originally spawned the current task
    /// chain. Prepended to every fast/slow/main user turn so a
    /// derived task doesn't lose the operator's original question
    pub original_prompt: String,
    /// Optional fast-agent gather prompt. Workflow steps use this
    /// to keep final output schemas away from the gather phase while
    /// preserving the exact final prompt for the slow/coding agent.
    /// When unset, gather and final synthesis both use `prompt`.
    pub gather_prompt: Option<String>,
    /// Which pipeline this task should run. `Analysis` (default)
    /// feeds the findings merger; `Coding` swaps in
    /// `slow_coding_system`, skips the lens fan-out, and returns a
    /// TaskSummary with `code_output` populated and `findings`
    /// empty. The session sets this from `define_goal`'s classifier.
    pub mode: kres_core::TaskMode,
    /// Plan produced by [`crate::define_plan`] for the operator's
    /// top-level prompt, or None when no planner was configured or
    /// it failed. Forwarded to every agent turn (fast + slow via
    /// `CodePrompt`, main via `DataFetcher::set_plan_context`, goal
    /// via `check_goal`) so every LLM call sees the same plan
    /// alongside the derived goal.
    pub plan: Option<kres_core::Plan>,
    /// True on the first task spawned from a given top-level
    /// prompt — the task that immediately follows `define_plan`.
    /// Controls whether the slow agent is told it may rewrite the
    /// plan in its response; subsequent pipeline-driven tasks keep
    /// this false so plan churn stays bounded.
    pub allow_plan_rewrite: bool,
    /// When true, the slow agent's LLM call asks the client to
    /// surface `LlmError::OverInputLimit` instead of internally
    /// shrinking the user-message text on a 429-with-over-limit.
    /// Lets the caller perform structured shrinking (e.g. prune a
    /// workflow step's prior_attempts) and re-issue the run. Other
    /// callers leave this false and keep the existing auto-shrink.
    pub surface_over_input_limit: bool,
    /// Route the synthesis call (the one after the fast-gather loop)
    /// to the fast client instead of the slow client. Used by
    /// workflow steps declared `agent: fast` in fix.json so a
    /// routing/classification step (orchestrator, compile-triage,
    /// fixes-tag-search, etc.) doesn't burn Opus output time on a
    /// decision Sonnet can make. When false (default), the slow
    /// client runs the synthesis call as before.
    pub synthesis_use_fast: bool,
    /// Use the dedicated routing-agent system prompt for this
    /// synthesis call. ONLY for the orchestrator workflow step in
    /// fix.json — that step is pure routing over typed inputs, with
    /// no code analysis or context gathering, and the routing prompt
    /// matches that shape. Other fast-tagged steps (research,
    /// lore-search, fixes-tag-search, compile-triage) DO analyze
    /// gathered code/history and keep the fast-gather system prompt
    /// at synthesis time. Default false preserves the existing
    /// fast_system / slow_system selection.
    pub synthesis_use_routing_prompt: bool,
}

fn record_usage(
    tracker: &Option<Arc<UsageTracker>>,
    role: &str,
    model: &Model,
    usage: &kres_llm::request::Usage,
) {
    if let Some(t) = tracker {
        t.record(
            role,
            &model.id,
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_creation_input_tokens,
            usage.cache_read_input_tokens,
        );
    }
}

/// Collapse a `Usage` into the shape the TurnLogger serialises.
fn log_usage(u: &kres_llm::request::Usage) -> LoggedUsage {
    LoggedUsage {
        input: u.input_tokens,
        output: u.output_tokens,
        cache_creation: u.cache_creation_input_tokens,
        cache_read: u.cache_read_input_tokens,
    }
}

fn slow_variant_call_config(
    model: Model,
    max_tokens: u32,
    max_input_tokens: Option<u32>,
    thinking: Option<ThinkingBudget>,
    system: Option<String>,
    stream_label: String,
) -> CallConfig {
    let mut cfg = CallConfig::defaults_for(model)
        .with_max_tokens(max_tokens)
        .with_stream_label(stream_label);
    if let Some(thinking) = thinking {
        cfg = cfg.with_thinking(thinking);
    }
    if let Some(system) = system {
        cfg = cfg.with_system(system);
    }
    if let Some(n) = max_input_tokens {
        cfg = cfg.with_max_input_tokens(n);
    }
    cfg
}

/// Build a single lens-call future. `use_cache=true` sends the
/// shared prefix as a `cache_control: ephemeral` block; `use_cache=false`
/// folds the prefix into plain content so the request carries no
/// cache breakpoint at all. Same byte-identical prompt either way —
/// only the wire framing changes.
fn build_lens_call_future(
    spec: LensCallSpec,
    variant: SlowAgentVariant,
    ctx: LensCallContext,
    use_cache: bool,
) -> impl std::future::Future<Output = RawLensResult> + Send + 'static {
    let LensCallSpec {
        lens_id,
        lens_value,
        lens_name,
        lens_suffix,
    } = spec;
    let SlowAgentVariant {
        client,
        model,
        system,
        max_tokens,
        max_input_tokens,
        thinking,
        label: model_label,
    } = variant;
    let LensCallContext {
        shared_prefix,
        task_brief,
        shutdown,
        usage,
        logger,
    } = ctx;
    let lens_label = format!("lens {lens_name} ({model_label})");
    let log_label = format!("phase=slow-lens task={task_brief} lens={lens_id} model={model_label}");
    let lens_logged = format!("{shared_prefix}{lens_suffix}");
    async move {
        let messages = if use_cache {
            vec![Message {
                role: "user".into(),
                content: lens_suffix,
                cache: false,
                cached_prefix: Some(shared_prefix),
            }]
        } else {
            vec![Message {
                role: "user".into(),
                content: lens_logged.clone(),
                cache: false,
                cached_prefix: None,
            }]
        };
        let cfg = slow_variant_call_config(
            model.clone(),
            max_tokens,
            max_input_tokens,
            thinking,
            system,
            lens_label,
        );
        if let Some(lg) = &logger {
            let meta = cfg.request_meta();
            lg.log_code_labeled_with_request(
                "user",
                Some(&log_label),
                &lens_logged,
                None,
                None,
                Some(&meta),
            );
        }
        tokio::select! {
            _ = shutdown.cancelled() => Err((lens_id, model_label, "cancelled".to_string())),
            r = client.messages_streaming(&cfg, &messages) => match r {
                Ok(resp) => {
                    record_usage(&usage, "slow", &model, &resp.usage);
                    let t = extract_text(&resp);
                    if let Some(lg) = &logger {
                        let th = extract_thinking(&resp);
                        lg.log_code_labeled(
                            "assistant",
                            Some(&log_label),
                            &t,
                            Some(log_usage(&resp.usage)),
                            th.as_deref(),
                        );
                    }
                    let parsed = parse_code_response(&t);
                    Ok((lens_id, lens_value, model_label, t, parsed))
                }
                Err(e) => {
                    tracing::warn!(target: "kres_agents", "lens call failed: {e}");
                    Err((lens_id, model_label, e.to_string()))
                }
            }
        }
    }
}

/// Extract the concatenated "thinking" block text from a response, if
/// any — =` argument to `log_code`.
fn extract_thinking(resp: &kres_llm::request::MessagesResponse) -> Option<String> {
    let mut out = String::new();
    for block in &resp.content {
        if let kres_llm::request::ContentBlock::Thinking { thinking } = block {
            out.push_str(thinking);
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

#[derive(Debug, Clone)]
pub struct TaskSummary {
    /// Raw slow-agent text before parsing into the standard kres
    /// response envelope. Workflow steps may declare typed outputs that are
    /// valid JSON but are not `analysis`/`findings`/`followups`; the
    /// workflow runner needs this text to extract those fields.
    pub raw_response: String,
    pub analysis: String,
    pub findings: Vec<Finding>,
    pub followups: Vec<Followup>,
    pub fast_rounds: u8,
    pub strategy: ParseStrategy,
    /// Pipeline the task ran through. `Analysis` is the default and
    /// matches the historical shape (findings in `findings`); `Coding`
    /// means the slow agent produced source files in `code_output`
    /// instead and the merger/consolidator should be skipped.
    pub mode: kres_core::TaskMode,
    /// Source files emitted by a Coding-mode task. Empty for
    /// Audit-mode tasks.
    pub code_output: Vec<kres_core::CodeFile>,
    /// String-replacement edits emitted by a Coding-mode task.
    /// The reaper applies each entry via tools::edit_file.
    pub code_edits: Vec<kres_core::CodeEdit>,
    /// Optional rewritten plan proposed by the slow agent. Wire
    /// shape is `{steps: [...]}` via [`kres_core::PlanRewrite`] —
    /// the caller merges it with the existing plan's metadata via
    /// `apply_to` before handing it to `mgr.set_plan`. Populated
    /// only when the slow agent emitted a `plan` field; only the
    /// first slow call per top-level prompt is expected to set
    /// this (see `RunContext.allow_plan_rewrite`).
    pub plan: Option<kres_core::PlanRewrite>,
}

impl AgentRunner {
    /// Convenience wrapper with an empty RunContext.
    pub async fn run_once(
        &self,
        prompt: &str,
        shutdown: &Shutdown,
    ) -> Result<TaskSummary, AgentError> {
        self.run_once_with_ctx(prompt, &RunContext::default(), shutdown)
            .await
    }

    /// Run one turn. `ctx.previous_findings` is shipped to the slow
    /// agent so it can dedup + build chains with earlier turns.
    pub async fn run_once_with_ctx(
        &self,
        prompt: &str,
        ctx: &RunContext,
        shutdown: &Shutdown,
    ) -> Result<TaskSummary, AgentError> {
        let composed = prepend_original_prompt(prompt, &ctx.original_prompt);
        let prompt: &str = composed.as_str();
        let gather_composed;
        let gather_prompt = if let Some(gather) = ctx.gather_prompt.as_deref() {
            gather_composed = prepend_original_prompt(gather, &ctx.original_prompt);
            gather_composed.as_str()
        } else {
            prompt
        };
        let log_task = if ctx.task_brief.is_empty() {
            "task".to_string()
        } else {
            ctx.task_brief.clone()
        };
        // Per-run skills clone — mutated mid-loop by `skill_reads`
        // responses so the fast agent can pull in extra files
        // mid-gather (§27,).
        let mut live_skills: Option<Value> = self.skills.clone();
        let mut symbols: Vec<Value> = Vec::new();
        let mut context: Vec<Value> = Vec::new();
        let mut prev_n_syms: usize = 0;
        let mut prev_n_ctx: usize = 0;
        let mut fast_rounds: u8 = 0;
        let mut fetched_keys: HashSet<String> = HashSet::new();

        for round in 0..self.max_fast_rounds {
            if shutdown.is_cancelled() {
                return Err(AgentError::Other(format!(
                    "shutdown cancelled during fast round {round}"
                )));
            }
            fast_rounds = round + 1;

            let new_syms = if symbols.len() > prev_n_syms {
                &symbols[prev_n_syms..]
            } else {
                &[]
            };
            let new_ctx = if context.len() > prev_n_ctx {
                &context[prev_n_ctx..]
            } else {
                &[]
            };
            // §7 / §28: on round 2+, ship the identity-only
            // `previously_fetched` manifest so the fast agent sees
            // "you already have these" without paying for full-body
            // retransmission.
            let pf_manifest = if prev_n_syms > 0 || prev_n_ctx > 0 {
                Some(crate::symbol::previously_fetched_manifest(
                    &symbols[..prev_n_syms],
                    &context[..prev_n_ctx],
                ))
            } else {
                None
            };
            let mut cp = CodePrompt::new(gather_prompt)
                .with_symbols(new_syms)
                .with_context(new_ctx);
            if let Some(ref pf) = pf_manifest {
                cp = cp.with_previously_fetched(pf);
            }
            if let Some(sk) = &live_skills {
                cp = cp.with_skills(sk);
            }
            if let Some(ref p) = ctx.plan {
                cp = cp.with_plan(p);
            }
            // §cache: split the envelope into a stable prefix
            // (question + skills + previous_findings + parallel_lenses
            // + plan) and a per-round volatile tail (symbols + context
            // + previously_fetched). The prefix cache-hits across
            // rounds; the tail does not.
            let (prefix, suffix) = cp.to_cached_split_json(CACHED_PREFIX_FIELDS)?;
            prev_n_syms = symbols.len();
            prev_n_ctx = context.len();

            let logged_content = format!("{prefix}{suffix}");
            let messages = vec![Message {
                role: "user".into(),
                content: suffix.clone(),
                cache: true,
                cached_prefix: if prefix.is_empty() {
                    None
                } else {
                    Some(prefix.clone())
                },
            }];
            let mut cfg = CallConfig::defaults_for(self.fast_model.clone())
                .with_max_tokens(self.fast_max_tokens)
                .with_stream_label(format!("fast round {fast_rounds}"));
            if let Some(thinking) = self.fast_thinking {
                cfg = cfg.with_thinking(thinking);
            }
            if let Some(s) = &self.fast_system {
                cfg = cfg.with_system(s.clone());
            }
            if let Some(n) = self.fast_max_input_tokens {
                cfg = cfg.with_max_input_tokens(n);
            }

            if let Some(lg) = &self.logger {
                let label = format!("phase=fast-gather task={log_task} round={fast_rounds}");
                lg.log_code_labeled("user", Some(&label), &logged_content, None, None);
            }
            if !new_syms.is_empty() || !new_ctx.is_empty() {
                kres_core::async_eprintln!(
                    "[fast round {fast_rounds}/{}] gathered +{} symbol(s), +{} context item(s)",
                    self.max_fast_rounds,
                    new_syms.len(),
                    new_ctx.len(),
                );
            }
            let text = tokio::select! {
                _ = shutdown.cancelled() => {
                    return Err(AgentError::Other("cancelled during fast call".into()));
                }
                r = self.fast_client.messages_streaming(&cfg, &messages) => {
                    let resp = r.map_err(|e| AgentError::Other(e.to_string()))?;
                    record_usage(&self.usage, "fast", &self.fast_model, &resp.usage);
                    let t = extract_text(&resp);
                    if let Some(lg) = &self.logger {
                        let thinking = extract_thinking(&resp);
                        let label = format!("phase=fast-gather task={log_task} round={fast_rounds}");
                        lg.log_code_labeled(
                            "assistant",
                            Some(&label),
                            &t,
                            Some(log_usage(&resp.usage)),
                            thinking.as_deref(),
                        );
                    }
                    t
                }
            };
            let parsed = parse_code_response(&text);

            // §27: honour skill_reads. Read each requested file and
            // graft it into the first skill's `files` map so the next
            // fast round sees the new content. Matches.
            if !parsed.skill_reads.is_empty() {
                kres_core::async_eprintln!(
                    "[fast round {}] skill_reads: {}",
                    fast_rounds,
                    parsed.skill_reads.join(", ")
                );
                apply_skill_reads(&mut live_skills, &parsed.skill_reads);
            }

            // bugs.md#Phase1 fix: if this round produced ONLY
            // skill_reads (no followups, not ready) we must loop
            // back, not jump to slow.
            let only_skill_reads = parsed.followups.is_empty()
                && !parsed.ready_for_slow
                && !parsed.skill_reads.is_empty();
            if parsed.ready_for_slow {
                kres_core::async_eprintln!("[fast round {fast_rounds}] ready for slow analysis");
                break;
            }
            if parsed.followups.is_empty() && !only_skill_reads {
                kres_core::async_eprintln!(
                    "[fast round {}] no more fetches; proceeding to slow analysis",
                    fast_rounds
                );
                break;
            }
            if only_skill_reads {
                continue;
            }

            // If every followup is a type:question (clarification
            // for the operator), the main agent can't fetch data
            // for it — looping would just re-ask the same question.
            // Break out to the slow agent, which can surface the
            // question to the operator via its own followups.
            if !parsed.followups.is_empty() && parsed.followups.iter().all(|f| f.kind == "question")
            {
                kres_core::async_eprintln!(
                    "[fast round {}] only type:question followups — breaking to slow",
                    fast_rounds
                );
                break;
            }

            // Dedup followups against previously-fetched keys.
            // The fast agent is stateless — it can see the
            // previously_fetched manifest (names only) but not the
            // content from earlier rounds.  When it needs that
            // content to decide it's ready, it re-requests the same
            // data.  Detect this and break to the slow agent, which
            // receives ALL accumulated data.
            let novel: Vec<_> = parsed
                .followups
                .iter()
                .filter(|fu| !fetched_keys.contains(&fu.cache_key()))
                .cloned()
                .collect();
            let n_dupes = parsed.followups.len() - novel.len();
            if n_dupes > 0 && novel.is_empty() {
                kres_core::async_eprintln!(
                    "[fast round {}] all {} followup(s) are re-requests — breaking to slow",
                    fast_rounds,
                    parsed.followups.len(),
                );
                break;
            }
            if n_dupes > 0 {
                kres_core::async_eprintln!(
                    "[fast round {}] deduped {} re-request(s), {} novel followup(s) remain",
                    fast_rounds,
                    n_dupes,
                    novel.len(),
                );
            }

            // Summarise the followups so operators can see what the
            // fast agent asked the main agent to fetch on their
            // behalf.
            let fu_summary: Vec<String> = novel
                .iter()
                .take(8)
                .map(|fu| format!("{}:{}", fu.kind, truncate(&fu.name, 40)))
                .collect();
            let tail = if novel.len() > 8 {
                format!(", +{} more", novel.len() - 8)
            } else {
                String::new()
            };
            kres_core::async_eprintln!(
                "[fast round {}] {} followup(s): {}{tail}",
                fast_rounds,
                novel.len(),
                fu_summary.join(", "),
            );

            for fu in &novel {
                fetched_keys.insert(fu.cache_key());
            }

            // Fetch via main agent (data layer).
            let fetched = tokio::select! {
                _ = shutdown.cancelled() => {
                    return Err(AgentError::Other("cancelled during fetch".into()));
                }
                f = self.fetcher.fetch(&novel, ctx.plan.as_ref()) => f?,
            };
            let got_syms = fetched.symbols.len();
            let got_ctx = fetched.context.len();
            let got_chars: usize = fetched
                .symbols
                .iter()
                .chain(fetched.context.iter())
                .map(|v| serde_json::to_string(v).map(|s| s.len()).unwrap_or(0))
                .sum();
            kres_core::async_eprintln!(
                "[fast round {}] fetch returned {} symbol(s), {} context item(s) ({}k chars)",
                fast_rounds,
                got_syms,
                got_ctx,
                got_chars / 1000,
            );
            symbols.extend(fetched.symbols);
            context.extend(fetched.context);
        }

        // Slow agent call.
        // Redact `details` before ANY budget / shipping step — the
        // per-task narrative stored on Finding.details is for
        // /summary only and must never reach an agent prompt.
        // bugs.md#L5: budget previous_findings to ~1M chars before
        // shipping. Symbols and context are also trimmed — a single
        // fetcher response can blow the per-slot budget even with a
        // short gather loop (e.g. a multi-MB type definition).
        let redacted_prev = kres_core::redact_findings_for_agent(&ctx.previous_findings);
        let trimmed_prev = shrink_findings_to_budget(&redacted_prev, 1_000_000);
        let trimmed_symbols = shrink_json_list_to_budget(&symbols, 1_000_000);
        let trimmed_context = shrink_json_list_to_budget(&context, 1_000_000);
        // Non-lensed slow calls are one-shot. Do not split off a
        // cached prefix here: there is no parallel fan-out to amortize
        // the cache write, and repeated workflow correction passes
        // should not keep paying to create slow-agent cache entries.
        // Review paths that actually benefit from a shared context
        // prefix go through run_with_lenses below.
        let mut slow_cp = CodePrompt::new(prompt)
            .with_symbols(&trimmed_symbols)
            .with_context(&trimmed_context)
            .with_previous_findings(&trimmed_prev);
        // Pass `live_skills` (post-skill_reads, §27), NOT
        // `self.skills` — if the fast agent pulled additional
        // files into the skill's `files` map mid-loop, the slow
        // agent needs those files too. Using `self.skills` would
        // hand the slow agent a stale, smaller skill payload.
        if let Some(sk) = &live_skills {
            slow_cp = slow_cp.with_skills(sk);
        }
        if let Some(ref p) = ctx.plan {
            slow_cp = slow_cp.with_plan(p);
        }
        if ctx.allow_plan_rewrite {
            slow_cp = slow_cp.with_plan_rewrite_allowed(true);
        }
        let slow_logged = slow_cp.to_json_string()?;
        let messages = vec![Message {
            role: "user".into(),
            content: slow_logged.clone(),
            cache: false,
            cached_prefix: None,
        }];
        // Route the synthesis call to the fast client when the
        // caller (typically a workflow step declared `agent: fast`)
        // asked for it. Default is slow — coding/review/deep-audit
        // work needs Opus. Routing/classification steps
        // (orchestrator, compile-triage, fixes-tag-search,
        // lore-search) pay Opus output time unnecessarily today;
        // synthesis_use_fast lets fix.json's `agent: fast` actually
        // mean fast.
        let use_fast = ctx.synthesis_use_fast;
        let log_phase = if use_fast { "fast-synth" } else { "slow" };
        let (
            synth_client,
            synth_model,
            synth_max_tokens,
            synth_max_in,
            synth_thinking,
            label_prefix,
        ) = if use_fast {
            (
                &self.fast_client,
                self.fast_model.clone(),
                self.fast_max_tokens,
                self.fast_max_input_tokens,
                self.fast_thinking,
                "fast-synth",
            )
        } else {
            (
                &self.slow_client,
                self.slow_model.clone(),
                self.slow_max_tokens,
                self.slow_max_input_tokens,
                self.slow_thinking,
                "slow",
            )
        };
        let mut cfg = CallConfig::defaults_for(synth_model.clone())
            .with_max_tokens(synth_max_tokens)
            .with_stream_label(match (use_fast, ctx.mode) {
                (true, _) => format!("{label_prefix} ({:?})", ctx.mode).to_lowercase(),
                (false, kres_core::TaskMode::Audit) => "slow".into(),
                (false, kres_core::TaskMode::Generic) => "slow (generic)".into(),
                (false, kres_core::TaskMode::Coding) => "slow (coding)".into(),
            });
        if let Some(thinking) = synth_thinking {
            cfg = cfg.with_thinking(thinking);
        }
        // System prompt selection:
        // - synthesis_use_routing_prompt (orchestrator step only):
        //   use the dedicated routing-agent system prompt. That step
        //   is pure routing over typed inputs and the routing prompt
        //   matches that shape.
        // - else use_fast (other fast-tagged steps): use the
        //   fast-gather system prompt. Those steps (research,
        //   lore-search, fixes-tag-search, compile-triage) DO
        //   analyze gathered code/history and the fast-gather prompt
        //   is the operator's chosen system prompt for `agent: fast`.
        // - else: per-mode slow system prompt (coding/generic/audit).
        let system_for_call = if ctx.synthesis_use_routing_prompt {
            self.routing_system.as_ref().or(self.fast_system.as_ref())
        } else if use_fast {
            self.fast_system.as_ref()
        } else {
            match ctx.mode {
                kres_core::TaskMode::Coding => {
                    if self.slow_coding_system.is_some() {
                        self.slow_coding_system.as_ref()
                    } else {
                        kres_core::async_eprintln!(
                            "[{log_phase}] coding-mode task but no slow_coding_system loaded — falling back to audit prompt"
                        );
                        self.slow_system.as_ref()
                    }
                }
                kres_core::TaskMode::Generic => {
                    if self.slow_generic_system.is_some() {
                        self.slow_generic_system.as_ref()
                    } else {
                        self.slow_system.as_ref()
                    }
                }
                kres_core::TaskMode::Audit => self.slow_system.as_ref(),
            }
        };
        if let Some(s) = system_for_call {
            cfg = cfg.with_system(s.clone());
        }
        if let Some(n) = synth_max_in {
            cfg = cfg.with_max_input_tokens(n);
        }
        if ctx.surface_over_input_limit {
            cfg = cfg.with_surface_over_input_limit(true);
        }
        // Log label reflects which model actually ran the synthesis
        // so trace consumers (and grep'ing operators) can see whether
        // a fast-routed step actually used Sonnet. log_phase is set
        // above (next to use_fast) so it's available for in-loop
        // error messages and for the post-call logger.
        if let Some(lg) = &self.logger {
            let label = format!("phase={log_phase} task={log_task}");
            let meta = cfg.request_meta();
            lg.log_code_labeled_with_request(
                "user",
                Some(&label),
                &slow_logged,
                None,
                None,
                Some(&meta),
            );
        }
        kres_core::async_eprintln!(
            "[{log_phase}] analyzing with {} symbol(s), {} context item(s), {} previous finding(s)",
            trimmed_symbols.len(),
            trimmed_context.len(),
            trimmed_prev.len(),
        );
        let synth_role_for_usage = if use_fast { "fast" } else { "slow" };
        let text = tokio::select! {
            _ = shutdown.cancelled() => {
                return Err(AgentError::Other(format!("cancelled during {log_phase} call")));
            }
            r = synth_client.messages_streaming(&cfg, &messages) => {
                let resp = r.map_err(|e| match e {
                    kres_llm::LlmError::OverInputLimit { actual, limit } => {
                        AgentError::OverInputLimit { actual, limit }
                    }
                    other => AgentError::Other(other.to_string()),
                })?;
                record_usage(&self.usage, synth_role_for_usage, &synth_model, &resp.usage);
                let t = extract_text(&resp);
                if let Some(lg) = &self.logger {
                    let thinking = extract_thinking(&resp);
                    let label = format!("phase={log_phase} task={log_task}");
                    lg.log_code_labeled(
                        "assistant",
                        Some(&label),
                        &t,
                        Some(log_usage(&resp.usage)),
                        thinking.as_deref(),
                    );
                }
                t
            }
        };
        let mut slow_parsed = parse_code_response(&text);
        // bugs.md#M3: surface the non-JSON case instead of letting it
        // masquerade as a valid-but-empty analysis. The strategy
        // field is also on TaskSummary for callers that want to
        // react in-band.
        //
        // Rescue path: when the slow agent returned pure prose, any
        // bug claims in that prose would otherwise be lost (findings
        // stays empty, the merger has nothing to promote — see
        // slow-code-agent-audit.system.md:37 "a bug that exists only in
        // prose will be LOST"). Ask the fast agent to translate the
        // prose into the expected envelope. If the translation also
        // fails to produce parseable JSON, keep the original
        // RawText result so the prose at least survives as analysis.
        if slow_parsed.strategy == ParseStrategy::RawText {
            tracing::warn!(
                target: "kres_agents",
                fast_rounds,
                "slow agent returned no parseable JSON; attempting fast-agent translation"
            );
            match self
                .translate_slow_raw_text(&slow_parsed.analysis, &ctx.task_brief, shutdown)
                .await
            {
                Some(translated) => {
                    kres_core::async_eprintln!(
                        "[slow] rescued via fast-agent translation: {} finding(s), {} followup(s)",
                        translated.findings.len(),
                        translated.followups.len(),
                    );
                    slow_parsed = translated;
                }
                None => {
                    tracing::warn!(
                        target: "kres_agents",
                        "fast-agent translation failed; prose preserved as analysis with no structured findings"
                    );
                }
            }
        }
        kres_core::async_eprintln!(
            "[slow] complete: {} finding(s), {} followup(s)",
            slow_parsed.findings.len(),
            slow_parsed.followups.len(),
        );
        if !slow_parsed.analysis.trim().is_empty() {
            kres_core::async_eprintln!(
                "[slow] analysis: {}",
                truncate(&one_line(&slow_parsed.analysis), 900)
            );
        }
        if !slow_parsed.followups.is_empty() {
            let fus: Vec<String> = slow_parsed
                .followups
                .iter()
                .take(5)
                .map(|fu| format!("{}:{}", fu.kind, truncate(&fu.name, 40)))
                .collect();
            let tail = if slow_parsed.followups.len() > 5 {
                format!(", +{} more", slow_parsed.followups.len() - 5)
            } else {
                String::new()
            };
            kres_core::async_eprintln!(
                "[slow] slow-agent followups (unmet wishes): {}{tail}",
                fus.join(", ")
            );
        }
        // For coding tasks, surface the emitted files in code_output
        // and drop any findings the model tried to emit anyway — a
        // coding task is not supposed to participate in the findings
        // pipeline (the reaper will skip merge/consolidator on this
        // mode). Analysis and Generic tasks keep the historical
        // shape (findings go through the merger) and do not emit
        // in-place edits — edits only flow from coding mode.
        let (findings_out, code_output, code_edits) = match ctx.mode {
            kres_core::TaskMode::Audit | kres_core::TaskMode::Generic => {
                (slow_parsed.findings, Vec::new(), Vec::new())
            }
            kres_core::TaskMode::Coding => {
                (Vec::new(), slow_parsed.code_output, slow_parsed.code_edits)
            }
        };
        // Only surface a slow-agent plan rewrite when this task is
        // the first slow call for the top-level prompt. Later
        // pipeline-driven tasks going through run_once_with_ctx
        // (follow-ups) are NOT permitted to reshape the plan — the
        // todo agent's per-turn reevaluation handles incremental
        // updates, and letting every slow call rewrite would churn
        // step ids mid-sweep and break the step_id→step linkage.
        let slow_plan = if ctx.allow_plan_rewrite {
            slow_parsed.plan
        } else {
            None
        };
        Ok(TaskSummary {
            raw_response: text,
            analysis: slow_parsed.analysis,
            findings: findings_out,
            followups: slow_parsed.followups,
            fast_rounds,
            strategy: slow_parsed.strategy,
            mode: ctx.mode,
            code_output,
            code_edits,
            plan: slow_plan,
        })
    }

    /// Re-emit a prose-only slow-agent reply as the JSON envelope the
    /// pipeline expects. Invoked from `run_once_with_ctx` when the
    /// slow call produced `ParseStrategy::RawText`. The fast agent is
    /// told to transcribe — not augment — so this should not invent
    /// findings the prose doesn't already make.
    ///
    /// Returns the reparsed response, or `None` when the translation
    /// itself couldn't be parsed (the caller then keeps the original
    /// prose-as-analysis result so the content isn't lost entirely).
    /// Deliberately skips `self.fast_system` — the fast-agent system
    /// prompt pushes toward the fast-agent schema (ready_for_slow /
    /// skill_reads), which is the wrong target here. Also skips the
    /// prompt cache — this path fires on the rare non-JSON turn, so
    /// the ~4KB translation prompt isn't worth a breakpoint.
    async fn translate_slow_raw_text(
        &self,
        prose: &str,
        task_brief: &str,
        shutdown: &Shutdown,
    ) -> Option<CodeResponse> {
        let user_content = format!(
            "The slow agent returned the analysis below as free-form prose \
             instead of the required JSON envelope. Re-emit the SAME CONTENT \
             as strict JSON with this shape:\n\
             {{\"analysis\": \"<prose narrative>\", \"findings\": [<Finding>, ...], \
             \"followups\": [{{\"type\": \"T\", \"name\": \"N\", \"reason\": \"R\"}}]}}\n\n\
             Rules:\n\
             - Do NOT invent new bugs or analysis. Transcribe the prose.\n\
             - Every actionable bug described in the prose MUST appear as a \
               Finding record with this schema: id (snake_case slug), title, \
               severity (low|medium|high), status ('active'), \
               relevant_symbols (array of {{name, filename, line, definition}}), \
               relevant_file_sections (array of {{filename, line_start, \
               line_end, content}}), summary, reproducer_sketch, impact. \
               Optional: mechanism_detail, fix_sketch, open_questions, \
               related_finding_ids.\n\
             - If the prose made a bug claim without enough detail for a \
               concrete Finding (no file:line, no reproducer), omit it \
               rather than fabricate fields.\n\
             - Output JSON only, no fences, no preamble.\n\n\
             ---\n\
             Task brief: {task_brief}\n\n\
             Prose analysis to translate:\n\n{prose}"
        );
        let messages = vec![Message {
            role: "user".into(),
            content: user_content.clone(),
            cache: false,
            cached_prefix: None,
        }];
        let mut cfg = CallConfig::defaults_for(self.fast_model.clone())
            .with_max_tokens(self.fast_max_tokens)
            .with_stream_label("fast translate raw slow");
        if let Some(thinking) = self.fast_thinking {
            cfg = cfg.with_thinking(thinking);
        }
        if let Some(n) = self.fast_max_input_tokens {
            cfg = cfg.with_max_input_tokens(n);
        }
        if let Some(lg) = &self.logger {
            lg.log_code("user", &user_content, None, None);
        }
        kres_core::async_eprintln!("[fast translate] structuring raw slow-agent prose");
        let text = tokio::select! {
            _ = shutdown.cancelled() => return None,
            r = self.fast_client.messages_streaming(&cfg, &messages) => {
                match r {
                    Ok(resp) => {
                        record_usage(&self.usage, "fast", &self.fast_model, &resp.usage);
                        let t = extract_text(&resp);
                        if let Some(lg) = &self.logger {
                            let thinking = extract_thinking(&resp);
                            lg.log_code(
                                "assistant",
                                &t,
                                Some(log_usage(&resp.usage)),
                                thinking.as_deref(),
                            );
                        }
                        t
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "kres_agents",
                            "raw-text translation call failed: {e}"
                        );
                        return None;
                    }
                }
            }
        };
        let reparsed = parse_code_response(&text);
        if reparsed.strategy == ParseStrategy::RawText {
            tracing::warn!(
                target: "kres_agents",
                "raw-text translation also returned non-JSON"
            );
            return None;
        }
        Some(reparsed)
    }
}

impl AgentRunner {
    fn append_comparison_entry(
        &self,
        task_brief: &str,
        slow_variants: &[SlowAgentVariant],
        lens_outputs: &[LensOutput<'_>],
        comparison: Option<Value>,
    ) {
        let Some(path) = self.comparison_path.as_ref() else {
            return;
        };
        let _guard = match self.comparison_lock.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                tracing::warn!(
                    target: "kres_agents",
                    "comparison: create {} failed: {e}",
                    parent.display()
                );
                return;
            }
        }
        let mut existing = match std::fs::read_to_string(path) {
            Ok(s) if !s.trim().is_empty() => {
                serde_json::from_str::<Value>(&s).unwrap_or_else(|_| json!([]))
            }
            _ => json!([]),
        };
        if !existing.is_array() {
            existing = json!([]);
        }
        let outputs: Vec<Value> = lens_outputs
            .iter()
            .map(|out| {
                json!({
                    "lens": out.lens,
                    "slow_model": out.slow_model.unwrap_or("unknown"),
                    "analysis_chars": out.analysis.len(),
                    "finding_count": out.findings.len(),
                    "followup_count": out.followups.len(),
                    "finding_ids": out.findings.iter().map(|f| f.id.clone()).collect::<Vec<_>>(),
                })
            })
            .collect();
        let entry = json!({
            "turn": existing.as_array().map(|a| a.len() + 1).unwrap_or(1),
            "created_at": chrono::Utc::now().to_rfc3339(),
            "task_brief": task_brief,
            "slow_models": slow_variants.iter().map(|v| v.label.clone()).collect::<Vec<_>>(),
            "outputs": outputs,
            "comparison": comparison,
        });
        if let Some(arr) = existing.as_array_mut() {
            arr.push(entry);
        }
        match serde_json::to_vec_pretty(&existing) {
            Ok(bytes) => {
                if let Err(e) = std::fs::write(path, bytes) {
                    tracing::warn!(
                        target: "kres_agents",
                        "comparison: write {} failed: {e}",
                        path.display()
                    );
                }
            }
            Err(e) => tracing::warn!(target: "kres_agents", "comparison: encode failed: {e}"),
        }
    }

    /// Run a task with N parallel slow-agent lens calls over the
    /// same gathered symbols/context, then consolidate.
    ///
    /// A lens that fails or returns no usable output is retried once
    /// on the same gathered context. If it still fails, the whole
    /// lensed review errors instead of consolidating a partial clean
    /// result.
    pub async fn run_with_lenses(
        &self,
        prompt: &str,
        lenses: &[LensSpec],
        consolidator: &ConsolidatorClient,
        consolidate_rules: Option<&str>,
        ctx: &RunContext,
        shutdown: &Shutdown,
    ) -> Result<TaskSummary, AgentError> {
        if lenses.is_empty() {
            return self.run_once_with_ctx(prompt, ctx, shutdown).await;
        }
        let fanout = self
            .run_lenses_shared_gather_repairing(
                prompt,
                lenses,
                ctx,
                shutdown,
                LensRepairPolicy {
                    max_retries: GENERIC_LENS_REPAIR_RETRIES,
                    repair_instruction: "Your previous response for this review lens did not produce usable lens output. Reuse the same gathered source/context and reply with this lens's analysis, findings, or followups.",
                },
                validate_generic_lens_output,
            )
            .await?;
        if !fanout.failures.is_empty() {
            return Err(AgentError::Other(format!(
                "shared lens fan-out failed {} of {} lens call(s): {}",
                fanout.failures.len(),
                fanout.attempted,
                fanout.failure_summary()
            )));
        }
        let empty_lenses: Vec<String> = fanout
            .outputs
            .iter()
            .filter_map(|output| validate_generic_lens_output(output).err())
            .collect();
        if !empty_lenses.is_empty() {
            return Err(AgentError::Other(format!(
                "shared lens fan-out produced unusable output for {} lens call(s): {}",
                empty_lenses.len(),
                empty_lenses.join("; ")
            )));
        }

        let mut outs: Vec<LensOutput<'_>> = Vec::new();
        let mut all_followups: Vec<Followup> = Vec::new();
        for output in &fanout.outputs {
            let parsed = &output.parsed;
            outs.push(LensOutput {
                lens: &output.lens,
                slow_model: output.slow_model.as_deref(),
                analysis: &parsed.analysis,
                findings: &parsed.findings,
                followups: &parsed.followups,
            });
            all_followups.extend(parsed.followups.iter().cloned());
        }

        let finished = fanout.outputs.len();
        let findings: usize = fanout
            .outputs
            .iter()
            .map(|output| output.parsed.findings.len())
            .sum();
        kres_core::async_eprintln!(
            "[review lenses] {} of {} complete across {} slow model(s), {} raw finding(s), {} followup(s)",
            finished,
            fanout.attempted,
            fanout.slow_variant_count,
            findings,
            all_followups.len(),
        );

        let consolidated = consolidate_lenses_with_logger(
            consolidator,
            &ctx.task_brief,
            &outs,
            consolidate_rules,
            self.logger.clone(),
        )
        .await?;
        self.append_comparison_entry(
            &ctx.task_brief,
            &self.effective_slow_variants(),
            &outs,
            consolidated.comparison.clone(),
        );
        merge_followups(&mut all_followups, consolidated.followups);
        Ok(TaskSummary {
            raw_response: consolidated.analysis.clone(),
            analysis: consolidated.analysis,
            findings: consolidated.findings,
            followups: all_followups,
            fast_rounds: fanout.fast_rounds,
            strategy: ParseStrategy::WholeBody,
            mode: kres_core::TaskMode::Audit,
            code_output: Vec::new(),
            code_edits: Vec::new(),
            // Lens fan-out runs N parallel slow calls; merging N
            // plan rewrites would churn step ids. Audit-mode plan
            // rewrites flow through the todo-agent's per-turn
            // reevaluation path instead. Single-slow analysis tasks
            // (lens count 0) still get plan rewrite via
            // run_once_with_ctx above.
            plan: None,
        })
    }

    pub async fn run_lenses_shared_gather_repairing<F>(
        &self,
        prompt: &str,
        lenses: &[LensSpec],
        ctx: &RunContext,
        shutdown: &Shutdown,
        repair: LensRepairPolicy<'_>,
        validate: F,
    ) -> Result<LensFanoutOutput, AgentError>
    where
        F: Fn(&LensRunOutput) -> Result<(), String>,
    {
        let prepared = self.prepare_lens_fanout(prompt, ctx, shutdown).await?;
        let mut fanout = self
            .run_prepared_lens_fanout(
                &prepared,
                LensFanoutCall {
                    lenses,
                    all_lenses: lenses,
                    extra_lens_instruction: None,
                    cache_mode: CacheMode::PrimeThenParallel,
                },
                ctx,
                shutdown,
            )
            .await?;

        for retry in 0..repair.max_retries {
            // Collect per-lens errors so we can surface the specific
            // validator/transport message to each retried lens — the
            // generic repair instruction alone does not tell the model
            // why its output was rejected.
            let mut retry_errors: Vec<(String, String)> = Vec::new();
            for failure in &fanout.failures {
                retry_errors.push((failure.lens_id.clone(), failure.error.clone()));
            }
            for output in &fanout.outputs {
                if let Err(e) = validate(output) {
                    retry_errors.push((output.lens_id.clone(), e));
                }
            }
            if retry_errors.is_empty() {
                return Ok(fanout);
            }
            let retry_lenses: BTreeSet<String> =
                retry_errors.iter().map(|(id, _)| id.clone()).collect();

            let repair_lenses: Vec<LensSpec> = lenses
                .iter()
                .filter(|lens| retry_lenses.contains(&lens.id))
                .cloned()
                .collect();
            if repair_lenses.is_empty() {
                return Err(AgentError::Other(format!(
                    "shared lens fan-out selected unknown lens id(s) for repair: {}",
                    retry_lenses.into_iter().collect::<Vec<_>>().join(", ")
                )));
            }

            kres_core::async_eprintln!(
                "[review lenses] repairing {} lens output(s) on shared context (attempt {}/{})",
                repair_lenses.len(),
                retry + 1,
                repair.max_retries,
            );
            let mut detailed_repair = repair.repair_instruction.trim_end().to_string();
            detailed_repair.push_str(
                "\n\nValidator error(s) from the previous attempt (find the entry tagged with your lens id):",
            );
            for (lens_id, err) in &retry_errors {
                detailed_repair.push_str(&format!("\n- lens '{lens_id}': {err}"));
            }
            let repaired = self
                .run_prepared_lens_fanout(
                    &prepared,
                    LensFanoutCall {
                        lenses: &repair_lenses,
                        all_lenses: lenses,
                        extra_lens_instruction: Some(&detailed_repair),
                        cache_mode: CacheMode::Parallel,
                    },
                    ctx,
                    shutdown,
                )
                .await?;
            fanout
                .outputs
                .retain(|output| !retry_lenses.contains(&output.lens_id));
            fanout
                .failures
                .retain(|failure| !retry_lenses.contains(&failure.lens_id));
            fanout.outputs.extend(repaired.outputs);
            fanout.failures.extend(repaired.failures);
        }
        Ok(fanout)
    }

    async fn prepare_lens_fanout(
        &self,
        prompt: &str,
        ctx: &RunContext,
        shutdown: &Shutdown,
    ) -> Result<PreparedLensFanout, AgentError> {
        let composed = prepend_original_prompt(prompt, &ctx.original_prompt);
        let prompt: &str = composed.as_str();
        let gather_composed;
        let gather_prompt = if let Some(gather) = ctx.gather_prompt.as_deref() {
            gather_composed = prepend_original_prompt(gather, &ctx.original_prompt);
            gather_composed.as_str()
        } else {
            prompt
        };
        // Gather once via fast+main (same loop as run_once, up to the
        // point where we'd call the slow agent).
        let (symbols, context, fast_rounds, live_skills) = self
            .gather(gather_prompt, ctx.plan.as_ref(), shutdown)
            .await?;

        // All review lenses share the same gathered source/context.
        // `run_prepared_lens_fanout` runs the first lens sequentially
        // with `cache_control` to prime the Anthropic prompt cache,
        // then fans the rest out in parallel so they cache_read the
        // same prefix. If the seed call fails it falls back to
        // running every lens in parallel without `cache_control` —
        // we lose caching but the fan-out survives. This function
        // just stages the shared-prefix bytes; all dispatch logic
        // lives downstream.
        let slow_variants = self.effective_slow_variants();
        let redacted_prev = kres_core::redact_findings_for_agent(&ctx.previous_findings);
        let trimmed_prev = shrink_findings_to_budget(&redacted_prev, 1_000_000);
        let trimmed_symbols = shrink_json_list_to_budget(&symbols, 1_000_000);
        let trimmed_context = shrink_json_list_to_budget(&context, 1_000_000);
        let mut shared_cp = CodePrompt::new(prompt)
            .with_symbols(&trimmed_symbols)
            .with_context(&trimmed_context)
            .with_previous_findings(&trimmed_prev);
        if let Some(sk) = &live_skills {
            shared_cp = shared_cp.with_skills(sk);
        }
        if let Some(ref p) = ctx.plan {
            shared_cp = shared_cp.with_plan(p);
        }
        let (shared_prefix, _) = shared_cp.to_cached_split_json(LENS_SHARED_CACHE_FIELDS)?;

        Ok(PreparedLensFanout {
            prompt: prompt.to_string(),
            shared_prefix,
            slow_variants,
            trimmed_symbols,
            trimmed_context,
            trimmed_prev,
            live_skills,
            fast_rounds,
        })
    }

    async fn run_prepared_lens_fanout(
        &self,
        prepared: &PreparedLensFanout,
        call: LensFanoutCall<'_>,
        ctx: &RunContext,
        shutdown: &Shutdown,
    ) -> Result<LensFanoutOutput, AgentError> {
        let LensFanoutCall {
            lenses,
            all_lenses,
            extra_lens_instruction,
            cache_mode,
        } = call;
        // Build per-lens specs once; the same specs feed every slow
        // variant and may be reused across the cache-prime attempt
        // and the no-cache fallback.
        let mut lens_specs: Vec<LensCallSpec> = Vec::with_capacity(lenses.len());
        for lens in lenses.iter() {
            // §20b: send identity-only lens descriptors to the slow
            // agent.
            let parallel_lenses = json!({
                "your_lens": lens_identity(lens),
                "other_lenses": all_lenses
                    .iter()
                    .filter(|candidate| candidate.id != lens.id)
                    .map(lens_identity)
                    .collect::<Vec<_>>(),
            });
            // §20a: in-prose "Apply this lens" imperative so the slow
            // agent doesn't have to infer the lens angle from the
            // parallel_lenses JSON alone.
            let lens_prompt_line = format!("[{}] {}", lens.kind, lens.name);
            let mut lens_extra = String::new();
            if let Some(extra) = extra_lens_instruction {
                lens_extra.push_str(extra.trim_end());
                lens_extra.push_str("\n\n");
            }
            lens_extra.push_str(&format!(
                "Apply this lens to your analysis:\n{lens_prompt_line}"
            ));
            if !lens.reason.is_empty() {
                lens_extra.push_str(&format!("\n(why: {})", lens.reason));
            }
            let mut lens_cp = CodePrompt::new(&prepared.prompt)
                .with_symbols(&prepared.trimmed_symbols)
                .with_context(&prepared.trimmed_context)
                .with_previous_findings(&prepared.trimmed_prev)
                .with_parallel_lenses(&parallel_lenses)
                .with_lens_instruction(&lens_extra);
            // §cache: include skills in the lens prompt — same
            // rationale as the single slow call above. Use the
            // post-gather `live_skills` so any skill files the fast
            // agent pulled in mid-gather reach the lens slow agents
            // too.
            if let Some(sk) = &prepared.live_skills {
                lens_cp = lens_cp.with_skills(sk);
            }
            if let Some(ref p) = ctx.plan {
                lens_cp = lens_cp.with_plan(p);
            }
            let lens_suffix = lens_cp.to_cached_tail_json(LENS_SHARED_CACHE_FIELDS)?;
            lens_specs.push(LensCallSpec {
                lens_id: lens.id.clone(),
                lens_value: lens_identity(lens),
                lens_name: lens.name.clone(),
                lens_suffix,
            });
        }
        // Each slow variant has its own prompt cache (cache key
        // includes model + system + prefix bytes), so each variant
        // runs its own sequence concurrently. PrimeThenParallel
        // serializes one cache-priming call per variant and falls
        // back to no-cache parallel on seed failure; Parallel skips
        // priming entirely.
        let variant_runs = prepared.slow_variants.iter().cloned().map(|variant| {
            let lens_specs = lens_specs.clone();
            let shared_prefix = prepared.shared_prefix.clone();
            let task_brief = ctx.task_brief.clone();
            let shutdown = shutdown.clone();
            let usage = self.usage.clone();
            let logger = self.logger.clone();
            async move {
                let make_future = |spec: LensCallSpec, use_cache: bool| {
                    let ctx = LensCallContext {
                        shared_prefix: shared_prefix.clone(),
                        task_brief: task_brief.clone(),
                        shutdown: shutdown.clone(),
                        usage: usage.clone(),
                        logger: logger.clone(),
                    };
                    build_lens_call_future(spec, variant.clone(), ctx, use_cache)
                };
                match cache_mode {
                    CacheMode::PrimeThenParallel => {
                        let Some((first_spec, rest_specs)) = lens_specs.split_first() else {
                            return Vec::<RawLensResult>::new();
                        };
                        let seed = make_future(first_spec.clone(), true).await;
                        match &seed {
                            Ok(_) => {
                                let rest: Vec<_> = rest_specs
                                    .iter()
                                    .map(|s| make_future(s.clone(), true))
                                    .collect();
                                let mut raws = Vec::with_capacity(1 + rest.len());
                                raws.push(seed);
                                raws.extend(join_all(rest).await);
                                raws
                            }
                            Err(_) => {
                                // Seed failed: give up on caching and
                                // run every lens (including the one
                                // that just failed) in parallel with
                                // no `cache_control`. A single
                                // transient seed failure must not
                                // stall or shrink the fan-out.
                                //
                                // Exception: if shutdown is already
                                // cancelled, every fallback future
                                // would just re-error with
                                // "cancelled" — return the seed
                                // result alone instead of spamming N
                                // duplicate cancellation entries.
                                if shutdown.is_cancelled() {
                                    return vec![seed];
                                }
                                let fallback: Vec<_> = lens_specs
                                    .iter()
                                    .map(|s| make_future(s.clone(), false))
                                    .collect();
                                join_all(fallback).await
                            }
                        }
                    }
                    CacheMode::Parallel => {
                        let futs: Vec<_> = lens_specs
                            .iter()
                            .map(|s| make_future(s.clone(), false))
                            .collect();
                        join_all(futs).await
                    }
                }
            }
        });
        let raws: Vec<RawLensResult> = join_all(variant_runs).await.into_iter().flatten().collect();

        let mut outputs = Vec::new();
        let mut failures = Vec::new();
        for raw in raws.into_iter() {
            match raw {
                Ok((lens_id, lens, model_label, raw_response, parsed)) => {
                    outputs.push(LensRunOutput {
                        lens_id,
                        lens,
                        slow_model: Some(model_label),
                        raw_response,
                        parsed,
                    });
                }
                Err((lens_id, model_label, error)) => {
                    failures.push(LensRunFailure {
                        lens_id,
                        slow_model: Some(model_label),
                        error,
                    });
                }
            }
        }
        Ok(LensFanoutOutput {
            outputs,
            failures,
            fast_rounds: prepared.fast_rounds,
            attempted: lenses.len() * prepared.slow_variants.len(),
            slow_variant_count: prepared.slow_variants.len(),
        })
    }

    fn effective_slow_variants(&self) -> Vec<SlowAgentVariant> {
        if self.slow_variants.is_empty() {
            vec![SlowAgentVariant {
                client: self.slow_client.clone(),
                model: self.slow_model.clone(),
                system: self.slow_system.clone(),
                max_tokens: self.slow_max_tokens,
                max_input_tokens: self.slow_max_input_tokens,
                thinking: self.slow_thinking,
                label: self.slow_model.id.clone(),
            }]
        } else {
            self.slow_variants.clone()
        }
    }

    /// Helper that runs the fast→main loop and returns accumulated
    /// (symbols, context, rounds_used). Shared between run_once and
    /// run_with_lenses.
    pub async fn gather(
        &self,
        prompt: &str,
        plan: Option<&kres_core::Plan>,
        shutdown: &Shutdown,
    ) -> Result<(Vec<Value>, Vec<Value>, u8, Option<Value>), AgentError> {
        let mut symbols: Vec<Value> = Vec::new();
        let mut context: Vec<Value> = Vec::new();
        let mut prev_n_syms: usize = 0;
        let mut prev_n_ctx: usize = 0;
        let mut fast_rounds: u8 = 0;
        let mut fetched_keys: HashSet<String> = HashSet::new();
        // §27 parity: honour mid-loop `skill_reads` in the lens
        // gather path just like `run_once_with_ctx` does. Without
        // this, a skill file the fast agent requests mid-gather
        // never lands in the lens slow-agent payload.
        let mut live_skills: Option<Value> = self.skills.clone();
        for round in 0..self.max_fast_rounds {
            if shutdown.is_cancelled() {
                return Err(AgentError::Other(format!(
                    "shutdown cancelled during fast round {round}"
                )));
            }
            fast_rounds = round + 1;
            let new_syms = if symbols.len() > prev_n_syms {
                &symbols[prev_n_syms..]
            } else {
                &[]
            };
            let new_ctx = if context.len() > prev_n_ctx {
                &context[prev_n_ctx..]
            } else {
                &[]
            };
            let pf_manifest = if prev_n_syms > 0 || prev_n_ctx > 0 {
                Some(crate::symbol::previously_fetched_manifest(
                    &symbols[..prev_n_syms],
                    &context[..prev_n_ctx],
                ))
            } else {
                None
            };
            let mut cp = CodePrompt::new(prompt)
                .with_symbols(new_syms)
                .with_context(new_ctx);
            if let Some(ref pf) = pf_manifest {
                cp = cp.with_previously_fetched(pf);
            }
            if let Some(sk) = &live_skills {
                cp = cp.with_skills(sk);
            }
            if let Some(p) = plan {
                cp = cp.with_plan(p);
            }
            let (gp_prefix, gp_suffix) = cp.to_cached_split_json(CACHED_PREFIX_FIELDS)?;
            prev_n_syms = symbols.len();
            prev_n_ctx = context.len();
            let logged_content = format!("{gp_prefix}{gp_suffix}");
            let messages = vec![Message {
                role: "user".into(),
                content: gp_suffix,
                cache: true,
                cached_prefix: if gp_prefix.is_empty() {
                    None
                } else {
                    Some(gp_prefix)
                },
            }];
            let mut cfg = CallConfig::defaults_for(self.fast_model.clone())
                .with_max_tokens(self.fast_max_tokens)
                .with_stream_label("fast (lens gather)");
            if let Some(thinking) = self.fast_thinking {
                cfg = cfg.with_thinking(thinking);
            }
            if let Some(s) = &self.fast_system {
                cfg = cfg.with_system(s.clone());
            }
            if let Some(n) = self.fast_max_input_tokens {
                cfg = cfg.with_max_input_tokens(n);
            }
            if let Some(lg) = &self.logger {
                let label = format!("phase=fast-gather-lenses round={fast_rounds}");
                lg.log_code_labeled("user", Some(&label), &logged_content, None, None);
            }
            let text = tokio::select! {
                _ = shutdown.cancelled() => return Err(AgentError::Other("cancelled during fast call".into())),
                r = self.fast_client.messages_streaming(&cfg, &messages) => {
                    let resp = r.map_err(|e| AgentError::Other(e.to_string()))?;
                    record_usage(&self.usage, "fast", &self.fast_model, &resp.usage);
                    let t = extract_text(&resp);
                    if let Some(lg) = &self.logger {
                        let th = extract_thinking(&resp);
                        let label = format!("phase=fast-gather-lenses round={fast_rounds}");
                        lg.log_code_labeled(
                            "assistant",
                            Some(&label),
                            &t,
                            Some(log_usage(&resp.usage)),
                            th.as_deref(),
                        );
                    }
                    t
                }
            };
            let parsed = parse_code_response(&text);
            if !parsed.skill_reads.is_empty() {
                apply_skill_reads(&mut live_skills, &parsed.skill_reads);
            }
            let only_skill_reads = parsed.followups.is_empty()
                && !parsed.ready_for_slow
                && !parsed.skill_reads.is_empty();
            if parsed.ready_for_slow {
                break;
            }
            if parsed.followups.is_empty() && !only_skill_reads {
                break;
            }
            if only_skill_reads {
                continue;
            }
            // If every followup is a type:question (a clarification
            // asked of the operator), the fetcher can't produce data
            // for any of them — spinning another main-agent round
            // just burns tokens while the fast agent re-asks. Break
            // and let the slow/lens path surface the questions.
            if !parsed.followups.is_empty() && parsed.followups.iter().all(|f| f.kind == "question")
            {
                kres_core::async_eprintln!(
                    "[fast gather round {}] only type:question followups — breaking",
                    fast_rounds
                );
                break;
            }
            let novel: Vec<_> = parsed
                .followups
                .iter()
                .filter(|fu| !fetched_keys.contains(&fu.cache_key()))
                .cloned()
                .collect();
            let n_dupes = parsed.followups.len() - novel.len();
            if n_dupes > 0 && novel.is_empty() {
                kres_core::async_eprintln!(
                    "[fast gather round {}] all {} followup(s) are re-requests — breaking",
                    fast_rounds,
                    parsed.followups.len(),
                );
                break;
            }
            if n_dupes > 0 {
                kres_core::async_eprintln!(
                    "[fast gather round {}] deduped {} re-request(s), {} novel remain",
                    fast_rounds,
                    n_dupes,
                    novel.len(),
                );
            }
            for fu in &novel {
                fetched_keys.insert(fu.cache_key());
            }
            let fetched = tokio::select! {
                _ = shutdown.cancelled() => return Err(AgentError::Other("cancelled during fetch".into())),
                f = self.fetcher.fetch(&novel, plan) => f?,
            };
            symbols.extend(fetched.symbols);
            context.extend(fetched.context);
        }
        Ok((symbols, context, fast_rounds, live_skills))
    }
}

/// Config bundle for the cross-lens consolidator. Holds a Client +
/// model + optional system prompt so the pipeline caller can
/// construct it once and reuse across tasks.
#[derive(Clone)]
pub struct ConsolidatorClient {
    pub client: Arc<Client>,
    pub model: Model,
    pub system: Option<String>,
    pub max_tokens: u32,
    pub max_input_tokens: Option<u32>,
}

/// Graft a list of newly-requested skill files into the per-run
/// skills JSON. Matches `skill_reads` handling at
///reads happen against the fast-agent's
/// filesystem, and the file contents land in the FIRST skill's
/// `files` map (most skills are singletons in practice).
pub fn apply_skill_reads(skills: &mut Option<Value>, reads: &[String]) {
    if reads.is_empty() {
        return;
    }
    // Ensure there's a skills object to mutate.
    let obj = skills.get_or_insert_with(|| Value::Object(serde_json::Map::new()));
    let map = match obj.as_object_mut() {
        Some(m) => m,
        None => return,
    };
    if map.is_empty() {
        // Create a synthetic "runtime" skill so the loop has
        // somewhere to land the files.
        map.insert("runtime".into(), json!({"content": "", "files": {}}));
    }
    // Mutate the first skill's files map (BTreeMap iteration is
    // stable, matching 's "update skills[0].files").
    let first_key = map.keys().next().cloned().unwrap();
    let first = map.get_mut(&first_key).unwrap();
    if !first.is_object() {
        *first = json!({"content": "", "files": {}});
    }
    let first_obj = first.as_object_mut().unwrap();
    let files_entry = first_obj
        .entry("files".to_string())
        .or_insert_with(|| json!({}));
    let files_map = match files_entry.as_object_mut() {
        Some(m) => m,
        None => {
            *files_entry = json!({});
            files_entry.as_object_mut().unwrap()
        }
    };
    for path in reads {
        if path.is_empty() {
            continue;
        }
        match std::fs::read_to_string(path) {
            Ok(content) => {
                files_map.insert(path.clone(), Value::String(content));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                match resolve_skill_read_in_subdirs(path) {
                    Some((resolved, content)) => {
                        tracing::info!(
                            target: "kres_agents",
                            asked = %path,
                            resolved = %resolved.display(),
                            "skill_read resolved via subdir lookup"
                        );
                        files_map.insert(path.clone(), Value::String(content));
                    }
                    None => {
                        tracing::warn!(
                            target: "kres_agents",
                            path,
                            "skill_read failed: {e}"
                        );
                        files_map.insert(
                            path.clone(),
                            Value::String(format!("[skill_read failed: {e}]")),
                        );
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    target: "kres_agents",
                    path,
                    "skill_read failed: {e}"
                );
                files_map.insert(
                    path.clone(),
                    Value::String(format!("[skill_read failed: {e}]")),
                );
            }
        }
    }
}

/// When the agent emits a skill_read for a path that doesn't exist,
/// look in immediate subdirectories of the requested file's parent
/// for a basename match. Skill libraries (review-prompts/kernel/)
/// commonly nest by topic — for example, `subsystem/vfs.md` lives a
/// directory deeper than `technical-patterns.md`. The agent has all
/// the path information in its prompt but routinely drops the
/// nesting segment when composing absolute paths. Rather than rely
/// on prompt wording the LLM has already proven it ignores, fall
/// back to a single-level subdirectory search.
///
/// Strict resolution rules:
/// - search only one level of subdirectories (no recursion) so a
///   stray match deep in `examples/` doesn't substitute for an
///   intentional read
/// - require EXACTLY ONE candidate; multiple matches are
///   ambiguous and we'd rather fail loudly than guess
///
/// Returns the resolved path + file content on a hit, None
/// otherwise.
fn resolve_skill_read_in_subdirs(path: &str) -> Option<(std::path::PathBuf, String)> {
    let p = std::path::Path::new(path);
    let parent = p.parent()?;
    let basename = p.file_name()?;
    let entries = std::fs::read_dir(parent).ok()?;
    let mut matches: Vec<std::path::PathBuf> = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let candidate = entry.path().join(basename);
        if candidate.is_file() {
            matches.push(candidate);
        }
    }
    if matches.len() != 1 {
        return None;
    }
    let resolved = matches.into_iter().next().unwrap();
    let content = std::fs::read_to_string(&resolved).ok()?;
    Some((resolved, content))
}

/// Strip a lens to `{type, name, id?, reason?}` for the
/// `parallel_lenses` blob. Matches
/// — we expose just enough for the slow agent to
/// discriminate "your lens" from sibling lenses without bleeding any
/// internal LensSpec fields into the prompt.
pub fn lens_identity(lens: &LensSpec) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("type".into(), json!(lens.kind));
    obj.insert("name".into(), json!(lens.name));
    if !lens.id.is_empty() {
        obj.insert("id".into(), json!(lens.id));
    }
    if !lens.reason.is_empty() {
        obj.insert("reason".into(), json!(lens.reason));
    }
    Value::Object(obj)
}

/// Prepend the original user prompt to a derived task prompt so
/// fast/slow agents see the top-level context alongside the current
/// task brief. Matches 648-650, 3232. When the two
/// strings are equal (top-level task) or `original_prompt` is empty,
/// returns `prompt` unchanged.
pub fn prepend_original_prompt(prompt: &str, original_prompt: &str) -> String {
    if original_prompt.is_empty() || original_prompt == prompt {
        return prompt.to_string();
    }
    format!(
        "Original user prompt: {}\nCurrent task: {}",
        original_prompt, prompt
    )
}

fn merge_followups(dst: &mut Vec<Followup>, src: Vec<Followup>) {
    let mut seen: HashSet<String> = dst.iter().map(Followup::cache_key).collect();
    for fu in src {
        if seen.insert(fu.cache_key()) {
            dst.push(fu);
        }
    }
}

fn validate_generic_lens_output(output: &LensRunOutput) -> Result<(), String> {
    if output.parsed.analysis.trim().is_empty()
        && output.parsed.findings.is_empty()
        && output.parsed.followups.is_empty()
    {
        let model = output.slow_model.as_deref().unwrap_or("unknown-model");
        Err(format!(
            "{} ({model}): no analysis, findings, or followups",
            output.lens_id
        ))
    } else {
        Ok(())
    }
}

/// Cut a string to `n` chars with an ellipsis. Used by the verbose
/// AgentRunner printouts so a long followup name doesn't flood the
/// REPL line.
fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let head: String = s.chars().take(n).collect();
    format!("{head}…")
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn extract_text(resp: &kres_llm::request::MessagesResponse) -> String {
    let mut out = String::new();
    for block in &resp.content {
        if let kres_llm::request::ContentBlock::Text { text } = block {
            out.push_str(text);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::followup::Followup;

    #[tokio::test]
    async fn null_fetcher_returns_empty() {
        let f = NullFetcher;
        let r = f
            .fetch(
                &[Followup {
                    kind: "source".into(),
                    name: "x".into(),
                    reason: String::new(),
                    path: None,
                    nice_to_have: false,
                }],
                None,
            )
            .await
            .unwrap();
        assert!(r.symbols.is_empty());
        assert!(r.context.is_empty());
    }

    /// Unit-test the loop-back decision table. We can't easily run
    /// the whole AgentRunner without a live API, but we can test
    /// the key condition that caused the Phase-1 "did nothing" bug.
    #[test]
    fn parse_only_skill_reads_triggers_loopback() {
        // The fast agent emits a response with nothing but
        // skill_reads. This should NOT be treated as "ready for slow".
        let r = parse_code_response(
            r#"{"analysis": "I need to load the kernel skill",
                "followups": [],
                "skill_reads": ["/kernel.md"],
                "ready_for_slow": false}"#,
        );
        assert!(r.followups.is_empty());
        assert!(!r.ready_for_slow);
        assert!(!r.skill_reads.is_empty());
        // AgentRunner's decision: only_skill_reads = true, so loop back.
        let only_skill_reads =
            r.followups.is_empty() && !r.ready_for_slow && !r.skill_reads.is_empty();
        assert!(only_skill_reads);
    }

    #[test]
    fn parse_empty_triggers_slow_handoff() {
        // No followups, no skill_reads, not ready — the AgentRunner
        // should break out and still run the slow agent.
        let r = parse_code_response(r#"{"analysis": "no work needed"}"#);
        let only_skill_reads =
            r.followups.is_empty() && !r.ready_for_slow && !r.skill_reads.is_empty();
        assert!(!only_skill_reads);
        assert!(r.followups.is_empty());
    }

    #[test]
    fn parse_ready_for_slow_short_circuits() {
        let r = parse_code_response(
            r#"{"analysis": "ready", "followups": [], "ready_for_slow": true}"#,
        );
        assert!(r.ready_for_slow);
    }

    /// Mirrors the new early-exit rule in `run_once_with_ctx` and
    /// `gather`: if every followup has kind=="question", the fetcher
    /// can't produce data for any of them, so the AgentRunner
    /// breaks out instead of spinning another round.
    #[test]
    fn question_only_followups_trip_early_exit() {
        let r = parse_code_response(
            r#"{"analysis": "need a target",
                "followups": [
                    {"type": "question", "name": "which file?"},
                    {"type": "question", "name": "which function?"}
                ],
                "ready_for_slow": false}"#,
        );
        assert!(!r.followups.is_empty());
        assert!(r.followups.iter().all(|f| f.kind == "question"));
    }

    #[test]
    fn mixed_followups_do_not_trip_early_exit() {
        let r = parse_code_response(
            r#"{"analysis": "need a target",
                "followups": [
                    {"type": "question", "name": "which file?"},
                    {"type": "source", "name": "foo"}
                ],
                "ready_for_slow": false}"#,
        );
        assert!(!r.followups.iter().all(|f| f.kind == "question"));
    }

    /// `apply_skill_reads` must graft the requested file into the
    /// first skill's `files` map so a subsequent gather round (and
    /// the lens slow agents that read `live_skills`) see it.
    #[test]
    fn apply_skill_reads_inserts_file_into_first_skill() {
        let dir =
            std::env::temp_dir().join(format!("kres-apply-skill-reads-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("skill.md");
        std::fs::write(&p, "hello skill body").unwrap();
        let mut skills = Some(json!({
            "kernel": {"content": "guide", "files": {}}
        }));
        apply_skill_reads(&mut skills, &[p.to_string_lossy().to_string()]);
        let files = skills
            .as_ref()
            .and_then(|v| v.get("kernel"))
            .and_then(|k| k.get("files"))
            .and_then(|f| f.as_object())
            .expect("files map");
        let body = files
            .get(p.to_str().unwrap())
            .and_then(|v| v.as_str())
            .expect("file body");
        assert_eq!(body, "hello skill body");
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_dir(&dir);
    }

    /// Regression for the FIX-flow vfs.md miss: agent emitted
    /// kernel/vfs.md but the file is actually at
    /// kernel/subsystem/vfs.md. The reaper used to surface the
    /// raw NotFound back to the agent and the slow agent would
    /// proceed without the subsystem guide. Now the subdir
    /// fallback picks it up when there's exactly one match.
    #[test]
    fn apply_skill_reads_falls_back_to_one_subdir_match() {
        let root = std::env::temp_dir().join(format!("kres-skill-subdir-{}", std::process::id()));
        let sub = root.join("subsystem");
        std::fs::create_dir_all(&sub).unwrap();
        let actual = sub.join("vfs.md");
        std::fs::write(&actual, "VFS body").unwrap();
        let asked = root.join("vfs.md"); // does NOT exist at this level
        let mut skills = Some(json!({"kernel": {"content": "", "files": {}}}));
        apply_skill_reads(&mut skills, &[asked.to_string_lossy().to_string()]);
        let body = skills
            .as_ref()
            .and_then(|v| v.get("kernel"))
            .and_then(|k| k.get("files"))
            .and_then(|f| f.get(asked.to_str().unwrap()))
            .and_then(|v| v.as_str())
            .expect("file body");
        assert_eq!(body, "VFS body");
        let _ = std::fs::remove_file(&actual);
        let _ = std::fs::remove_dir(&sub);
        let _ = std::fs::remove_dir(&root);
    }

    /// Two subdir matches → ambiguous, do NOT guess. Surface the
    /// original NotFound so the agent / operator can resolve.
    #[test]
    fn apply_skill_reads_subdir_fallback_refuses_ambiguity() {
        let root =
            std::env::temp_dir().join(format!("kres-skill-subdir-ambig-{}", std::process::id()));
        let a = root.join("a");
        let b = root.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("dup.md"), "from a").unwrap();
        std::fs::write(b.join("dup.md"), "from b").unwrap();
        let asked = root.join("dup.md");
        let mut skills = Some(json!({"kernel": {"content": "", "files": {}}}));
        apply_skill_reads(&mut skills, &[asked.to_string_lossy().to_string()]);
        let body = skills
            .as_ref()
            .and_then(|v| v.get("kernel"))
            .and_then(|k| k.get("files"))
            .and_then(|f| f.get(asked.to_str().unwrap()))
            .and_then(|v| v.as_str())
            .expect("file slot");
        assert!(
            body.starts_with("[skill_read failed:"),
            "ambiguous fallback should surface original NotFound, got: {body:?}"
        );
        let _ = std::fs::remove_file(a.join("dup.md"));
        let _ = std::fs::remove_file(b.join("dup.md"));
        let _ = std::fs::remove_dir(&a);
        let _ = std::fs::remove_dir(&b);
        let _ = std::fs::remove_dir(&root);
    }
}
