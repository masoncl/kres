//! Goal system: define + check the task's completion criteria.
//!
//! Ports / `_check_goal` (/:
//! 3304-3343, 3346-3392). These run on the SAME model and client as
//! the main (data-dispatch) agent, and log to `main.jsonl` — but
//! they DO NOT share the main-agent's system prompt. The main-agent
//! prompt trains the model to reply `done` when no fetch actions
//! are needed; that trained reflex was firing on check_goal calls
//! and blowing past the JSON envelope the caller expects (observed
//! in session e84c7fac: reply=`done`, parse failed,
//! the old fail-open completion fallback fired). [`GOAL_INSTRUCTIONS`] is the dedicated
//! system prompt for this agent — it tells the model it's a judge,
//! not a fetcher, and to return raw, unfenced JSON only.
//!
//! Ownership: the session calls `define_goal` after each top-level
//! prompt (or `--prompt FILE` initial run), stores the returned
//! `goal` + `mode` plus (via `define_plan`) the resulting `Plan`,
//! then calls `check_goal` (with the current plan attached) after
//! every reaped task. When the goal is met, the session moves all
//! remaining pending todos to the deferred list.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::json;

use kres_core::log::{LoggedUsage, TurnLogger};
use kres_core::UsageTracker;
use kres_llm::{client::Client, config::CallConfig, request::Message, Model};

/// Dedicated system prompt for define_goal / check_goal. Swapped in
/// for the main-agent's fetcher system prompt so the goal judge
/// doesn't reply `done` to JSON-shaped requests.
pub const GOAL_INSTRUCTIONS: &str = include_str!("prompts/goal.txt");

/// Config needed to run a goal call. Same shape as the main-agent
/// client — reuses the main-agent's client for both.
#[derive(Clone)]
pub struct GoalClient {
    pub client: Arc<Client>,
    pub model: Model,
    pub system: Option<String>,
    pub max_tokens: u32,
    pub max_input_tokens: Option<u32>,
    pub logger: Option<Arc<TurnLogger>>,
    pub usage: Option<Arc<UsageTracker>>,
}

fn record_usage(gc: &GoalClient, usage: &kres_llm::request::Usage) {
    if let Some(tracker) = &gc.usage {
        tracker.record(
            "goal",
            gc.model.id.clone(),
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_creation_input_tokens,
            usage.cache_read_input_tokens,
        );
    }
}

async fn call_with_shutdown(
    gc: &GoalClient,
    cfg: &CallConfig,
    messages: &[Message],
    shutdown: Option<kres_core::Shutdown>,
) -> Result<kres_llm::request::MessagesResponse, kres_llm::LlmError> {
    if let Some(shutdown) = shutdown {
        tokio::select! {
            _ = shutdown.cancelled() => Err(kres_llm::LlmError::Other("cancelled".into())),
            result = gc.client.messages_streaming(cfg, messages) => result,
        }
    } else {
        gc.client.messages_streaming(cfg, messages).await
    }
}

/// Result of a `check_goal` call.
#[derive(Debug, Clone)]
pub struct GoalCheck {
    /// Whether the goal is considered met by the analysis so far.
    pub met: bool,
    pub reason: String,
    pub missing: Vec<String>,
}

/// Result of a `define_goal` call: the completion criterion + the
/// classified work mode ("audit" for defect review, "generic" for
/// free-form questions, "coding" for writing files).
#[derive(Debug, Clone)]
pub struct GoalDefinition {
    pub goal: String,
    pub mode: TaskMode,
}

pub use kres_core::TaskMode;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct DefineResponse {
    goal: String,
    mode: Option<TaskMode>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct CheckResponse {
    met: bool,
    reason: String,
    missing: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct PlanStepRaw {
    #[serde(default)]
    id: String,
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    depends_on: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct PlanResponse {
    steps: Vec<PlanStepRaw>,
}

/// Ask the main agent for a completion criterion. Returns None when
/// the agent fails to produce a well-shaped response — callers
/// should treat "no goal" as "run until --turns or the todo list
/// drains" and NOT invoke `check_goal` ( behaviour).
///
/// `plan` is the manager's current plan, when one exists. Forwarded
/// to the agent so per-task goals derived from pipeline-driven
/// follow-up prompts can be framed in terms of which plan step the
/// task is serving. Without it, `define_goal` sees only the bare
/// follow-up query and produces goals that read like isolated
/// sub-questions — the downstream check_goal / todo_update path
/// then has no handle to attribute the completed work back to its
/// parent step, so step status stays `pending` even after
/// substantial exploration (observed on the RCU overnight run:
/// ~1.1k `tasks.h`/`rcu_tasks` mentions in code.jsonl, yet the
/// `audit-rcu-tasks` step stayed pending in session.json).
fn build_define_goal_request(prompt: &str, plan: Option<&kres_core::Plan>) -> serde_json::Value {
    let mut request = json!({
        "task": "define_goal",
        "query": prompt.chars().take(2000).collect::<String>(),
        "instructions": "Define a clear, specific goal for this query \
                         AND classify the work mode. What must be true \
                         for this query to be considered complete? Be \
                         concrete: name specific things that must be \
                         found, verified, written, or answered. The \
                         `mode` field selects the pipeline:\n\
                         - \"audit\" — the DEFECT-REVIEW flow: \
                           multi-angle audit, lens fan-out, \
                           consolidator + findings pipeline. Pick \
                           ONLY when the operator asked to find or \
                           review bugs / defects / correctness \
                           issues in a target. An \"efficiency \
                           review\", a \"design review\", or any \
                           non-defect assessment does NOT belong \
                           here.\n\
                         - \"generic\" — one slow-agent call per task \
                           over the fast/main/slow/goal loop, no lens \
                           fan-out. Pick for free-form questions \
                           (\"explain\", \"what does X do\", \"trace \
                           path from A to B\"), efficiency / \
                           performance reviews, design-intent \
                           investigations, and any narrow prompt \
                           whose output is prose rather than files \
                           or defect findings.\n\
                         - \"coding\" — write files (source code for \
                           reproducers / PoCs / selftests / triggers \
                           / harnesses, OR prose documents such as \
                           markdown reports to an operator-named \
                           path). Pick when the REQUESTED OUTPUT is \
                           a file on disk — source the operator will \
                           run, or a document like \
                           `./suggestions.md` they asked to be \
                           written.\n\
                         Default to \"generic\" when the prompt is \
                         ambiguous — it's the cheapest analytical \
                         path.\n\
                         When a `plan` field is present, use it as \
                         scoping context only. If the query genuinely \
                         overlaps one of the plan's steps, phrase the \
                         goal in terms of that step (and include its \
                         step id in the goal prose) so satisfying the \
                         goal advances a named step. If the query is a \
                         new topic with no clear plan-step overlap, \
                         ignore the plan and frame the goal on the \
                         query's own terms — do not force-fit an \
                         unrelated prompt into an existing step.\n\
                         Treat tool names, workflow names, and output \
                         schema names in the prompt as infrastructure, \
                         not as the codebase under review. For example, \
                         a request for Finding records names the output \
                         format; it does not imply the target project. \
                         If the target is a git ref such as HEAD, scope \
                         the goal to the changes introduced by that ref \
                         in the current workspace repository, not to a \
                         whole-tree audit unless the prompt explicitly \
                         asks for one.\n\
                         Return raw, unfenced JSON only:\n\
                         {\"goal\": \"specific completion criteria\", \
                          \"mode\": \"audit\" | \"generic\" | \"coding\"}"
    });
    if let Some(p) = plan {
        if let Ok(v) = serde_json::to_value(p) {
            request
                .as_object_mut()
                .expect("request is an object literal")
                .insert("plan".into(), v);
        }
    }
    request
}

pub async fn define_goal(
    gc: &GoalClient,
    prompt: &str,
    plan: Option<&kres_core::Plan>,
    shutdown: Option<kres_core::Shutdown>,
) -> Option<GoalDefinition> {
    let request = build_define_goal_request(prompt, plan);
    let body = serde_json::to_string_pretty(&request).ok()?;
    let mut cfg = CallConfig::defaults_for(gc.model.clone())
        .with_max_tokens(gc.max_tokens)
        .with_stream_label("define_goal");
    if let Some(s) = &gc.system {
        cfg = cfg.with_system(s.clone());
    }
    if let Some(n) = gc.max_input_tokens {
        cfg = cfg.with_max_input_tokens(n);
    }
    // define_goal is one-shot per prompt — tail cache would never
    // be read. Skip the +25% write tax.
    let messages = vec![Message {
        role: "user".into(),
        content: body.clone(),
        cache: false,
        cached_prefix: None,
    }];
    if let Some(lg) = &gc.logger {
        lg.log_main("user", &body, None, None);
    }
    let resp = match call_with_shutdown(gc, &cfg, &messages, shutdown.clone()).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(target: "kres_agents", "define_goal failed: {e}");
            return None;
        }
    };
    record_usage(gc, &resp.usage);
    let text = extract_text(&resp);
    if let Some(lg) = &gc.logger {
        lg.log_main(
            "assistant",
            &text,
            Some(LoggedUsage {
                input: resp.usage.input_tokens,
                output: resp.usage.output_tokens,
                cache_creation: resp.usage.cache_creation_input_tokens,
                cache_read: resp.usage.cache_read_input_tokens,
            }),
            None,
        );
    }
    let parsed =
        parse_or_repair_goal_json::<DefineResponse>(gc, &text, "define-goal", "goal", shutdown)
            .await?;
    if parsed.goal.is_empty() {
        return None;
    }
    Some(GoalDefinition {
        goal: parsed.goal,
        mode: parsed.mode.unwrap_or_default(),
    })
}

/// Ask the main agent whether `goal` has been met by `analysis`.
/// Returns `(met, reason, missing)`. Failures are fail-closed: they return a
/// non-complete check with an explicit retry item so malformed JSON or a flaky
/// call cannot drain pending work.
///
/// `original_prompt` is the operator's raw query. Including it as a
/// separate field lets the judge weigh the literal intent ("check
/// every file one by one") against the derived `goal` string that
/// may have compressed or generalised that intent during
/// define_goal.
fn build_check_goal_request(
    original_prompt: &str,
    goal: &str,
    analysis: &str,
    plan: Option<&kres_core::Plan>,
) -> serde_json::Value {
    let mut request = json!({
        "task": "check_goal",
        "original_prompt": original_prompt,
        "goal": goal,
        "analysis": analysis,
        "instructions": "Has the analysis satisfied BOTH the operator's \
                         original_prompt AND the derived goal? The goal is \
                         a summary the main agent produced from the prompt; \
                         treat the prompt as the ground-truth intent. If \
                         the prompt asks for a sweep (e.g. 'check every \
                         file', 'analyse each function') and the analysis \
                         only covers the first item, that is NOT met — \
                         list the remaining items in `missing`. When a \
                         `plan` field is present, use it as a checklist: \
                         treat the goal as unmet when concrete, untouched \
                         plan steps still apply to this prompt. In audit \
                         mode, negative coverage claims require evidence. \
                         Do not accept claims such as no remaining users, \
                         all callers updated, old path unreachable, only \
                         reader, or only writer unless the analysis cites \
                         concrete source, type, search, caller/callee, or history \
                         evidence. If a plan step says to trace unchanged \
                         callers, callees, callbacks, readers, writers, \
                         shared helpers, or old-contract users, and the \
                         analysis only asserts that this was checked without \
                         naming the concrete evidence, set met=false and \
                         put the missing source/type/search/callgraph/history \
                         item in `missing`. Return raw, unfenced JSON only:\n\
                         {\"met\": true/false, \"reason\": \"why or why not\", \
                         \"missing\": [\"what still needs to be done\"]}"
    });
    if let Some(p) = plan {
        if let Ok(v) = serde_json::to_value(p) {
            request
                .as_object_mut()
                .expect("request is an object literal")
                .insert("plan".into(), v);
        }
    }
    request
}

pub async fn check_goal(
    gc: &GoalClient,
    original_prompt: &str,
    goal: &str,
    analysis: &str,
    plan: Option<&kres_core::Plan>,
    shutdown: Option<kres_core::Shutdown>,
) -> GoalCheck {
    let request = build_check_goal_request(original_prompt, goal, analysis, plan);
    let body = match serde_json::to_string_pretty(&request) {
        Ok(s) => s,
        Err(_) => return failed_goal_check("could not serialize goal-check request"),
    };
    let mut cfg = CallConfig::defaults_for(gc.model.clone())
        .with_max_tokens(gc.max_tokens)
        .with_stream_label("check_goal");
    if let Some(s) = &gc.system {
        cfg = cfg.with_system(s.clone());
    }
    if let Some(n) = gc.max_input_tokens {
        cfg = cfg.with_max_input_tokens(n);
    }
    // check_goal is one-shot per completed task — no reader for a
    // tail cache. Skip the +25% write tax.
    let messages = vec![Message {
        role: "user".into(),
        content: body.clone(),
        cache: false,
        cached_prefix: None,
    }];
    if let Some(lg) = &gc.logger {
        lg.log_main("user", &body, None, None);
    }
    let resp = match call_with_shutdown(gc, &cfg, &messages, shutdown.clone()).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(target: "kres_agents", "check_goal failed: {e}");
            return failed_goal_check(&format!("goal-check request failed: {e}"));
        }
    };
    record_usage(gc, &resp.usage);
    let text = extract_text(&resp);
    if let Some(lg) = &gc.logger {
        lg.log_main(
            "assistant",
            &text,
            Some(LoggedUsage {
                input: resp.usage.input_tokens,
                output: resp.usage.output_tokens,
                cache_creation: resp.usage.cache_creation_input_tokens,
                cache_read: resp.usage.cache_read_input_tokens,
            }),
            None,
        );
    }
    match parse_or_repair_goal_json::<CheckResponse>(gc, &text, "check-goal", "met", shutdown).await
    {
        Some(r) => GoalCheck {
            met: r.met,
            reason: r.reason,
            missing: r.missing,
        },
        None => failed_goal_check("goal-check response was malformed and could not be repaired"),
    }
}

fn failed_goal_check(reason: &str) -> GoalCheck {
    GoalCheck {
        met: false,
        reason: reason.into(),
        missing: vec!["retry the completion check".into()],
    }
}

/// Ask the goal agent for a concrete decomposition of `prompt` into
/// ordered steps. Returns `None` on any failure — callers treat "no
/// plan" as "the pipeline runs the usual loop with no pre-staged
/// plan", which is the behaviour from before plans existed.
///
/// The returned [`kres_core::Plan`] is stored on the manager via
/// `set_plan`; the session persistence layer writes it into
/// `session.json` on every reaper tick, `/plan` displays it, and
/// every downstream agent sees it: fast + slow via `CodePrompt`,
/// main via `DataFetcher::fetch`, goal-judge via `check_goal`,
/// todo-agent via `update_todo_via_agent`. The first slow call and
/// every todo-agent turn may return a rewritten plan that swaps in
/// via `set_plan`.
pub async fn define_plan(
    gc: &GoalClient,
    prompt: &str,
    goal: &str,
    mode: TaskMode,
    existing: Option<&kres_core::Plan>,
    shutdown: Option<kres_core::Shutdown>,
) -> Option<kres_core::Plan> {
    let mut request = json!({
        "task": "define_plan",
        "original_prompt": prompt,
        "goal": goal,
        "mode": mode,
        "instructions": "Decompose the original prompt + derived goal \
                         into 3-12 ordered concrete steps. Every \
                         title names a specific file, symbol, \
                         subsystem, code path, or artifact. In \
                         audit mode, decompose by file / symbol / \
                         subsystem — NOT by lens (object lifetime, \
                         memory, bounds, races, general correctness). \
                         Those lenses already run on every slow call; \
                         restating them as plan steps produces a \
                         useless plan. Keep titles imperative, \
                         <= 80 chars; descriptions one-to-two \
                         sentences. IDs must be unique kebab-case \
                         SLUGS that describe the work (e.g. \
                         `audit-ring-buffer-init`, \
                         `walk-sqpoll-thread-path`), NOT positional \
                         tags like s1/s2. Semantic ids survive \
                         reordering and later rewrites because they \
                         name what the step DOES; positional tags \
                         get accidentally reassigned to unrelated \
                         steps. When an `existing_plan` field is \
                         present and the new prompt is a \
                         continuation / refinement of the same work, \
                         KEEP existing step ids that still apply and \
                         add/edit steps only where the new prompt \
                         demands it. Preserve step ids verbatim \
                         whenever the step's intent survives — \
                         churning ids orphans todos that were \
                         pointing at them. Only produce a wholly \
                         fresh plan when the new prompt is clearly \
                         a different topic. Treat tool names, workflow \
                         names, and schema names in the prompt as \
                         infrastructure, not as source-tree names. If \
                         the target is HEAD or another git ref, first \
                         decompose around files, symbols, or subsystems \
                         in the current workspace repository's target \
                         diff/stat; do not add a repo-survey step or \
                         whole-tree audit unless the prompt explicitly \
                         asks for one. Do not invent a project layout \
                         from the tool/schema wording. Return raw, unfenced JSON only:\n\
                         {\"steps\": [{\"id\": \"audit-...\", \"title\": \"...\", \
                         \"description\": \"...\"}]}"
    });
    if let Some(p) = existing {
        if let Ok(v) = serde_json::to_value(p) {
            request
                .as_object_mut()
                .expect("request is an object literal")
                .insert("existing_plan".into(), v);
        }
    }
    let body = serde_json::to_string_pretty(&request).ok()?;
    let mut cfg = CallConfig::defaults_for(gc.model.clone())
        .with_max_tokens(gc.max_tokens)
        .with_stream_label("define_plan");
    if let Some(s) = &gc.system {
        cfg = cfg.with_system(s.clone());
    }
    if let Some(n) = gc.max_input_tokens {
        cfg = cfg.with_max_input_tokens(n);
    }
    // define_plan is one-shot per top-level prompt — tail cache
    // would never be read. Skip the +25% write tax.
    let messages = vec![Message {
        role: "user".into(),
        content: body.clone(),
        cache: false,
        cached_prefix: None,
    }];
    if let Some(lg) = &gc.logger {
        lg.log_main("user", &body, None, None);
    }
    let resp = match call_with_shutdown(gc, &cfg, &messages, shutdown.clone()).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(target: "kres_agents", "define_plan failed: {e}");
            return None;
        }
    };
    record_usage(gc, &resp.usage);
    let text = extract_text(&resp);
    if let Some(lg) = &gc.logger {
        lg.log_main(
            "assistant",
            &text,
            Some(LoggedUsage {
                input: resp.usage.input_tokens,
                output: resp.usage.output_tokens,
                cache_creation: resp.usage.cache_creation_input_tokens,
                cache_read: resp.usage.cache_read_input_tokens,
            }),
            None,
        );
    }
    let parsed =
        parse_or_repair_goal_json::<PlanResponse>(gc, &text, "define-plan", "steps", shutdown)
            .await?;
    if parsed.steps.is_empty() {
        return None;
    }
    let plan = build_plan_from_raw(parsed.steps, prompt, goal, mode);
    if plan.steps.is_empty() {
        return None;
    }
    Some(plan)
}

/// Build a [`kres_core::Plan`] from a vector of raw `PlanStepRaw`
/// DTOs. Split out from `define_plan` so the id-synthesis logic can
/// be unit-tested without a live goal client. Delegates the actual
/// synthesis + empty-title filtering to
/// [`kres_core::plan::normalize_steps`]; this function only maps
/// the wire DTO into the core [`kres_core::PlanStep`] shape before
/// normalisation.
fn build_plan_from_raw(
    raw: Vec<PlanStepRaw>,
    prompt: &str,
    goal: &str,
    mode: TaskMode,
) -> kres_core::Plan {
    let steps: Vec<kres_core::PlanStep> = raw
        .into_iter()
        .map(|r| {
            let mut s = kres_core::PlanStep::new(r.id, r.title);
            s.description = r.description;
            s.depends_on = r.depends_on;
            s
        })
        .collect();
    let mut plan = kres_core::Plan::new(prompt, goal, mode);
    plan.steps = kres_core::plan::normalize_steps(steps);
    plan
}

/// Find the first `{...}` block containing the requested key and
/// deserialise it into `T`. Matches (text, key)`
/// for the narrow "expect a JSON object with this field" case.
#[cfg(test)]
fn extract_json_with_key<T: for<'de> Deserialize<'de>>(text: &str, key: &str) -> Option<T> {
    crate::json_repair::JsonObjectContract {
        name: "goal-agent",
        fields: &[key],
    }
    .parse(text)
    .ok()
}

async fn parse_or_repair_goal_json<T: for<'de> Deserialize<'de> + schemars::JsonSchema>(
    gc: &GoalClient,
    text: &str,
    contract_name: &str,
    key: &str,
    shutdown: Option<kres_core::Shutdown>,
) -> Option<T> {
    let contract = crate::json_repair::JsonObjectContract {
        name: contract_name,
        fields: &[key],
    };
    let errors = match contract.parse(text) {
        Ok(parsed) => return Some(parsed),
        Err(errors) => errors,
    };
    let schema = serde_json::to_string(&schemars::schema_for!(T)).ok()?;
    let repaired = crate::json_repair::repair_json_response(crate::json_repair::JsonRepairCall {
        client: gc.client.clone(),
        model: gc.model.clone(),
        max_tokens: gc.max_tokens,
        max_input_tokens: gc.max_input_tokens,
        contract: crate::json_repair::JsonContract {
            name: contract_name,
            schema: &schema,
            instructions: "Correct representation and field types only; preserve the original decision and text.",
        },
        rejected_response: text,
        validation_errors: &errors,
        logger: gc.logger.clone(),
        log_kind: crate::json_repair::RepairLogKind::Main,
        shutdown,
    })
    .await
    .ok()?;
    record_usage(gc, &repaired.usage);
    contract.accept_repair(&repaired.text).ok()
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
    fn extract_json_strict() {
        let r: DefineResponse =
            extract_json_with_key(r#"{"goal": "find the bug"}"#, "goal").unwrap();
        assert_eq!(r.goal, "find the bug");
    }

    #[test]
    fn extract_json_rejects_embedded_object() {
        let response: Option<CheckResponse> = extract_json_with_key(
            r#"prefix {"met": true, "reason": "ok", "missing": []} suffix"#,
            "met",
        );
        assert!(response.is_none());
    }

    #[test]
    fn extract_json_rejects_multiple_candidates() {
        let text = r#"first {"goal":42} then {"goal":"real","mode":"audit"}"#;
        let response: Option<DefineResponse> = extract_json_with_key(text, "goal");
        assert!(response.is_none());
    }

    #[test]
    fn extract_json_missing_key() {
        let r: Option<DefineResponse> = extract_json_with_key(r#"{"other": "x"}"#, "goal");
        assert!(r.is_none());
    }

    #[test]
    fn control_responses_reject_missing_required_fields() {
        assert!(extract_json_with_key::<DefineResponse>("{}", "goal").is_none());
        assert!(extract_json_with_key::<CheckResponse>("{}", "met").is_none());
        assert!(extract_json_with_key::<PlanResponse>("{}", "steps").is_none());
        assert!(extract_json_with_key::<CheckResponse>(r#"{"met":false}"#, "met").is_none());
    }

    #[test]
    fn missing_mode_field_defaults_to_generic_via_unwrap_or_default() {
        // Parse a classifier reply that omits the `mode` field.
        // DefineResponse::mode is Option<TaskMode> with serde default
        // = None, and define_goal's caller unwraps via
        // parsed.mode.unwrap_or_default(). That must resolve to
        // Generic — matches goal.txt's "Default to 'generic' when
        // ambiguous" policy.
        let r: DefineResponse =
            extract_json_with_key(r#"{"goal": "audit target for efficiency"}"#, "goal").unwrap();
        assert!(r.mode.is_none(), "mode field absent in reply");
        assert_eq!(r.mode.unwrap_or_default(), kres_core::TaskMode::Generic);
    }

    #[test]
    fn unparseable_mode_string_drops_whole_reply_outer_fallback_handles() {
        // Classifier hallucinates a mode that doesn't match the
        // three-variant enum. serde rename_all=lowercase rejects
        // the string, which fails the whole DefineResponse parse
        // (not just the one field). extract_json_with_key returns
        // None, and the caller in session.rs handles that via
        // `None => (None, TaskMode::default())`. With the
        // default=Generic pinned test above, the outer observable
        // behaviour is still "fall back to Generic". Documented
        // here so a future change to the deserialize policy
        // (e.g. tolerating unknown modes) doesn't silently break
        // the outer fallback.
        let r: Option<DefineResponse> =
            extract_json_with_key(r#"{"goal": "check x", "mode": "investigation"}"#, "goal");
        assert!(r.is_none(), "unparseable mode collapses entire reply");
    }

    #[test]
    fn failed_goal_check_does_not_claim_completion() {
        let c = failed_goal_check("bad response");
        assert!(!c.met);
        assert!(!c.missing.is_empty());
    }

    fn sample_plan() -> kres_core::Plan {
        // Build via JSON round-trip so this module doesn't need a
        // chrono dev-dependency just for the created_at field.
        serde_json::from_value(json!({
            "prompt": "review rcu",
            "goal": "enumerate rcu bugs",
            "mode": "audit",
            "steps": [{"id": "audit-rcu-tree-core", "title": "tree.c"}],
            "created_at": "2026-04-23T12:00:00Z",
        }))
        .expect("sample_plan JSON is well-formed")
    }

    #[test]
    fn define_goal_request_embeds_plan_when_some() {
        let plan = sample_plan();
        let r = build_define_goal_request("tree.c CPU hotplug", Some(&plan));
        let obj = r.as_object().unwrap();
        assert_eq!(
            obj.get("task").and_then(|v| v.as_str()),
            Some("define_goal")
        );
        let plan_v = obj.get("plan").expect("plan should be embedded");
        assert_eq!(
            plan_v.get("prompt").and_then(|v| v.as_str()),
            Some("review rcu"),
        );
        let step0 = plan_v
            .get("steps")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .unwrap();
        assert_eq!(
            step0.get("id").and_then(|v| v.as_str()),
            Some("audit-rcu-tree-core"),
        );
    }

    #[test]
    fn define_goal_request_omits_plan_when_none() {
        let r = build_define_goal_request("first prompt, no plan yet", None);
        assert!(
            r.as_object().unwrap().get("plan").is_none(),
            "plan key should be absent when caller passes None",
        );
    }

    #[test]
    fn define_goal_request_treats_schema_names_as_infrastructure() {
        let r = build_define_goal_request("review HEAD and emit Finding records", None);
        let instructions = r
            .get("instructions")
            .and_then(|v| v.as_str())
            .expect("instructions present");
        assert!(instructions.contains("schema names"));
        assert!(instructions.contains("current workspace repository"));
        assert!(instructions.contains("does not imply the target project"));
        assert!(instructions.contains("not to a whole-tree audit"));
    }

    #[test]
    fn check_goal_request_rejects_unsupported_negative_coverage_claims() {
        let plan = sample_plan();
        let r = build_check_goal_request(
            "review target for concrete bugs",
            "audit target",
            "analysis says old path unreachable",
            Some(&plan),
        );
        let instructions = r
            .get("instructions")
            .and_then(|v| v.as_str())
            .expect("instructions present");
        assert!(instructions.contains("negative coverage claims require evidence"));
        assert!(instructions.contains("all callers updated"));
        assert!(instructions.contains("source, type, search, caller/callee, or history"));
        assert!(instructions.contains("set met=false"));
        assert!(
            r.as_object().unwrap().get("plan").is_some(),
            "plan key should be present for checklist-based goal judging",
        );
    }

    #[test]
    fn extract_plan_response_with_missing_ids() {
        // The id-synthesis path lives inline in `define_plan`; unit
        // test the JSON parse here to make sure the DTO accepts
        // missing id / description fields.
        let r: PlanResponse = extract_json_with_key(
            r#"{"steps": [{"title": "step1"}, {"id": "", "title": "step2"}]}"#,
            "steps",
        )
        .unwrap();
        assert_eq!(r.steps.len(), 2);
        assert_eq!(r.steps[0].id, "");
        assert_eq!(r.steps[0].title, "step1");
    }

    #[test]
    fn extract_plan_response_rejects_goal_shaped_reply() {
        // A goal.txt-shaped reply does NOT contain "steps"; brace
        // matcher returns None so the caller falls back to "no plan".
        let r: Option<PlanResponse> =
            extract_json_with_key(r#"{"goal": "x", "mode": "audit"}"#, "steps");
        assert!(r.is_none());
    }

    fn step_raw(id: &str, title: &str) -> PlanStepRaw {
        PlanStepRaw {
            id: id.into(),
            title: title.into(),
            description: String::new(),
            depends_on: Vec::new(),
        }
    }

    #[test]
    fn build_plan_preserves_agent_ids_when_unique() {
        let plan = build_plan_from_raw(
            vec![step_raw("s1", "one"), step_raw("s2", "two")],
            "prompt",
            "goal",
            TaskMode::Audit,
        );
        assert_eq!(plan.steps.len(), 2);
        assert_eq!(plan.steps[0].id, "s1");
        assert_eq!(plan.steps[1].id, "s2");
    }

    #[test]
    fn build_plan_synthesises_slug_ids_when_empty() {
        let plan = build_plan_from_raw(
            vec![
                step_raw("", "Audit ring buffer init"),
                step_raw("", "Walk IO_WQ cancel path"),
            ],
            "prompt",
            "goal",
            TaskMode::Audit,
        );
        assert_eq!(plan.steps.len(), 2);
        // Semantic slugs — survive reorder because they name the
        // step rather than its position.
        assert_eq!(plan.steps[0].id, "audit-ring-buffer-init");
        assert_eq!(plan.steps[1].id, "walk-io-wq-cancel-path");
    }

    #[test]
    fn build_plan_resolves_id_collisions_via_suffix() {
        let plan = build_plan_from_raw(
            vec![
                step_raw("audit-foo", "Audit foo"),
                step_raw("audit-foo", "Audit bar"),
                step_raw("audit-foo", "Audit baz"),
            ],
            "prompt",
            "goal",
            TaskMode::Audit,
        );
        // The first keeps its id; the later two get slugs derived
        // from their own titles rather than being forced onto the
        // same slug with a suffix (which would lose semantic
        // meaning).
        assert_eq!(plan.steps.len(), 3);
        assert_eq!(plan.steps[0].id, "audit-foo");
        assert_eq!(plan.steps[1].id, "audit-bar");
        assert_eq!(plan.steps[2].id, "audit-baz");
    }

    #[test]
    fn build_plan_slug_collision_falls_back_to_numeric_suffix() {
        // Agent-provided id duplicates what slugify would produce
        // for a later row. The synthesiser's title-based slug is
        // already claimed; walking `-N` must reach a free slot.
        let plan = build_plan_from_raw(
            vec![
                step_raw("audit-same", "Audit unrelated first"),
                step_raw("", "Audit same"),
            ],
            "prompt",
            "goal",
            TaskMode::Audit,
        );
        assert_eq!(plan.steps.len(), 2);
        assert_eq!(plan.steps[0].id, "audit-same");
        assert_eq!(plan.steps[1].id, "audit-same-2");
    }

    #[test]
    fn build_plan_skips_empty_titles_without_eating_id_slots() {
        // An empty-title row must not reserve its id before we
        // filter it out.
        let plan = build_plan_from_raw(
            vec![step_raw("audit-kept", ""), step_raw("", "Audit kept")],
            "prompt",
            "goal",
            TaskMode::Audit,
        );
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].id, "audit-kept");
        assert_eq!(plan.steps[0].title, "Audit kept");
    }

    #[test]
    fn build_plan_all_empty_titles_yields_no_steps() {
        let plan = build_plan_from_raw(
            vec![step_raw("anything", ""), step_raw("", "")],
            "prompt",
            "goal",
            TaskMode::Audit,
        );
        assert!(plan.steps.is_empty());
    }

    #[test]
    fn build_plan_titleless_slug_falls_back_to_step_n() {
        // A title that contains no slug-able characters falls back
        // to `step-<N>` so the plan is never left with an empty id.
        let plan =
            build_plan_from_raw(vec![step_raw("", "!!!")], "prompt", "goal", TaskMode::Audit);
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].id, "step-1");
    }
}
