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
use kres_core::shutdown::Shutdown;
use kres_llm::{
    client::Client,
    config::CallConfig,
    model::{Effort, ThinkingBudget},
    request::{mark_last_n_user_cached, Message},
    Model,
};

use crate::{
    consolidate::{consolidate_lenses_with_logger, LensOutput},
    error::AgentError,
    followup::Followup,
    json_repair::{repair_json_response, JsonContract, JsonRepairCall, RepairLogKind},
    prompt::CodePrompt,
    response::{
        diagnose_code_response, log_json_normalization, CodeResponse, CodeResponseContract,
        ParseStrategy,
    },
};

const GENERIC_LENS_REPAIR_RETRIES: usize = 1;
const LOW_EFFORT_EXPLICIT_THINKING_TOKENS: u32 = 1_024;
const JSON_ONLY_OUTPUT_INSTRUCTION: &str = "Return exactly one raw JSON object. Do not return Markdown: no Markdown headings, prose preamble, code fences, backticks, or trailing commentary.";
const FAST_GATHER_KINDS: &[&str] = &[
    "survey", "source", "type", "callers", "callees", "search", "grep", "read", "file", "find",
    "git", "make", "meson", "cargo", "bash", "lore", "question",
];

fn lower_thinking_effort(thinking: ThinkingBudget, max_tokens: u32) -> ThinkingBudget {
    match thinking {
        ThinkingBudget::Disabled => ThinkingBudget::Disabled,
        ThinkingBudget::ExplicitBudget(_) => {
            ThinkingBudget::enabled_clamped(LOW_EFFORT_EXPLICIT_THINKING_TOKENS, max_tokens)
        }
        ThinkingBudget::Adaptive(_) => ThinkingBudget::Adaptive(Effort::Low),
    }
}

fn fast_gather_semantic_errors(response: &CodeResponse) -> Vec<String> {
    let mut errors = Vec::new();
    for (index, followup) in response.followups.iter().enumerate() {
        let kind = followup.kind.trim();
        if !FAST_GATHER_KINDS.contains(&kind) {
            errors.push(format!(
                "followups[{index}].type `{kind}` is unsupported; allowed: {}",
                FAST_GATHER_KINDS.join(", ")
            ));
        }
        if followup.name.trim().is_empty() {
            errors.push(format!("followups[{index}].name must not be empty"));
        }
        if followup.reason.trim().is_empty() {
            errors.push(format!("followups[{index}].reason must not be empty"));
        }
        if followup
            .path
            .as_ref()
            .is_some_and(|path| path.trim().is_empty())
        {
            errors.push(format!(
                "followups[{index}].path must not be empty when present"
            ));
        }
    }
    for (index, path) in response.skill_reads.iter().enumerate() {
        if path.trim().is_empty() {
            errors.push(format!("skill_reads[{index}] must not be empty"));
        }
    }
    errors
}

fn append_json_only_output_instruction(prompt: &str) -> String {
    format!("{}\n\n{}", prompt.trim_end(), JSON_ONLY_OUTPUT_INSTRUCTION)
}

fn validate_fast_gather_text(text: &str) -> Result<CodeResponse, Vec<String>> {
    CodeResponseContract::default().validate_with(text, fast_gather_semantic_errors)
}

fn validate_fast_gather_text_for_run(
    text: &str,
    disable_skill_reads: bool,
    allowed_gather_kinds: Option<&BTreeSet<String>>,
) -> Result<CodeResponse, Vec<String>> {
    let response = validate_fast_gather_text(text)?;
    let errors =
        fast_gather_run_policy_errors(&response, disable_skill_reads, allowed_gather_kinds);
    if errors.is_empty() {
        Ok(response)
    } else {
        Err(errors)
    }
}

fn fast_gather_run_policy_errors(
    response: &CodeResponse,
    disable_skill_reads: bool,
    allowed_gather_kinds: Option<&BTreeSet<String>>,
) -> Vec<String> {
    let mut errors = Vec::new();
    if disable_skill_reads && !response.skill_reads.is_empty() {
        errors.push(
            "skill_reads are disabled for this run; return an empty skill_reads array".into(),
        );
    }
    if let Some(allowed) = allowed_gather_kinds {
        for (index, followup) in response.followups.iter().enumerate() {
            if !allowed.contains(&followup.kind) {
                errors.push(format!(
                    "followups[{index}].type `{}` is disabled for this run; allowed: {}",
                    followup.kind,
                    allowed.iter().cloned().collect::<Vec<_>>().join(", ")
                ));
            }
        }
    }
    errors
}

#[cfg(test)]
fn validate_fast_gather_response(response: &CodeResponse) -> Result<(), Vec<String>> {
    let mut errors = response.validation_errors.clone();
    errors.extend(
        response
            .unknown_fields
            .keys()
            .map(|field| format!("unknown top-level field `{field}`")),
    );
    if response.strategy == ParseStrategy::RawText {
        errors.push("response must be one JSON object, not prose".to_string());
    }
    errors.extend(fast_gather_semantic_errors(response));
    errors.sort();
    errors.dedup();
    errors.is_empty().then_some(()).ok_or(errors)
}

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
/// Stable task scope is cached independently of the evidence delta. Gather is
/// a multi-turn conversation: each source/context record is appended once,
/// and the two newest user cache breakpoints let the next round reuse the
/// prior task/evidence prefix without rebroadcasting it in a new message.
/// `plan_rewrite_allowed` deliberately remains volatile because it is present
/// only on selected synthesis calls.
// Gather sends one `skills` payload; the common/task split happens only at
// synthesis, so `common_skills` deliberately does not appear here.
const CACHED_PREFIX_FIELDS: &[&str] = &["question", "previous_findings", "skills", "plan"];
const LENS_SHARED_CACHE_FIELDS: &[&str] = &[
    "question",
    "symbols",
    "context",
    "previous_findings",
    "common_skills",
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
    pub allowed_response_extensions: BTreeSet<String>,
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
    pub over_input_limit: Option<(u64, u64)>,
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
    pub contract_name: &'a str,
    pub schema: &'a str,
}

struct PreparedLensFanout {
    prompt: String,
    shared_prefix: String,
    slow_variants: Vec<SlowAgentVariant>,
    symbols: Vec<Value>,
    context: Vec<Value>,
    previous_findings: Vec<Finding>,
    common_skills: Option<Value>,
    task_skills: Option<Value>,
    fast_rounds: u8,
}

type RawLensSuccess = (String, Value, String, String, CodeResponse);
type RawLensFailure = (String, String, String, Option<(u64, u64)>);
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
    run_keys: Option<&'a BTreeSet<(String, Option<String>)>>,
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
    /// Route this variant only to the workflow's supplemental review lens.
    pub supplemental_lens_only: bool,
}

fn supplemental_lens_id(lenses: &[LensSpec]) -> Option<&str> {
    ["maintainer", "general"]
        .into_iter()
        .find(|id| lenses.iter().any(|lens| lens.id == *id))
}

fn variant_runs_lens(
    supplemental_lens_only: bool,
    lens_id: &str,
    supplemental_lens: Option<&str>,
) -> bool {
    !supplemental_lens_only || supplemental_lens == Some(lens_id)
}

/// Inputs to one run that vary per-task. Separated from AgentRunner
/// so a single AgentRunner can run many tasks in parallel, each with
/// its own previous_findings / task_brief / original_prompt.
#[derive(Debug, Clone, Default)]
pub struct RunContext {
    /// Workflow-declared top-level response fields. The shared envelope
    /// rejects every other extension, so misspellings cannot be consumed as
    /// successful output while workflows can still define typed results.
    pub allowed_response_extensions: BTreeSet<String>,
    /// Findings from prior turns; attached to the slow-agent prompt
    /// via CodePrompt::with_previous_findings.
    pub previous_findings: Vec<Finding>,
    /// Plan step executed by this task. Included in the compact plan
    /// projection so agents do not have to infer the active row from prose.
    pub active_plan_step_id: Option<String>,
    /// Complete task scope for downstream inference calls such as the lens
    /// consolidator and prose-finding promoter. Log renderers may abbreviate
    /// it for display, but request construction must not.
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
    /// Reject and repair fast-gather responses that request skill files.
    /// Workflow steps set this when their workflow declares `skills: []`.
    /// Generic tasks retain on-demand skill reads through the default `false`.
    pub disable_skill_reads: bool,
    /// Optional per-run allowlist for fast-gather followup kinds.
    /// The whole-file review scan uses this to permit only `survey`.
    pub allowed_gather_kinds: Option<BTreeSet<String>>,
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
    /// Symbols already gathered by an earlier workflow step that this
    /// run should start from instead of re-fetching. The gather loop
    /// seeds its `symbols` accumulator with these so round 0 ships
    /// them to the agent as already-available context. Empty for the
    /// first step in a chain. See the workflow runner's per-step
    /// gathered-context cache.
    pub seed_symbols: Vec<Value>,
    /// Context items already gathered by an earlier workflow step,
    /// seeded the same way as [`Self::seed_symbols`].
    pub seed_context: Vec<Value>,
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
        supplemental_lens_only: _,
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
            _ = shutdown.cancelled() => Err((lens_id, model_label, "cancelled".to_string(), None)),
            r = client.messages_streaming(&cfg, &messages) => match r {
                Ok(resp) => {
                    record_usage(&usage, "slow", &model, &resp.usage);
                    let t = extract_text(&resp);
                    if let Some(lg) = &logger {
                        let th = extract_thinking(&resp);
                        lg.log_code_labeled_with_model(
                            "assistant",
                            Some(&log_label),
                            &t,
                            Some(log_usage(&resp.usage)),
                            th.as_deref(),
                            resp.model.as_deref(),
                        );
                    }
                    let parsed = diagnose_code_response(&t);
                    Ok((lens_id, lens_value, model_label, t, parsed))
                }
                Err(e) => {
                    tracing::warn!(target: "kres_agents", "lens call failed: {e}");
                    let over_input_limit = match &e {
                        kres_llm::LlmError::OverInputLimit { actual, limit } => {
                            Some((*actual, *limit))
                        }
                        _ => None,
                    };
                    Err((lens_id, model_label, e.to_string(), over_input_limit))
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
    /// All symbols this run accumulated across its fast-gather rounds
    /// (including any seeded from an earlier step). The workflow
    /// runner caches these per step id so a dependent step can seed
    /// its own gather via [`RunContext::seed_symbols`] and avoid
    /// re-fetching the same source. Empty for runs that did no gather
    /// (for example, a direct one-shot response).
    pub gathered_symbols: Vec<Value>,
    /// Context items this run accumulated, cached the same way as
    /// [`Self::gathered_symbols`].
    pub gathered_context: Vec<Value>,
}

impl AgentRunner {
    async fn repair_fast_gather_response(
        &self,
        invalid: &str,
        errors: &[String],
        label: &str,
        shutdown: &Shutdown,
    ) -> Result<CodeResponse, AgentError> {
        let contract = CodeResponseContract::default();
        let schema = contract
            .schema_json_for(&[
                "analysis",
                "followups",
                "skill_reads",
                "ready_for_slow",
                "plan",
            ])
            .to_string();
        let repaired = repair_json_response(JsonRepairCall {
            client: self.fast_client.clone(),
            model: self.fast_model.clone(),
            max_tokens: self.fast_max_tokens,
            max_input_tokens: self.fast_max_input_tokens,
            thinking: self.fast_thinking,
            contract: JsonContract {
                name: label,
                schema: &schema,
                instructions: "Preserve the intended analysis and requests. Followups require a known non-empty type, name, and reason.",
            },
            rejected_response: invalid,
            validation_errors: errors,
            logger: self.logger.clone(),
            log_kind: RepairLogKind::Code,
            shutdown: Some(shutdown.clone()),
        })
        .await?;
        record_usage(&self.usage, "fast", &self.fast_model, &repaired.usage);
        let parsed = contract
            .accept_repair_with(&repaired.text, fast_gather_semantic_errors)
            .map_err(|errors| {
                AgentError::Other(format!(
                    "fast gather response remained invalid after repair: {}",
                    errors.join("; ")
                ))
            })?;
        Ok(parsed)
    }

    /// Run one raw prompt through the primary slow model without a fast gather
    /// round. Bootstrap analyses use this when Rust has already assembled all
    /// input and must send it intact.
    pub async fn run_primary_slow_inference(
        &self,
        system: &str,
        prompt: &str,
        task_label: &str,
        shutdown: &Shutdown,
    ) -> Result<String, AgentError> {
        self.run_primary_slow_inference_profiled(system, None, prompt, task_label, false, shutdown)
            .await
    }

    /// Run a low-reasoning primary-slow call over `stable_prefix + prompt_tail`.
    /// Change surveys use this for cheap parallel classification while
    /// retaining the configured slow model.
    ///
    /// `cache_prefix` must be true only when several calls share the same
    /// prefix bytes. A cache write is billed above ordinary input, so marking
    /// a prefix that nothing else will read is a straight loss — the observed
    /// case was a single-call change survey paying 144k of cache creation for
    /// zero reads. When false the two halves are concatenated into one plain
    /// message; the model sees identical text either way.
    pub async fn run_primary_slow_inference_low_effort(
        &self,
        system: &str,
        stable_prefix: &str,
        prompt_tail: &str,
        cache_prefix: bool,
        task_label: &str,
        shutdown: &Shutdown,
    ) -> Result<String, AgentError> {
        let joined;
        let (cached_prefix, tail) = if cache_prefix {
            (Some(stable_prefix), prompt_tail)
        } else {
            joined = format!("{stable_prefix}{prompt_tail}");
            (None, joined.as_str())
        };
        self.run_primary_slow_inference_profiled(
            system,
            cached_prefix,
            tail,
            task_label,
            true,
            shutdown,
        )
        .await
    }

    async fn run_primary_slow_inference_profiled(
        &self,
        system: &str,
        cached_prefix: Option<&str>,
        prompt_tail: &str,
        task_label: &str,
        low_effort: bool,
        shutdown: &Shutdown,
    ) -> Result<String, AgentError> {
        let messages = vec![Message {
            role: "user".into(),
            content: prompt_tail.to_string(),
            cache: cached_prefix.is_some(),
            cached_prefix: cached_prefix.map(str::to_string),
        }];
        let mut cfg = CallConfig::defaults_for(self.slow_model.clone())
            .with_max_tokens(self.slow_max_tokens)
            .with_stream_label(format!("slow ({task_label})"))
            .with_system(system.to_string());
        let configured_thinking = self.slow_thinking.unwrap_or(cfg.thinking);
        if low_effort {
            let max_tokens = cfg.max_tokens;
            cfg = cfg
                .with_thinking(lower_thinking_effort(configured_thinking, max_tokens))
                .with_text_verbosity("low");
        } else {
            cfg = cfg.with_thinking(configured_thinking);
        }
        if let Some(limit) = self.slow_max_input_tokens {
            cfg = cfg.with_max_input_tokens(limit);
        }
        if let Some(logger) = &self.logger {
            let label = format!("phase=slow task={task_label}");
            let meta = cfg.request_meta();
            let logged_prompt = cached_prefix
                .map(|prefix| format!("{prefix}{prompt_tail}"))
                .unwrap_or_else(|| prompt_tail.to_string());
            logger.log_code_labeled_with_request(
                "user",
                Some(&label),
                &logged_prompt,
                None,
                None,
                Some(&meta),
            );
        }
        let response = tokio::select! {
            _ = shutdown.cancelled() => {
                return Err(AgentError::Other(format!(
                    "cancelled during slow {task_label} call"
                )));
            }
            response = self.slow_client.messages_streaming(&cfg, &messages) => {
                response.map_err(AgentError::from)?
            }
        };
        record_usage(&self.usage, "slow", &self.slow_model, &response.usage);
        let text = extract_text(&response);
        if let Some(logger) = &self.logger {
            let label = format!("phase=slow task={task_label}");
            let thinking = extract_thinking(&response);
            logger.log_code_labeled_with_model(
                "assistant",
                Some(&label),
                &text,
                Some(log_usage(&response.usage)),
                thinking.as_deref(),
                response.model.as_deref(),
            );
        }
        Ok(text)
    }

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
        let composed = append_json_only_output_instruction(&prepend_original_prompt(
            prompt,
            &ctx.original_prompt,
        ));
        let prompt: &str = composed.as_str();
        let gather_composed;
        let gather_prompt = if let Some(gather) = ctx.gather_prompt.as_deref() {
            gather_composed = append_json_only_output_instruction(&prepend_original_prompt(
                gather,
                &ctx.original_prompt,
            ));
            gather_composed.as_str()
        } else {
            prompt
        };
        let log_task = if ctx.task_brief.is_empty() {
            "task".to_string()
        } else {
            ctx.task_brief.clone()
        };
        let (symbols, context, fast_rounds, live_skills, task_skill_paths) =
            self.gather(gather_prompt, ctx, shutdown).await?;

        // Slow agent call.
        // Redact `details` before ANY budget / shipping step — the
        // per-task narrative stored on Finding.details is for
        // /summary only and must never reach an agent prompt.
        // Canonicalization removes exact duplicate representations, but it is
        // deliberately lossless: inference construction must never discard
        // source, tool output, findings, or prompt text to satisfy a local
        // request-size policy. Provider-specific framing happens below the
        // prompt layer without changing the visible content.
        let (symbols, context) = crate::symbol::canonicalize_prompt_evidence(&symbols, &context);
        let previous_findings = kres_core::redact_findings_for_agent(&ctx.previous_findings);
        // Non-lensed slow calls are one-shot. Do not split off a
        // cached prefix here: there is no parallel fan-out to amortize
        // the cache write, and repeated workflow correction passes
        // should not keep paying to create slow-agent cache entries.
        // Review paths that actually benefit from a shared context
        // prefix go through run_with_lenses below.
        let mut slow_cp = CodePrompt::new(prompt)
            .with_symbols(&symbols)
            .with_context(&context)
            .with_previous_findings(&previous_findings);
        // Split post-gather skills against the runner's stable base. This
        // preserves every selected file while keeping common bytes distinct
        // from per-task additions.
        let synthesis_skills = split_skills_for_synthesis(live_skills.as_ref(), &task_skill_paths);
        if let Some(sk) = &synthesis_skills.common {
            slow_cp = slow_cp.with_common_skills(sk);
        }
        if let Some(sk) = &synthesis_skills.task {
            slow_cp = slow_cp.with_skills(sk);
        }
        if let Some(ref p) = ctx.plan {
            slow_cp = slow_cp.with_plan(p, ctx.active_plan_step_id.as_deref());
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
        // - synthesis_use_routing_prompt (orchestrator step only, independent of client):
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
            symbols.len(),
            context.len(),
            previous_findings.len(),
        );
        let synth_role_for_usage = if use_fast { "fast" } else { "slow" };
        let synthesis = tokio::select! {
            _ = shutdown.cancelled() => {
                return Err(AgentError::Other(format!("cancelled during {log_phase} call")));
            }
            r = synth_client.messages_streaming(&cfg, &messages) => r,
        };
        let mut text = match synthesis {
            Ok(resp) => {
                record_usage(&self.usage, synth_role_for_usage, &synth_model, &resp.usage);
                let t = extract_text(&resp);
                if let Some(lg) = &self.logger {
                    let thinking = extract_thinking(&resp);
                    let label = format!("phase={log_phase} task={log_task}");
                    lg.log_code_labeled_with_model(
                        "assistant",
                        Some(&label),
                        &t,
                        Some(log_usage(&resp.usage)),
                        thinking.as_deref(),
                        resp.model.as_deref(),
                    );
                }
                t
            }
            Err(kres_llm::LlmError::OverInputLimit { actual, limit }) => {
                return Err(AgentError::OverInputLimit { actual, limit });
            }
            Err(other) => return Err(AgentError::Other(other.to_string())),
        };
        let response_contract =
            CodeResponseContract::new(ctx.allowed_response_extensions.iter().cloned());
        let response_schema = response_contract.schema_json().to_string();
        let tolerant_contract = response_contract.clone().allowing_invalid_findings();
        let initial_validation = tolerant_contract.validate(&text);
        let mut slow_parsed = initial_validation
            .as_ref()
            .cloned()
            .unwrap_or_else(|_| diagnose_code_response(&text));
        let envelope_errors = initial_validation.err().unwrap_or_default();
        if !envelope_errors.is_empty() {
            if let Ok(repaired) = repair_json_response(JsonRepairCall {
                client: synth_client.clone(),
                model: synth_model.clone(),
                max_tokens: synth_max_tokens,
                max_input_tokens: synth_max_in,
                thinking: synth_thinking,
                contract: JsonContract {
                    name: "slow-synthesis",
                    schema: &response_schema,
                    instructions: "Preserve analysis, Finding ids, followup requests, output paths, edit paths, and plan step identities. Correct representation and field types only.",
                },
                rejected_response: &text,
                validation_errors: &envelope_errors,
                logger: self.logger.clone(),
                log_kind: RepairLogKind::Code,
                shutdown: Some(shutdown.clone()),
            })
            .await
            {
                record_usage(&self.usage, synth_role_for_usage, &synth_model, &repaired.usage);
                if let Ok(candidate) = tolerant_contract.accept_repair(&repaired.text) {
                    text = repaired.text;
                    slow_parsed = candidate;
                } else {
                    tracing::warn!(target: "kres_agents", "slow synthesis JSON repair failed the strict response contract");
                }
            }
        }
        if !slow_parsed.invalid_findings.is_empty() {
            let outcome = crate::finding_repair::repair_invalid_findings(
                synth_client.clone(),
                synth_model.clone(),
                synth_max_tokens,
                synth_max_in,
                std::mem::take(&mut slow_parsed.invalid_findings),
                &ctx.previous_findings,
                crate::finding_repair::FindingRepairRuntime {
                    logger: self.logger.clone(),
                    thinking: synth_thinking,
                    cancel: Some(crate::finding_repair::FindingRepairCancel::Shutdown(
                        shutdown.clone(),
                    )),
                    usage: self.usage.clone(),
                    role: synth_role_for_usage,
                },
            )
            .await?;
            slow_parsed.merge_repaired_findings(outcome.findings);
            if !outcome.unrepaired.is_empty() {
                return Err(AgentError::Other(
                    crate::finding_repair::format_unrepaired_findings(&outcome.unrepaired),
                ));
            }
            text = replace_response_findings(&text, &slow_parsed.findings)?;
        }
        if let Err(errors) = response_contract.validate(&text) {
            return Err(AgentError::Other(format!(
                "slow response remained invalid after JSON repair: {}",
                errors.join("; ")
            )));
        }
        log_json_normalization(self.logger.as_deref(), &slow_parsed, log_phase);
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
            // Return the same complete canonical evidence set that the slow
            // call received. Dependent steps may deduplicate it by stable
            // evidence identity, but must not inherit a size-trimmed subset.
            gathered_symbols: symbols,
            gathered_context: context,
        })
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
        let lens_schema = CodeResponseContract::default()
            .schema_json_for(&["analysis", "findings", "followups"])
            .to_string();
        let fanout = self
            .run_lenses_shared_gather_repairing(
                prompt,
                lenses,
                ctx,
                shutdown,
                LensRepairPolicy {
                    max_retries: GENERIC_LENS_REPAIR_RETRIES,
                    repair_instruction: "Your previous response for this review lens did not produce usable lens output. Reuse the same gathered source/context and reply with this lens's analysis, findings, or followups.",
                    contract_name: "review-lens",
                    schema: &lens_schema,
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
            Some(shutdown.clone()),
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
            // Lens fan-out shares one gather across all lenses; the
            // per-step cache is only consumed by sequential dependent
            // steps (validate, fix), not lensed review, so an empty
            // hand-back here is correct.
            gathered_symbols: Vec::new(),
            gathered_context: Vec::new(),
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
                    run_keys: None,
                },
                ctx,
                shutdown,
            )
            .await?;

        for retry in 0..repair.max_retries {
            if let Some((actual, limit)) = fanout
                .failures
                .iter()
                .find_map(|failure| failure.over_input_limit)
            {
                return Err(AgentError::OverInputLimit { actual, limit });
            }
            // Collect per-lens errors so we can surface the specific
            // validator/transport message to each retried lens — the
            // generic repair instruction alone does not tell the model
            // why its output was rejected.
            let mut retry_errors: Vec<(String, Option<String>, String)> = Vec::new();
            for failure in &fanout.failures {
                retry_errors.push((
                    failure.lens_id.clone(),
                    failure.slow_model.clone(),
                    failure.error.clone(),
                ));
            }
            for output in &fanout.outputs {
                if let Err(e) = validate(output) {
                    retry_errors.push((output.lens_id.clone(), output.slow_model.clone(), e));
                }
            }
            if retry_errors.is_empty() {
                for output in &fanout.outputs {
                    log_json_normalization(self.logger.as_deref(), &output.parsed, "slow-lens");
                }
                return Ok(fanout);
            }

            // First repair representation/schema only, using the rejected
            // response and the caller's exact contract. A successful repair
            // avoids paying for a complete lens rerun and cannot bypass the
            // caller-owned validator below.
            let invalid_outputs: BTreeSet<(String, Option<String>)> = retry_errors
                .iter()
                .map(|(lens_id, model, _)| (lens_id.clone(), model.clone()))
                .collect();
            for output in fanout.outputs.iter_mut().filter(|output| {
                invalid_outputs.contains(&lens_run_key(output))
                    && lens_has_structural_json_error(output)
            }) {
                let errors: Vec<String> = retry_errors
                    .iter()
                    .filter(|(lens_id, model, _)| {
                        lens_id == &output.lens_id && model == &output.slow_model
                    })
                    .map(|(_, _, error)| error.clone())
                    .collect();
                if let Some(repaired) = self
                    .repair_lens_json(output, &errors, &repair, &ctx.previous_findings, shutdown)
                    .await
                {
                    *output = repaired;
                }
            }
            retry_errors.retain(|(lens_id, model, _)| {
                find_lens_output(&fanout.outputs, lens_id, model)
                    .map_or(true, |output| validate(output).is_err())
            });
            if retry_errors.is_empty() && fanout.failures.is_empty() {
                for output in &fanout.outputs {
                    log_json_normalization(self.logger.as_deref(), &output.parsed, "slow-lens");
                }
                return Ok(fanout);
            }
            let retry_runs: BTreeSet<(String, Option<String>)> = retry_errors
                .iter()
                .map(|(id, model, _)| (id.clone(), model.clone()))
                .collect();

            let repair_lenses: Vec<LensSpec> = lenses
                .iter()
                .filter(|lens| retry_runs.iter().any(|(id, _)| id == &lens.id))
                .cloned()
                .collect();
            if repair_lenses.is_empty() {
                return Err(AgentError::Other(format!(
                    "shared lens fan-out selected unknown lens id(s) for repair: {}",
                    retry_runs
                        .iter()
                        .map(|(id, model)| format!(
                            "{}@{}",
                            id,
                            model.as_deref().unwrap_or("unknown")
                        ))
                        .collect::<Vec<_>>()
                        .join(", ")
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
            for (lens_id, model, err) in &retry_errors {
                let model = model.as_deref().unwrap_or("unknown-model");
                detailed_repair.push_str(&format!("\n- lens '{lens_id}' model '{model}': {err}"));
            }
            let repaired = self
                .run_prepared_lens_fanout(
                    &prepared,
                    LensFanoutCall {
                        lenses: &repair_lenses,
                        all_lenses: lenses,
                        extra_lens_instruction: Some(&detailed_repair),
                        cache_mode: CacheMode::Parallel,
                        run_keys: Some(&retry_runs),
                    },
                    ctx,
                    shutdown,
                )
                .await?;
            fanout
                .outputs
                .retain(|output| !retry_runs.contains(&lens_run_key(output)));
            fanout.failures.retain(|failure| {
                !retry_runs.contains(&(failure.lens_id.clone(), failure.slow_model.clone()))
            });
            fanout.outputs.extend(
                repaired
                    .outputs
                    .into_iter()
                    .filter(|output| retry_runs.contains(&lens_run_key(output))),
            );
            fanout
                .failures
                .extend(repaired.failures.into_iter().filter(|failure| {
                    retry_runs.contains(&(failure.lens_id.clone(), failure.slow_model.clone()))
                }));
        }
        for output in &fanout.outputs {
            log_json_normalization(self.logger.as_deref(), &output.parsed, "slow-lens");
        }
        if let Some((actual, limit)) = fanout
            .failures
            .iter()
            .find_map(|failure| failure.over_input_limit)
        {
            return Err(AgentError::OverInputLimit { actual, limit });
        }
        Ok(fanout)
    }

    async fn repair_lens_json(
        &self,
        output: &LensRunOutput,
        errors: &[String],
        policy: &LensRepairPolicy<'_>,
        existing_findings: &[Finding],
        shutdown: &Shutdown,
    ) -> Option<LensRunOutput> {
        let variant = self
            .effective_slow_variants()
            .into_iter()
            .find(|variant| output.slow_model.as_deref() == Some(variant.label.as_str()))?;
        let contract =
            CodeResponseContract::new(output.allowed_response_extensions.iter().cloned());
        if let Ok(mut parsed) = contract
            .clone()
            .allowing_invalid_findings()
            .validate(&output.raw_response)
        {
            if !parsed.invalid_findings.is_empty() {
                let outcome = crate::finding_repair::repair_invalid_findings(
                    variant.client.clone(),
                    variant.model.clone(),
                    variant.max_tokens,
                    variant.max_input_tokens,
                    std::mem::take(&mut parsed.invalid_findings),
                    existing_findings,
                    crate::finding_repair::FindingRepairRuntime {
                        logger: self.logger.clone(),
                        thinking: variant.thinking,
                        cancel: Some(crate::finding_repair::FindingRepairCancel::Shutdown(
                            shutdown.clone(),
                        )),
                        usage: self.usage.clone(),
                        role: "slow",
                    },
                )
                .await
                .ok()?;
                if !outcome.unrepaired.is_empty() {
                    return None;
                }
                parsed.merge_repaired_findings(outcome.findings);
                let repaired_text =
                    replace_response_findings(&output.raw_response, &parsed.findings).ok()?;
                let parsed = contract.validate(&repaired_text).ok()?;
                return Some(LensRunOutput {
                    lens_id: output.lens_id.clone(),
                    lens: output.lens.clone(),
                    slow_model: output.slow_model.clone(),
                    parsed,
                    raw_response: repaired_text,
                    allowed_response_extensions: output.allowed_response_extensions.clone(),
                });
            }
        }
        let result = repair_json_response(JsonRepairCall {
            client: variant.client,
            model: variant.model.clone(),
            max_tokens: variant.max_tokens,
            max_input_tokens: variant.max_input_tokens,
            thinking: variant.thinking,
            contract: JsonContract {
                name: policy.contract_name,
                schema: policy.schema,
                instructions: policy.repair_instruction,
            },
            rejected_response: &output.raw_response,
            validation_errors: errors,
            logger: self.logger.clone(),
            log_kind: RepairLogKind::Code,
            shutdown: Some(shutdown.clone()),
        })
        .await
        .ok()?;
        record_usage(&self.usage, "slow", &variant.model, &result.usage);
        let parsed = match contract.accept_repair(&result.text) {
            Ok(parsed) => parsed,
            Err(_) => {
                tracing::warn!(
                    target: "kres_agents",
                    lens = %output.lens_id,
                    "JSON repair failed the strict lens response contract"
                );
                return None;
            }
        };
        Some(LensRunOutput {
            lens_id: output.lens_id.clone(),
            lens: output.lens.clone(),
            slow_model: output.slow_model.clone(),
            parsed,
            raw_response: result.text,
            allowed_response_extensions: output.allowed_response_extensions.clone(),
        })
    }

    async fn prepare_lens_fanout(
        &self,
        prompt: &str,
        ctx: &RunContext,
        shutdown: &Shutdown,
    ) -> Result<PreparedLensFanout, AgentError> {
        let composed = append_json_only_output_instruction(&prepend_original_prompt(
            prompt,
            &ctx.original_prompt,
        ));
        let prompt: &str = composed.as_str();
        let gather_composed;
        let gather_prompt = if let Some(gather) = ctx.gather_prompt.as_deref() {
            gather_composed = append_json_only_output_instruction(&prepend_original_prompt(
                gather,
                &ctx.original_prompt,
            ));
            gather_composed.as_str()
        } else {
            prompt
        };
        // Gather once via fast+main (same loop as run_once, up to the
        // point where we'd call the slow agent).
        let (symbols, context, fast_rounds, live_skills, task_skill_paths) =
            self.gather(gather_prompt, ctx, shutdown).await?;

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
        let (symbols, context) = crate::symbol::canonicalize_prompt_evidence(&symbols, &context);
        let previous_findings = kres_core::redact_findings_for_agent(&ctx.previous_findings);
        let mut shared_cp = CodePrompt::new(prompt)
            .with_symbols(&symbols)
            .with_context(&context)
            .with_previous_findings(&previous_findings);
        let synthesis_skills = split_skills_for_synthesis(live_skills.as_ref(), &task_skill_paths);
        if let Some(sk) = &synthesis_skills.common {
            shared_cp = shared_cp.with_common_skills(sk);
        }
        if let Some(sk) = &synthesis_skills.task {
            shared_cp = shared_cp.with_skills(sk);
        }
        if let Some(ref p) = ctx.plan {
            shared_cp = shared_cp.with_plan(p, ctx.active_plan_step_id.as_deref());
        }
        let shared_prefix = shared_cp
            .to_split_documents(LENS_SHARED_CACHE_FIELDS)?
            .stable;

        Ok(PreparedLensFanout {
            prompt: prompt.to_string(),
            shared_prefix,
            slow_variants,
            symbols,
            context,
            previous_findings,
            common_skills: synthesis_skills.common,
            task_skills: synthesis_skills.task,
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
            run_keys,
        } = call;
        let supplemental_lens = supplemental_lens_id(all_lenses);
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
                .with_symbols(&prepared.symbols)
                .with_context(&prepared.context)
                .with_previous_findings(&prepared.previous_findings)
                .with_parallel_lenses(&parallel_lenses)
                .with_lens_instruction(&lens_extra);
            // Put byte-stable skill material first, followed by guides loaded
            // during this task. Both halves are verbatim and together equal
            // the post-gather synthesis skill set.
            if let Some(sk) = &prepared.common_skills {
                lens_cp = lens_cp.with_common_skills(sk);
            }
            if let Some(sk) = &prepared.task_skills {
                lens_cp = lens_cp.with_skills(sk);
            }
            if let Some(ref p) = ctx.plan {
                lens_cp = lens_cp.with_plan(p, ctx.active_plan_step_id.as_deref());
            }
            let lens_suffix = lens_cp.to_delta_document(LENS_SHARED_CACHE_FIELDS)?;
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
            let lens_specs = lens_specs
                .iter()
                .filter(|spec| {
                    if !variant_runs_lens(
                        variant.supplemental_lens_only,
                        &spec.lens_id,
                        supplemental_lens,
                    ) {
                        return false;
                    }
                    run_keys.map_or(true, |keys| {
                        keys.contains(&(spec.lens_id.clone(), Some(variant.label.clone())))
                    })
                })
                .cloned()
                .collect::<Vec<_>>();
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
                        allowed_response_extensions: ctx.allowed_response_extensions.clone(),
                    });
                }
                Err((lens_id, model_label, error, over_input_limit)) => {
                    failures.push(LensRunFailure {
                        lens_id,
                        slow_model: Some(model_label),
                        error,
                        over_input_limit,
                    });
                }
            }
        }
        Ok(LensFanoutOutput {
            outputs,
            failures,
            fast_rounds: prepared.fast_rounds,
            attempted: prepared
                .slow_variants
                .iter()
                .map(|variant| {
                    lenses
                        .iter()
                        .filter(|lens| {
                            variant_runs_lens(
                                variant.supplemental_lens_only,
                                &lens.id,
                                supplemental_lens,
                            )
                        })
                        .count()
                })
                .sum(),
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
                supplemental_lens_only: false,
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
        ctx: &RunContext,
        shutdown: &Shutdown,
    ) -> Result<(Vec<Value>, Vec<Value>, u8, Option<Value>, BTreeSet<String>), AgentError> {
        let (mut symbols, mut context) =
            crate::symbol::canonicalize_prompt_evidence(&ctx.seed_symbols, &ctx.seed_context);
        let mut fast_rounds: u8 = 0;
        let mut fetched_keys: HashSet<String> = HashSet::new();
        // Honour mid-loop `skill_reads` in the one shared gather path. The
        // returned live payload feeds either single synthesis or lens fan-out.
        let mut live_skills: Option<Value> = if ctx.disable_skill_reads {
            None
        } else {
            self.skills.clone()
        };
        let mut round_symbols = symbols.clone();
        let mut round_context = context.clone();
        let mut round_skills = live_skills.clone();
        // Paths this task's `skill_reads` grafted into the live payload.
        // Drives both the per-round delta and the synthesis common/task split.
        let mut task_skill_paths: BTreeSet<String> = BTreeSet::new();
        let mut history: Vec<Message> = Vec::new();
        for round in 0..self.max_fast_rounds {
            if shutdown.is_cancelled() {
                return Err(AgentError::Other(format!(
                    "shutdown cancelled during fast round {round}"
                )));
            }
            fast_rounds = round + 1;
            let round_question = if round == 0 {
                prompt
            } else {
                "Continue the same task using the newly gathered evidence in this message. Earlier task scope, evidence, and decisions remain in the conversation history."
            };
            let mut cp = CodePrompt::new(round_question)
                .with_symbols(&round_symbols)
                .with_context(&round_context);
            // Prior findings ride the round-0 cached prefix. Later rounds
            // retain them through conversation history.
            let fast_previous_findings =
                (round == 0).then(|| kres_core::redact_findings_for_agent(&ctx.previous_findings));
            if let Some(findings) = fast_previous_findings.as_ref() {
                cp = cp.with_previous_findings(findings);
            }
            if let Some(sk) = &round_skills {
                cp = cp.with_skills(sk);
            }
            if round == 0 {
                if let Some(p) = ctx.plan.as_ref() {
                    cp = cp.with_plan(p, ctx.active_plan_step_id.as_deref());
                }
            }
            // Only the first user turn needs a separately cached stable
            // task-scope block. Later turns are evidence deltas and each gets
            // one cache marker. With the cached system prompt this stays
            // within Anthropic's four-block protocol maximum while preserving
            // the exact conversation text.
            let split = if round == 0 {
                cp.to_split_documents(CACHED_PREFIX_FIELDS)?
            } else {
                crate::prompt::SplitPrompt {
                    stable: String::new(),
                    delta: cp.to_json_string()?,
                }
            };
            let logged_content = split.rendered();
            history.push(Message {
                role: "user".into(),
                content: split.delta,
                cache: true,
                cached_prefix: if split.stable.is_empty() {
                    None
                } else {
                    Some(split.stable)
                },
            });
            mark_last_n_user_cached(&mut history, 2);
            let logged_request = serde_json::to_string(&serde_json::json!({
                "messages": history.iter().map(|message| {
                    serde_json::json!({
                        "role": message.role,
                        "content": format!(
                            "{}{}",
                            message.cached_prefix.as_deref().unwrap_or(""),
                            message.content
                        ),
                    })
                }).collect::<Vec<_>>(),
            }))?;
            let mut cfg = CallConfig::defaults_for(self.fast_model.clone())
                .with_max_tokens(self.fast_max_tokens)
                .with_stream_label("fast gather");
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
                let task = if ctx.task_brief.is_empty() {
                    "task"
                } else {
                    &ctx.task_brief
                };
                let label = format!("phase=fast-gather task={task} round={fast_rounds}");
                let meta = cfg.request_meta();
                lg.log_code_user_request_content(
                    Some(&label),
                    &logged_content,
                    &logged_request,
                    Some(&meta),
                );
            }
            let text = tokio::select! {
                _ = shutdown.cancelled() => return Err(AgentError::Other("cancelled during fast call".into())),
                r = self.fast_client.messages_streaming(&cfg, &history) => {
                    let resp = r.map_err(AgentError::from)?;
                    record_usage(&self.usage, "fast", &self.fast_model, &resp.usage);
                    let t = extract_text(&resp);
                    if let Some(lg) = &self.logger {
                        let th = extract_thinking(&resp);
                        let task = if ctx.task_brief.is_empty() {
                            "task"
                        } else {
                            &ctx.task_brief
                        };
                        let label = format!("phase=fast-gather task={task} round={fast_rounds}");
                        lg.log_code_labeled_with_model(
                            "assistant",
                            Some(&label),
                            &t,
                            Some(log_usage(&resp.usage)),
                            th.as_deref(),
                            resp.model.as_deref(),
                        );
                    }
                    t
                }
            };
            let parsed = match validate_fast_gather_text_for_run(
                &text,
                ctx.disable_skill_reads,
                ctx.allowed_gather_kinds.as_ref(),
            ) {
                Ok(parsed) => parsed,
                Err(errors) => {
                    kres_core::async_eprintln!(
                        "[fast gather round {fast_rounds}] invalid response; retrying once: {}",
                        errors.join("; ")
                    );
                    let repaired = self
                        .repair_fast_gather_response(
                            &text,
                            &errors,
                            "fast gather schema repair",
                            shutdown,
                        )
                        .await?;
                    let policy_errors = fast_gather_run_policy_errors(
                        &repaired,
                        ctx.disable_skill_reads,
                        ctx.allowed_gather_kinds.as_ref(),
                    );
                    if !policy_errors.is_empty() {
                        return Err(AgentError::Other(format!(
                            "fast gather response still violated run policy after repair: {}",
                            policy_errors.join("; ")
                        )));
                    }
                    repaired
                }
            };
            log_json_normalization(self.logger.as_deref(), &parsed, "fast-gather");
            history.push(Message::plain(
                "assistant",
                serde_json::to_string(&json!({
                    "analysis": &parsed.analysis,
                    "followups": &parsed.followups,
                    "skill_reads": &parsed.skill_reads,
                    "ready_for_slow": parsed.ready_for_slow,
                    "plan": &parsed.plan,
                }))?,
            ));
            round_symbols.clear();
            round_context.clear();
            round_skills = None;
            if !parsed.skill_reads.is_empty() {
                let grafted: BTreeSet<String> =
                    apply_skill_reads(&mut live_skills, &parsed.skill_reads)
                        .into_iter()
                        .collect();
                task_skill_paths.extend(grafted.iter().cloned());
                // Send only the files this round added; earlier ones remain in
                // conversation history.
                if let Some(live) = live_skills.as_ref() {
                    round_skills = nonempty_object(project_skills(live, &grafted, true));
                }
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
                f = self.fetcher.fetch(&novel, ctx.plan.as_ref()) => f?,
            };
            let (fetched_symbols, fetched_context) =
                crate::symbol::canonicalize_prompt_evidence(&fetched.symbols, &fetched.context);
            for symbol in fetched_symbols {
                if crate::symbol::append_prompt_evidence(&mut symbols, symbol.clone()) {
                    round_symbols.push(symbol);
                }
            }
            for item in fetched_context {
                if crate::symbol::append_prompt_evidence(&mut context, item.clone()) {
                    round_context.push(item);
                }
            }
        }
        let (symbols, context) = crate::symbol::canonicalize_prompt_evidence(&symbols, &context);
        Ok((symbols, context, fast_rounds, live_skills, task_skill_paths))
    }
}

fn lens_run_key(output: &LensRunOutput) -> (String, Option<String>) {
    (output.lens_id.clone(), output.slow_model.clone())
}

fn find_lens_output<'a>(
    outputs: &'a [LensRunOutput],
    lens_id: &str,
    model: &Option<String>,
) -> Option<&'a LensRunOutput> {
    outputs
        .iter()
        .find(|output| output.lens_id == lens_id && &output.slow_model == model)
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
    pub thinking: Option<ThinkingBudget>,
    pub usage: Option<Arc<UsageTracker>>,
}

/// Graft a list of newly-requested skill files into the per-run
/// skills JSON. Matches `skill_reads` handling at
///reads happen against the fast-agent's
/// filesystem, and the file contents land in the FIRST skill's
/// `files` map (most skills are singletons in practice).
/// Returns the paths actually grafted into the payload, in request order.
/// A failed read still counts: its `[skill_read failed: ...]` marker is new
/// bytes this task and belongs in the task-specific half of the split.
pub fn apply_skill_reads(skills: &mut Option<Value>, reads: &[String]) -> Vec<String> {
    if reads.is_empty() {
        return Vec::new();
    }
    // Ensure there's a skills object to mutate.
    let obj = skills.get_or_insert_with(|| Value::Object(serde_json::Map::new()));
    let map = match obj.as_object_mut() {
        Some(m) => m,
        None => return Vec::new(),
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
    let mut grafted = Vec::new();
    for path in reads {
        if path.is_empty() {
            continue;
        }
        grafted.push(path.clone());
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
    grafted
}

#[derive(Default)]
struct SynthesisSkills {
    common: Option<Value>,
    task: Option<Value>,
}

/// Split the live skill payload into the part that was present before this
/// task ran and the files this task's `skill_reads` grafted in.
///
/// `apply_skill_reads` is the only mutator of the payload, so the set of paths
/// it reported is an exact description of the difference — no tree diffing
/// needed, and no risk of a structural comparison disagreeing with what was
/// actually loaded. The union of the two halves is the live payload verbatim;
/// this is a cache-layout split, not a reduction. `common` is byte-stable
/// across tasks that share a base payload, so it can lead the cached prefix.
fn split_skills_for_synthesis(
    live: Option<&Value>,
    task_paths: &BTreeSet<String>,
) -> SynthesisSkills {
    let Some(live) = live else {
        return SynthesisSkills::default();
    };
    SynthesisSkills {
        common: nonempty_object(project_skills(live, task_paths, false)),
        task: nonempty_object(project_skills(live, task_paths, true)),
    }
}

/// Project the skill payload onto `paths`. With `only_listed`, keep just those
/// files (the task-selected half, which needs no scaffold — the common half
/// already carries it). Otherwise keep everything except those files.
fn project_skills(live: &Value, paths: &BTreeSet<String>, only_listed: bool) -> Value {
    let mut projected = serde_json::Map::new();
    let Some(skills) = live.as_object() else {
        return Value::Object(projected);
    };
    for (skill_name, skill) in skills {
        let Some(fields) = skill.as_object() else {
            if !only_listed {
                projected.insert(skill_name.clone(), skill.clone());
            }
            continue;
        };
        let mut kept = serde_json::Map::new();
        for (field, value) in fields {
            if field == "files" {
                let mut files = serde_json::Map::new();
                if let Some(live_files) = value.as_object() {
                    for (path, body) in live_files {
                        if paths.contains(path) == only_listed {
                            files.insert(path.clone(), body.clone());
                        }
                    }
                }
                // The common half must reproduce the base payload byte for
                // byte, including an empty `files` map, or a task that loads
                // no guides and one that loads several emit different common
                // bytes and lose the cross-task cache hit.
                if !files.is_empty() || !only_listed {
                    kept.insert(field.clone(), Value::Object(files));
                }
            } else if !only_listed {
                kept.insert(field.clone(), value.clone());
            }
        }
        if !kept.is_empty() {
            projected.insert(skill_name.clone(), Value::Object(kept));
        }
    }
    Value::Object(projected)
}

fn nonempty_object(value: Value) -> Option<Value> {
    value
        .as_object()
        .is_some_and(|object| !object.is_empty())
        .then_some(value)
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
    let model = output.slow_model.as_deref().unwrap_or("unknown-model");
    let parsed = CodeResponseContract::new(output.allowed_response_extensions.iter().cloned())
        .validate(&output.raw_response)
        .map_err(|errors| format!("{} ({model}): {}", output.lens_id, errors.join("; ")))?;
    if parsed.analysis.trim().is_empty()
        && parsed.findings.is_empty()
        && parsed.followups.is_empty()
    {
        Err(format!(
            "{} ({model}): no analysis, findings, or followups",
            output.lens_id
        ))
    } else {
        Ok(())
    }
}

fn lens_has_structural_json_error(output: &LensRunOutput) -> bool {
    CodeResponseContract::new(output.allowed_response_extensions.iter().cloned())
        .validate(&output.raw_response)
        .is_err()
}

fn replace_response_findings(
    text: &str,
    findings: &[kres_core::findings::Finding],
) -> Result<String, AgentError> {
    let normalized = crate::response::normalized_code_response_json(text)
        .map_err(|errors| AgentError::Other(errors.join("; ")))?;
    let mut root: Value = crate::json_repair::parse_strict_json("code-agent", &normalized)
        .map_err(|errors| AgentError::Other(errors.join("; ")))?;
    let object = root
        .as_object_mut()
        .ok_or_else(|| AgentError::Other("response must be one JSON object".into()))?;
    object.insert("findings".into(), serde_json::to_value(findings)?);
    serde_json::to_string(&root).map_err(AgentError::from)
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

    #[test]
    fn low_effort_profile_preserves_thinking_shape() {
        assert_eq!(
            lower_thinking_effort(ThinkingBudget::Adaptive(Effort::XHigh), 128_000),
            ThinkingBudget::Adaptive(Effort::Low)
        );
        assert_eq!(
            lower_thinking_effort(ThinkingBudget::ExplicitBudget(32_000), 128_000),
            ThinkingBudget::ExplicitBudget(1_024)
        );
        assert_eq!(
            lower_thinking_effort(ThinkingBudget::Disabled, 128_000),
            ThinkingBudget::Disabled
        );
    }

    #[test]
    fn secondary_slow_model_runs_only_workflow_supplemental_lens() {
        let review_lenses = vec![
            LensSpec::new("memory", "memory"),
            LensSpec::new("general", "general"),
        ];
        let fix_lenses = vec![
            LensSpec::new("general", "general"),
            LensSpec::new("maintainer", "maintainer"),
        ];

        assert!(variant_runs_lens(
            false,
            "memory",
            supplemental_lens_id(&review_lenses)
        ));
        assert!(!variant_runs_lens(
            true,
            "memory",
            supplemental_lens_id(&review_lenses)
        ));
        assert!(variant_runs_lens(
            true,
            "general",
            supplemental_lens_id(&review_lenses)
        ));
        assert!(!variant_runs_lens(
            true,
            "general",
            supplemental_lens_id(&fix_lenses)
        ));
        assert!(variant_runs_lens(
            true,
            "maintainer",
            supplemental_lens_id(&fix_lenses)
        ));
    }
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

    #[test]
    fn fast_gather_validation_rejects_raw_text() {
        let response = diagnose_code_response("please read foo.c");
        let errors = validate_fast_gather_response(&response).unwrap_err();
        assert!(errors.iter().any(|error| error.contains("JSON object")));
    }

    #[test]
    fn generic_lens_validation_rejects_raw_text_and_bad_shapes() {
        let raw = LensRunOutput {
            lens_id: "memory".into(),
            lens: json!({"id":"memory"}),
            slow_model: Some("test".into()),
            raw_response: "prose".into(),
            parsed: diagnose_code_response("prose"),
            allowed_response_extensions: BTreeSet::new(),
        };
        assert!(validate_generic_lens_output(&raw)
            .unwrap_err()
            .contains("JSON object"));

        let malformed = LensRunOutput {
            raw_response: r#"{"analysis":"x","findings":"bad"}"#.into(),
            parsed: diagnose_code_response(r#"{"analysis":"x","findings":"bad"}"#),
            ..raw
        };
        assert!(validate_generic_lens_output(&malformed)
            .unwrap_err()
            .contains("findings"));

        let empty_text = r#"{"analysis":""}"#;
        let empty = LensRunOutput {
            raw_response: empty_text.into(),
            parsed: CodeResponseContract::default()
                .validate(empty_text)
                .unwrap(),
            ..malformed
        };
        assert!(validate_generic_lens_output(&empty).is_err());
        assert!(!lens_has_structural_json_error(&empty));
    }

    #[test]
    fn comparison_lens_lookup_is_scoped_by_model() {
        let outputs = vec![
            LensRunOutput {
                lens_id: "bounds".into(),
                lens: json!({"id":"bounds"}),
                slow_model: Some("model-a".into()),
                raw_response: r#"{"analysis":"a"}"#.into(),
                parsed: diagnose_code_response(r#"{"analysis":"a"}"#),
                allowed_response_extensions: BTreeSet::new(),
            },
            LensRunOutput {
                lens_id: "bounds".into(),
                lens: json!({"id":"bounds"}),
                slow_model: Some("model-b".into()),
                raw_response: "invalid prose".into(),
                parsed: diagnose_code_response("invalid prose"),
                allowed_response_extensions: BTreeSet::new(),
            },
        ];
        let model_a = Some("model-a".to_string());
        let model_b = Some("model-b".to_string());
        assert!(validate_generic_lens_output(
            find_lens_output(&outputs, "bounds", &model_a).unwrap()
        )
        .is_ok());
        assert!(validate_generic_lens_output(
            find_lens_output(&outputs, "bounds", &model_b).unwrap()
        )
        .is_err());
    }

    #[test]
    fn fast_gather_validation_rejects_non_array_and_bad_items() {
        let non_array = diagnose_code_response(r#"{"analysis":"x","followups":"read foo"}"#);
        assert!(validate_fast_gather_response(&non_array)
            .unwrap_err()
            .iter()
            .any(|error| error.contains("must be an array")));

        let bad_item = diagnose_code_response(
            r#"{"analysis":"x","followups":[{"type":"read","reason":"why"}]}"#,
        );
        assert!(validate_fast_gather_response(&bad_item)
            .unwrap_err()
            .iter()
            .any(|error| error.contains("followups[0] is invalid")));
    }

    #[test]
    fn fast_gather_validation_rejects_unknown_fields() {
        let response = diagnose_code_response(r#"{"analysis":"x","folowups":[]}"#);
        let errors = validate_fast_gather_response(&response).unwrap_err();
        assert!(errors.iter().any(|error| error.contains("folowups")));
    }

    #[test]
    fn fast_gather_validation_rejects_bad_semantics() {
        let response = diagnose_code_response(
            r#"{"analysis":"x","followups":[{"type":"invented","name":" ","reason":""}]}"#,
        );
        let errors = validate_fast_gather_response(&response).unwrap_err();
        assert!(errors.iter().any(|error| error.contains("unsupported")));
        assert!(errors
            .iter()
            .any(|error| error.contains("name must not be empty")));
        assert!(errors
            .iter()
            .any(|error| error.contains("reason must not be empty")));
    }

    #[test]
    fn fast_gather_validation_accepts_typed_request() {
        let response = diagnose_code_response(
            r#"{"analysis":"need source","followups":[{"type":"read","name":"mm/filemap.c:1+80","reason":"map entry points"}]}"#,
        );
        assert!(validate_fast_gather_response(&response).is_ok());
    }

    #[test]
    fn fast_gather_validation_accepts_file_survey_request() {
        let response = diagnose_code_response(
            r#"{"analysis":"need an outline","followups":[{"type":"survey","name":"mm/filemap.c","reason":"build a targeted review inventory"}]}"#,
        );
        assert!(validate_fast_gather_response(&response).is_ok());
    }

    #[test]
    fn fast_gather_validation_rejects_skill_reads_when_disabled() {
        let text = r#"{"analysis":"load more policy","followups":[],"skill_reads":["technical-patterns.md"],"ready_for_slow":false}"#;

        assert!(validate_fast_gather_text_for_run(text, false, None).is_ok());
        let errors = validate_fast_gather_text_for_run(text, true, None).unwrap_err();
        assert!(errors.iter().any(|error| error.contains("disabled")));
    }

    /// Unit-test the loop-back decision table. We can't easily run
    /// the whole AgentRunner without a live API, but we can test
    /// the key condition that caused the Phase-1 "did nothing" bug.
    #[test]
    fn parse_only_skill_reads_triggers_loopback() {
        // The fast agent emits a response with nothing but
        // skill_reads. This should NOT be treated as "ready for slow".
        let r = diagnose_code_response(
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
        let r = diagnose_code_response(r#"{"analysis": "no work needed"}"#);
        let only_skill_reads =
            r.followups.is_empty() && !r.ready_for_slow && !r.skill_reads.is_empty();
        assert!(!only_skill_reads);
        assert!(r.followups.is_empty());
    }

    #[test]
    fn parse_ready_for_slow_short_circuits() {
        let r = diagnose_code_response(
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
        let r = diagnose_code_response(
            r#"{"analysis": "need a target",
                "followups": [
                    {"type": "question", "name": "which file?", "reason": "the target is ambiguous"},
                    {"type": "question", "name": "which function?", "reason": "the requested scope is ambiguous"}
                ],
                "ready_for_slow": false}"#,
        );
        assert!(!r.followups.is_empty());
        assert!(r.followups.iter().all(|f| f.kind == "question"));
    }

    #[test]
    fn mixed_followups_do_not_trip_early_exit() {
        let r = diagnose_code_response(
            r#"{"analysis": "need a target",
                "followups": [
                    {"type": "question", "name": "which file?", "reason": "the target is ambiguous"},
                    {"type": "source", "name": "foo", "reason": "the implementation is required"}
                ],
                "ready_for_slow": false}"#,
        );
        assert!(!r.followups.iter().all(|f| f.kind == "question"));
    }

    #[test]
    fn prompt_evidence_is_never_removed_for_size() {
        let symbols = vec![
            json!({"name": "old", "definition": "x".repeat(80)}),
            json!({
                "name": "new",
                "definition": "small"
            }),
        ];
        let context = vec![
            json!({"source": "old", "content": "y".repeat(80)}),
            json!({
                "source": "new",
                "content": "small"
            }),
        ];

        let (preserved_symbols, preserved_context) =
            crate::symbol::canonicalize_prompt_evidence(&symbols, &context);

        assert_eq!(preserved_symbols.len(), symbols.len());
        assert_eq!(preserved_context.len(), context.len());
        assert_eq!(preserved_symbols[0]["definition"], symbols[0]["definition"]);
        assert_eq!(preserved_symbols[1]["definition"], symbols[1]["definition"]);
        assert_eq!(preserved_context[0]["content"], context[0]["content"]);
        assert_eq!(preserved_context[1]["content"], context[1]["content"]);
    }

    #[test]
    fn every_prior_finding_reaches_the_prompt_with_its_source_intact() {
        fn finding(id: &str) -> Finding {
            Finding {
                id: id.into(),
                title: id.into(),
                severity: kres_core::Severity::Medium,
                status: kres_core::Status::Active,
                relevant_symbols: Vec::new(),
                relevant_file_sections: Vec::new(),
                summary: "summary".into(),
                reproducer_sketch: "reproducer".into(),
                impact: "impact".into(),
                mechanism_detail: None,
                fix_sketch: None,
                open_questions: Vec::new(),
                first_seen_task: None,
                last_updated_task: None,
                first_seen_at: None,
                related_finding_ids: Vec::new(),
                details: Vec::new(),
                reactivate: false,
                introduced_by: None,
            }
        }

        // A finding anchored somewhere the task never mentions is still sent
        // in full: cross-file contract review depends on seeing its source.
        let mut anchored_elsewhere = finding("elsewhere");
        anchored_elsewhere
            .relevant_symbols
            .push(kres_core::findings::RelevantSymbol {
                name: "other_fn".into(),
                filename: "other.c".into(),
                line: 20,
                definition: "y".repeat(1_100_000),
            });
        let mut anchored_here = finding("here");
        anchored_here
            .relevant_symbols
            .push(kres_core::findings::RelevantSymbol {
                name: "target_fn".into(),
                filename: "target.c".into(),
                line: 10,
                definition: "x".repeat(1_100_000),
            });

        let input = vec![anchored_elsewhere, anchored_here];
        let shipped = kres_core::redact_findings_for_agent(&input);

        assert_eq!(shipped.len(), 2);
        for finding in &shipped {
            assert_eq!(
                finding.relevant_symbols[0].definition.len(),
                1_100_000,
                "finding {} lost its source body",
                finding.id
            );
        }
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

    #[test]
    fn synthesis_skills_split_stable_common_from_task_selected_guides() {
        let live = json!({
            "kernel": {
                "content": "stable scaffold",
                "files": {
                    "/prompts/technical-patterns.md": "stable patterns",
                    "/prompts/subsystem/subsystem.md": "routing index",
                    "/prompts/subsystem/mm-reclaim.md": "selected guide"
                }
            }
        });
        let task_paths = BTreeSet::from(["/prompts/subsystem/mm-reclaim.md".to_string()]);

        let split = split_skills_for_synthesis(Some(&live), &task_paths);
        let common = split.common.unwrap();
        let task = split.task.unwrap();

        // Common half keeps the scaffold and everything loaded at startup,
        // including the routing index — it is byte-stable, so it cache-hits
        // rather than needing to be dropped.
        assert_eq!(common["kernel"]["content"], "stable scaffold");
        assert_eq!(
            common["kernel"]["files"]["/prompts/technical-patterns.md"],
            "stable patterns"
        );
        assert_eq!(
            common["kernel"]["files"]["/prompts/subsystem/subsystem.md"],
            "routing index"
        );
        assert!(common["kernel"]["files"]
            .get("/prompts/subsystem/mm-reclaim.md")
            .is_none());

        // Task half carries only what this task loaded, with no scaffold.
        assert_eq!(
            task["kernel"]["files"]["/prompts/subsystem/mm-reclaim.md"],
            "selected guide"
        );
        assert_eq!(task["kernel"]["files"].as_object().unwrap().len(), 1);
        assert!(task["kernel"].get("content").is_none());
    }

    #[test]
    fn synthesis_skill_halves_reunite_into_the_live_payload() {
        // The split is a cache-layout decision. Every byte the fast agent had
        // after gathering must still reach synthesis across the two halves.
        let live = json!({
            "kernel": {
                "content": "scaffold",
                "files": {
                    "/a.md": "base",
                    "/b.md": "loaded this task",
                    "/c.md": "[skill_read failed: not found]"
                }
            },
            "other": {"content": "second skill", "files": {}}
        });
        let task_paths = BTreeSet::from(["/b.md".to_string(), "/c.md".to_string()]);

        let split = split_skills_for_synthesis(Some(&live), &task_paths);

        let mut union = split.common.unwrap();
        let task = split.task.unwrap();
        for (skill, fields) in task.as_object().unwrap() {
            let files = fields["files"].as_object().unwrap();
            let target = union[skill]["files"].as_object_mut().unwrap();
            for (path, body) in files {
                target.insert(path.clone(), body.clone());
            }
        }
        assert_eq!(union, live);
    }

    #[test]
    fn a_failed_skill_read_stays_in_the_task_half() {
        // A failure marker is new bytes this task produced; it belongs with
        // the task-selected files, not the byte-stable common prefix.
        let live = json!({
            "kernel": {
                "content": "scaffold",
                "files": {"/missing.md": "[skill_read failed: not found]"}
            }
        });
        let task_paths = BTreeSet::from(["/missing.md".to_string()]);

        let split = split_skills_for_synthesis(Some(&live), &task_paths);

        assert_eq!(
            split.task.unwrap()["kernel"]["files"]["/missing.md"],
            "[skill_read failed: not found]"
        );
        // Common reproduces the base payload exactly: scaffold plus an empty
        // files map, which is what a task loading nothing would also emit.
        let common = split.common.unwrap();
        assert_eq!(common["kernel"]["content"], "scaffold");
        assert_eq!(
            common["kernel"]["files"].as_object().unwrap().len(),
            0,
            "the failure marker must not leak into the stable prefix"
        );
    }

    #[test]
    fn gather_cache_markers_leave_room_for_cached_system_prompt() {
        fn marker_count(value: &Value) -> usize {
            match value {
                Value::Array(values) => values.iter().map(marker_count).sum(),
                Value::Object(values) => {
                    usize::from(values.contains_key("cache_control"))
                        + values.values().map(marker_count).sum::<usize>()
                }
                _ => 0,
            }
        }

        let mut history = vec![
            Message::plain("user", "evidence").with_cached_prefix("stable task scope"),
            Message::plain("assistant", "decision"),
            Message::cached("user", "new evidence delta"),
        ];
        history[0].cache = true;
        mark_last_n_user_cached(&mut history, 2);

        let serialized = serde_json::to_value(&history).unwrap();
        assert_eq!(marker_count(&serialized), 3);
        assert!(marker_count(&serialized) < 4);
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
