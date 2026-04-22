//! Single-task orchestrator.
//!
//! Wires fast agent → main agent (data fetch) → slow agent in the same
//! order as . Shutdown-aware (bugs.md#C2): every
//! await inside the loop is inside `tokio::select!` with the task's
//! Shutdown, so /stop / /clear / --turns reaches the loop immediately.
//!
//! Phase 4b limits:
//! - Single lens only (no parallel slow-agent fan-out yet — that's
//!   Phase 5).
//! - Main-agent data fetch is backed by a trait, so kres-repl can
//!   inject the real semcode/grep/read backend without kres-agents
//!   depending on kres-mcp.

use std::sync::Arc;

use async_trait::async_trait;
use futures::future::join_all;
use serde_json::{json, Value};

use kres_core::cost::UsageTracker;
use kres_core::findings::Finding;
use kres_core::lens::LensSpec;
use kres_core::log::{LoggedUsage, TurnLogger};
use kres_core::shrink::{shrink_findings_to_budget, shrink_json_list_to_budget};
use kres_core::shutdown::Shutdown;
use kres_llm::{client::Client, config::CallConfig, request::Message, Model};

use crate::{
    consolidate::{consolidate_lenses_with_logger, LensOutput},
    error::AgentError,
    followup::Followup,
    prompt::CodePrompt,
    response::{parse_code_response, CodeResponse, ParseStrategy},
};

/// CodePrompt fields that go into the cached-prefix block.
///
/// Only fields that are BYTE-IDENTICAL across rounds of the same
/// task belong here. `question`, `skills`, `parallel_lenses`, and
/// `previous_findings` satisfy that. `previously_fetched` does NOT
/// — it accumulates each round (round 1 has none, round 2 carries
/// round 1's manifest, round 3 carries rounds 1+2). If it were in
/// the prefix, round 2's prefix would diverge from round 1's and
/// the cache wouldn't hit. Keep it in the volatile tail alongside
/// `symbols` / `context`.
///
/// Evidence this matters: session `0392fbb5…` landed round 1 with
/// `cache_read=8403` (full prefix hit) but round 2 with
/// `cache_read=0, cache_create=33275` (miss) — the miss was driven
/// by previously_fetched growing.
// Keep the plan OUT of the cached prefix. The plan is the most
// mutation-prone static field in this envelope: define_plan on
// first submit, a slow-agent rewrite on the first turn, and the
// todo-agent can reshape it every turn. If the plan sat in the
// prefix, every rewrite would bust the prompt cache for the
// following fast + slow calls — wiping out the tens-of-kB
// question + skills + lenses prefix alongside it. Routing the
// plan through the volatile suffix keeps the big cache intact;
// the plan bytes (usually a handful of KB) get re-sent every
// round but nothing the prefix carried needs to be.
//
// `plan_rewrite_allowed` changes at most once per top-level
// prompt (submit_prompt_inner turns it on for the first task
// and off for follow-ups), so it stays in the prefix.
const CACHED_PREFIX_FIELDS: &[&str] = &[
    "question",
    "skills",
    "parallel_lenses",
    "previous_findings",
    "plan_rewrite_allowed",
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

/// No-op fetcher used in tests and in the Phase-4b sanity path. It
/// returns empty results regardless of input — good enough to prove
/// the orchestration plumbing without hitting any real backend.
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

/// Orchestrator for one Task turn.
pub struct Orchestrator {
    pub fast_client: Arc<Client>,
    pub fast_model: Model,
    pub fast_system: Option<String>,
    pub fast_max_tokens: u32,
    pub fast_max_input_tokens: Option<u32>,

    pub slow_client: Arc<Client>,
    pub slow_model: Model,
    pub slow_system: Option<String>,
    pub slow_max_tokens: u32,
    pub slow_max_input_tokens: Option<u32>,

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
    /// the analysis prompt it doesn't force the "you are a deep code
    /// analysis agent, emit findings" stance — generic tasks
    /// answer the operator's question directly and can emit
    /// `bash` followups for execution-style prompts. When `None`,
    /// generic tasks fall back to `slow_system` (the operator gets
    /// review-flavoured behaviour, which is usually fine but not
    /// ideal for free-form questions).
    pub slow_generic_system: Option<String>,

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

/// Inputs to one run that vary per-task. Separated from Orchestrator
/// so a single Orchestrator can run many tasks in parallel, each with
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
    /// Analysis-mode tasks.
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

impl Orchestrator {
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
        // Per-run skills clone — mutated mid-loop by `skill_reads`
        // responses so the fast agent can pull in extra files
        // mid-gather (§27,).
        let mut live_skills: Option<Value> = self.skills.clone();
        let mut symbols: Vec<Value> = Vec::new();
        let mut context: Vec<Value> = Vec::new();
        let mut prev_n_syms: usize = 0;
        let mut prev_n_ctx: usize = 0;
        let mut fast_rounds: u8 = 0;

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
            let mut cp = CodePrompt::new(prompt)
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
            if let Some(s) = &self.fast_system {
                cfg = cfg.with_system(s.clone());
            }
            if let Some(n) = self.fast_max_input_tokens {
                cfg = cfg.with_max_input_tokens(n);
            }

            if let Some(lg) = &self.logger {
                lg.log_code("user", &logged_content, None, None);
            }
            kres_core::async_eprintln!(
                "[fast round {}/{}] sending {}k chars ({}k cached prefix + {}k volatile; +{} syms, +{} ctx vs prev)",
                fast_rounds,
                self.max_fast_rounds,
                (prefix.len() + suffix.len()) / 1000,
                prefix.len() / 1000,
                suffix.len() / 1000,
                new_syms.len(),
                new_ctx.len(),
            );
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
                        lg.log_code(
                            "assistant",
                            &t,
                            Some(log_usage(&resp.usage)),
                            thinking.as_deref(),
                        );
                    }
                    kres_core::async_eprintln!(
                        "[fast round {}] reply: in={} out={} cache_read={} cache_create={}",
                        fast_rounds,
                        resp.usage.input_tokens,
                        resp.usage.output_tokens,
                        resp.usage.cache_read_input_tokens,
                        resp.usage.cache_creation_input_tokens,
                    );
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
                kres_core::async_eprintln!(
                    "[fast round {}] ready_for_slow (analysis: {}k chars)",
                    fast_rounds,
                    parsed.analysis.len() / 1000
                );
                break;
            }
            if parsed.followups.is_empty() && !only_skill_reads {
                kres_core::async_eprintln!(
                    "[fast round {}] no followups, no skill_reads — proceeding to slow",
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
            if !parsed.followups.is_empty()
                && parsed.followups.iter().all(|f| f.kind == "question")
            {
                kres_core::async_eprintln!(
                    "[fast round {}] only type:question followups — breaking to slow",
                    fast_rounds
                );
                break;
            }

            // Summarise the followups so operators can see what the
            // fast agent asked the main agent to fetch on their
            // behalf.
            let fu_summary: Vec<String> = parsed
                .followups
                .iter()
                .take(8)
                .map(|fu| format!("{}:{}", fu.kind, truncate(&fu.name, 40)))
                .collect();
            let tail = if parsed.followups.len() > 8 {
                format!(", +{} more", parsed.followups.len() - 8)
            } else {
                String::new()
            };
            kres_core::async_eprintln!(
                "[fast round {}] {} followup(s): {}{tail}",
                fast_rounds,
                parsed.followups.len(),
                fu_summary.join(", "),
            );

            // Fetch via main agent (data layer).
            let fetched = tokio::select! {
                _ = shutdown.cancelled() => {
                    return Err(AgentError::Other("cancelled during fetch".into()));
                }
                f = self.fetcher.fetch(&parsed.followups, ctx.plan.as_ref()) => f?,
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
        // bugs.md#L5: budget previous_findings to ~1M chars before
        // shipping. Symbols and context are also trimmed — a single
        // fetcher response can blow the per-slot budget even with a
        // short gather loop (e.g. a multi-MB type definition).
        let trimmed_prev = shrink_findings_to_budget(&ctx.previous_findings, 1_000_000);
        let trimmed_symbols = shrink_json_list_to_budget(&symbols, 1_000_000);
        let trimmed_context = shrink_json_list_to_budget(&context, 1_000_000);
        // §cache: same split policy on the slow-agent user message.
        // The question + previous_findings are stable across a
        // retry; symbols/context are volatile per-task.
        // §cache: include skills in the slow-agent prompt too.
        // Without it the cached prefix is just `{question}` — on
        // the order of 50 bytes — which Anthropic ignores for
        // prompt caching (there's a minimum cacheable block size).
        // Session `83f8ef0e` confirmed: slow call top-level keys
        // were `[question, context]`, no skills, `cache_read=0`,
        // `cache_create=27349`. Passing skills lifts the prefix
        // well past the minimum and gives the slow agent the same
        // domain guidance the fast agent has.
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
        let (slow_prefix, slow_suffix) = slow_cp.to_cached_split_json(CACHED_PREFIX_FIELDS)?;
        let slow_logged = format!("{slow_prefix}{slow_suffix}");
        let messages = vec![Message {
            role: "user".into(),
            content: slow_suffix.clone(),
            cache: true,
            cached_prefix: if slow_prefix.is_empty() {
                None
            } else {
                Some(slow_prefix.clone())
            },
        }];
        let mut cfg = CallConfig::defaults_for(self.slow_model.clone())
            .with_max_tokens(self.slow_max_tokens)
            .with_stream_label(match ctx.mode {
                kres_core::TaskMode::Analysis => "slow",
                kres_core::TaskMode::Generic => "slow (generic)",
                kres_core::TaskMode::Coding => "slow (coding)",
            });
        // Coding-mode tasks want a different system prompt: one that
        // tells the slow agent to emit `code_output` rather than
        // findings. Fall back to slow_system if the coding prompt
        // wasn't loaded (fresh install pre-setup.sh), noisily — the
        // analysis prompt will still produce something, just not a
        // useful code artifact.  Analysis and Generic share
        // slow_system; the difference between them is handled at the
        // dispatch level (lens fan-out vs single call), not in the
        // per-call system prompt.
        let slow_system_for_call = match ctx.mode {
            kres_core::TaskMode::Coding => {
                if self.slow_coding_system.is_some() {
                    self.slow_coding_system.as_ref()
                } else {
                    kres_core::async_eprintln!(
                        "[slow] coding-mode task but no slow_coding_system loaded — falling back to analysis prompt"
                    );
                    self.slow_system.as_ref()
                }
            }
            kres_core::TaskMode::Generic => {
                if self.slow_generic_system.is_some() {
                    self.slow_generic_system.as_ref()
                } else {
                    // Fall back quietly — analysis prompt still
                    // produces reasonable output for most free-form
                    // questions, it just trends toward "audit"
                    // phrasing.
                    self.slow_system.as_ref()
                }
            }
            kres_core::TaskMode::Analysis => self.slow_system.as_ref(),
        };
        if let Some(s) = slow_system_for_call {
            cfg = cfg.with_system(s.clone());
        }
        if let Some(n) = self.slow_max_input_tokens {
            cfg = cfg.with_max_input_tokens(n);
        }
        if let Some(lg) = &self.logger {
            lg.log_code("user", &slow_logged, None, None);
        }
        kres_core::async_eprintln!(
            "[slow] sending payload: {}k chars ({}k cached prefix + {}k volatile; {} symbols, {} context, {} prev findings)",
            slow_logged.len() / 1000,
            slow_prefix.len() / 1000,
            slow_suffix.len() / 1000,
            trimmed_symbols.len(),
            trimmed_context.len(),
            trimmed_prev.len(),
        );
        let text = tokio::select! {
            _ = shutdown.cancelled() => {
                return Err(AgentError::Other("cancelled during slow call".into()));
            }
            r = self.slow_client.messages_streaming(&cfg, &messages) => {
                let resp = r.map_err(|e| AgentError::Other(e.to_string()))?;
                record_usage(&self.usage, "slow", &self.slow_model, &resp.usage);
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
                kres_core::async_eprintln!(
                    "[slow] reply: in={} out={} cache_read={} cache_create={} ({} chars)",
                    resp.usage.input_tokens,
                    resp.usage.output_tokens,
                    resp.usage.cache_read_input_tokens,
                    resp.usage.cache_creation_input_tokens,
                    t.len(),
                );
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
        // slow-code-agent.system.md:37 "a bug that exists only in
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
            "[slow] parsed: analysis {}k chars, {} finding(s), {} followup(s), strategy={:?}",
            slow_parsed.analysis.len() / 1000,
            slow_parsed.findings.len(),
            slow_parsed.followups.len(),
            slow_parsed.strategy,
        );
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
            kres_core::TaskMode::Analysis | kres_core::TaskMode::Generic => {
                (slow_parsed.findings, Vec::new(), Vec::new())
            }
            kres_core::TaskMode::Coding => (
                Vec::new(),
                slow_parsed.code_output,
                slow_parsed.code_edits,
            ),
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
               severity (low|medium|high|critical), status ('active'), \
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
        if let Some(n) = self.fast_max_input_tokens {
            cfg = cfg.with_max_input_tokens(n);
        }
        if let Some(lg) = &self.logger {
            lg.log_code("user", &user_content, None, None);
        }
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

impl Orchestrator {
    /// Run a task with N parallel slow-agent lens calls over the
    /// same gathered symbols/context, then consolidate.
    ///
    /// Closes bugs.md#H4 lens handling: a lens call that errors
    /// yields an empty lens output rather than failing the whole
    /// task, but its failure shows up in the returned per-lens
    /// errors list so the operator can see it.
    pub async fn run_with_lenses(
        &self,
        prompt: &str,
        lenses: &[LensSpec],
        consolidator: &ConsolidatorClient,
        ctx: &RunContext,
        shutdown: &Shutdown,
    ) -> Result<TaskSummary, AgentError> {
        if lenses.is_empty() {
            return self.run_once_with_ctx(prompt, ctx, shutdown).await;
        }
        let composed = prepend_original_prompt(prompt, &ctx.original_prompt);
        let prompt: &str = composed.as_str();
        // Gather once via fast+main (same loop as run_once, up to the
        // point where we'd call the slow agent).
        let (symbols, context, fast_rounds, live_skills) =
            self.gather(prompt, ctx.plan.as_ref(), shutdown).await?;

        // Fan out N slow-agent calls in parallel.
        let mut futures = Vec::with_capacity(lenses.len());
        for (idx, lens) in lenses.iter().enumerate() {
            // §20b: send identity-only lens descriptors to the slow
            // agent. Matches
            let parallel_lenses = json!({
                "your_lens": lens_identity(lens),
                "other_lenses": lenses
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != idx)
                    .map(|(_, l)| lens_identity(l))
                    .collect::<Vec<_>>(),
            });
            // §20a: in-prose "Apply this lens" imperative so the slow
            // agent doesn't have to infer the lens angle from the
            // parallel_lenses JSON alone.
            let lens_prompt_line = format!("[{}] {}", lens.kind, lens.name);
            let mut lens_extra = format!("Apply this lens to your analysis:\n{lens_prompt_line}");
            if !lens.reason.is_empty() {
                lens_extra.push_str(&format!("\n(why: {})", lens.reason));
            }
            let lens_prompt = format!("{prompt}\n\n{lens_extra}");
            // bugs.md#L5: same trim applies in the lens path.
            let trimmed_prev = shrink_findings_to_budget(&ctx.previous_findings, 1_000_000);
            let trimmed_symbols = shrink_json_list_to_budget(&symbols, 1_000_000);
            let trimmed_context = shrink_json_list_to_budget(&context, 1_000_000);
            let mut lens_cp = CodePrompt::new(&lens_prompt)
                .with_symbols(&trimmed_symbols)
                .with_context(&trimmed_context)
                .with_previous_findings(&trimmed_prev)
                .with_parallel_lenses(&parallel_lenses);
            // §cache: include skills in the lens prompt — same
            // rationale as the single slow call above. Use the
            // post-gather `live_skills` so any skill files the
            // fast agent pulled in mid-gather reach the lens slow
            // agents too.
            if let Some(sk) = &live_skills {
                lens_cp = lens_cp.with_skills(sk);
            }
            if let Some(ref p) = ctx.plan {
                lens_cp = lens_cp.with_plan(p);
            }
            let (lens_prefix, lens_suffix) = lens_cp.to_cached_split_json(CACHED_PREFIX_FIELDS)?;
            let client = self.slow_client.clone();
            let model = self.slow_model.clone();
            let system = self.slow_system.clone();
            let max_tokens = self.slow_max_tokens;
            let max_input_tokens = self.slow_max_input_tokens;
            let shutdown_c = shutdown.clone();
            let usage = self.usage.clone();
            let logger = self.logger.clone();
            let lens_label = format!("lens {}", lens.name);
            futures.push(async move {
                let messages = vec![Message {
                    role: "user".into(),
                    content: lens_suffix,
                    cache: true,
                    cached_prefix: if lens_prefix.is_empty() {
                        None
                    } else {
                        Some(lens_prefix)
                    },
                }];
                let mut cfg = CallConfig::defaults_for(model.clone())
                    .with_max_tokens(max_tokens)
                    .with_stream_label(lens_label);
                if let Some(s) = system {
                    cfg = cfg.with_system(s);
                }
                if let Some(n) = max_input_tokens {
                    cfg = cfg.with_max_input_tokens(n);
                }
                if let Some(lg) = &logger {
                    lg.log_code("user", &messages[0].content, None, None);
                }
                tokio::select! {
                    _ = shutdown_c.cancelled() => None,
                    r = client.messages_streaming(&cfg, &messages) => match r {
                        Ok(resp) => {
                            record_usage(&usage, "slow", &model, &resp.usage);
                            let t = extract_text(&resp);
                            if let Some(lg) = &logger {
                                let th = extract_thinking(&resp);
                                lg.log_code(
                                    "assistant",
                                    &t,
                                    Some(log_usage(&resp.usage)),
                                    th.as_deref(),
                                );
                            }
                            Some(parse_code_response(&t))
                        }
                        Err(e) => {
                            tracing::warn!(target: "kres_agents", "lens call failed: {e}");
                            None
                        }
                    }
                }
            });
        }
        let raws: Vec<Option<CodeResponse>> = join_all(futures).await;

        // Build LensOutputs for the consolidator. Preserve only
        // non-None results; a failed lens contributes nothing.
        // §20c: filter lenses that produced neither analysis nor
        // findings so the consolidator doesn't have to handle noise.
        // §20e: per-lens descriptor is the reduced identity form, not
        // the full LensSpec.
        let lens_json: Vec<Value> = lenses.iter().map(lens_identity).collect();
        let mut outs: Vec<LensOutput<'_>> = Vec::new();
        let mut all_followups: Vec<Followup> = Vec::new();
        for (i, raw) in raws.iter().enumerate() {
            if let Some(parsed) = raw {
                if parsed.analysis.is_empty() && parsed.findings.is_empty() {
                    continue;
                }
                outs.push(LensOutput {
                    lens: &lens_json[i],
                    analysis: &parsed.analysis,
                    findings: &parsed.findings,
                });
                all_followups.extend(parsed.followups.iter().cloned());
            }
        }

        let consolidated = consolidate_lenses_with_logger(
            consolidator.client.clone(),
            consolidator.model.clone(),
            consolidator.system.as_deref(),
            consolidator.max_tokens,
            consolidator.max_input_tokens,
            &ctx.task_brief,
            &outs,
            self.logger.clone(),
        )
        .await?;
        Ok(TaskSummary {
            analysis: consolidated.analysis,
            findings: consolidated.findings,
            followups: all_followups,
            fast_rounds,
            strategy: ParseStrategy::WholeBody,
            mode: kres_core::TaskMode::Analysis,
            code_output: Vec::new(),
            code_edits: Vec::new(),
            // Lens fan-out runs N parallel slow calls; merging N
            // plan rewrites would churn step ids. Analysis-mode
            // plan rewrites flow through the todo-agent's per-turn
            // reevaluation path (a97bff2) instead. Single-slow
            // analysis tasks (lens count 0) still get plan rewrite
            // via run_once_with_ctx above.
            plan: None,
        })
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
            if let Some(s) = &self.fast_system {
                cfg = cfg.with_system(s.clone());
            }
            if let Some(n) = self.fast_max_input_tokens {
                cfg = cfg.with_max_input_tokens(n);
            }
            if let Some(lg) = &self.logger {
                lg.log_code("user", &messages[0].content, None, None);
            }
            let text = tokio::select! {
                _ = shutdown.cancelled() => return Err(AgentError::Other("cancelled during fast call".into())),
                r = self.fast_client.messages_streaming(&cfg, &messages) => {
                    let resp = r.map_err(|e| AgentError::Other(e.to_string()))?;
                    record_usage(&self.usage, "fast", &self.fast_model, &resp.usage);
                    let t = extract_text(&resp);
                    if let Some(lg) = &self.logger {
                        let th = extract_thinking(&resp);
                        lg.log_code(
                            "assistant",
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
            if !parsed.followups.is_empty()
                && parsed.followups.iter().all(|f| f.kind == "question")
            {
                kres_core::async_eprintln!(
                    "[fast gather round {}] only type:question followups — breaking",
                    fast_rounds
                );
                break;
            }
            let fetched = tokio::select! {
                _ = shutdown.cancelled() => return Err(AgentError::Other("cancelled during fetch".into())),
                f = self.fetcher.fetch(&parsed.followups, plan) => f?,
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
            Err(e) => {
                tracing::warn!(
                    target: "kres_agents",
                    path,
                    "skill_read failed: {e}"
                );
                // Surface the error back to the agent so it can
                // adjust — an empty string would be silently
                // misleading.
                files_map.insert(
                    path.clone(),
                    Value::String(format!("[skill_read failed: {e}]")),
                );
            }
        }
    }
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

/// Cut a string to `n` chars with an ellipsis. Used by the verbose
/// orchestrator printouts so a long followup name doesn't flood the
/// REPL line.
fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let head: String = s.chars().take(n).collect();
    format!("{head}…")
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
                }],
                None,
            )
            .await
            .unwrap();
        assert!(r.symbols.is_empty());
        assert!(r.context.is_empty());
    }

    /// Unit-test the loop-back decision table. We can't easily run
    /// the whole orchestrator without a live API, but we can test
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
        // Orchestrator's decision: only_skill_reads = true, so loop back.
        let only_skill_reads =
            r.followups.is_empty() && !r.ready_for_slow && !r.skill_reads.is_empty();
        assert!(only_skill_reads);
    }

    #[test]
    fn parse_empty_triggers_slow_handoff() {
        // No followups, no skill_reads, not ready — the orchestrator
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
    /// can't produce data for any of them, so the orchestrator
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
        let dir = std::env::temp_dir().join(format!(
            "kres-apply-skill-reads-{}",
            std::process::id()
        ));
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
}
