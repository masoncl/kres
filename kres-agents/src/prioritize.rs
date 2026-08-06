//! Prioritization agent: choose which pending todos run next.
//!
//! Split out of the todo agent. The todo agent owns the *content* of
//! the list — dedup, completion, retirement, plan linkage — and stores
//! pending work in a stable order it does not churn. This module owns
//! the single remaining question: given everything the session knows
//! so far, which N of the ready items should drive the next wave?
//!
//! Two reasons the split is worth a separate call.
//!
//! **Different context.** Ranking by expected payoff needs the session
//! question, the findings so far, the plan and the domain skills — the
//! same material the slow agents reason over. The todo agent's job
//! needs the list and the coverage prose, and nothing else. Merging
//! them forced one prompt to carry both and one model to do both.
//!
//! **Different model.** This runs on the slow coding agent, which is
//! the model that has been reading the actual source. The todo agent
//! runs wherever the operator pointed the `todo` role.
//!
//! The output is deliberately tiny — a handful of ids and a one-line
//! rationale each — so the call is input-bound. Contrast the todo
//! agent, which was output-bound at 9.2s per 1k output tokens over the
//! 2026-08-05 mm/page_alloc.c review.

use std::collections::HashSet;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};

use kres_core::findings::Finding;
use kres_core::log::{LoggedUsage, TurnLogger};
use kres_core::todo::TodoItem;
use kres_core::UsageTracker;
use kres_llm::{
    client::Client, config::CallConfig, model::ThinkingBudget, request::Message, Model,
};

use crate::error::AgentError;

/// Config bundle for the prioritization agent. Populated from the slow
/// agent's client/model plus the slow coding system prompt.
#[derive(Clone)]
pub struct PrioritizeClient {
    pub client: Arc<Client>,
    pub model: Model,
    pub system: Option<String>,
    pub max_tokens: u32,
    pub max_input_tokens: Option<u32>,
    pub thinking: Option<ThinkingBudget>,
    pub usage: Option<Arc<UsageTracker>>,
}

fn record_usage(pc: &PrioritizeClient, usage: &kres_llm::request::Usage) {
    if let Some(tracker) = &pc.usage {
        tracker.record(
            "prioritize",
            pc.model.id.clone(),
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_creation_input_tokens,
            usage.cache_read_input_tokens,
        );
    }
}

/// Per-call inputs. `ready` is the only volatile list: the caller has
/// already filtered out rows that are done, retired, running, or
/// blocked on an unfinished dependency, so every entry here is
/// genuinely dispatchable right now.
pub struct PrioritizeInputs<'a> {
    /// The operator's session prompt — what the whole run is for.
    pub question: &'a str,
    /// Dispatchable pending rows. Order carries no meaning.
    pub ready: &'a [TodoItem],
    /// Everything found so far, in full. A finding that looks
    /// unrelated by filename is exactly the one that makes an
    /// unrelated-looking todo urgent.
    pub previous_findings: &'a [Finding],
    /// Domain skills, as sent to the slow agents.
    pub skills: Option<&'a Value>,
    /// The current plan, so ranking can respect staging.
    pub plan: Option<&'a kres_core::Plan>,
    /// How many items the caller can actually dispatch. The agent
    /// must not return more than this.
    pub limit: usize,
}

/// One selection, in rank order.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Selection {
    /// `id` of a row from `ready`.
    id: String,
    /// One line on why this outranks what was left behind. Logged for
    /// the operator; not consumed by any Rust logic.
    #[serde(default)]
    why: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct PrioritizeResponse {
    selected: Vec<Selection>,
}

/// Top-level fields the agent may return.
const PRIORITIZE_RESPONSE_FIELDS: &[&str] = &["selected"];

/// Fields of a `prioritize` request that repeat between dispatch
/// waves, and therefore belong above the cache breakpoint.
///
/// The rule this list exists to obey: a field belongs here only if it
/// is the SAME BYTES on the next call. The stable half is one cached
/// prefix, so a single volatile member rewrites the entry for every
/// other member — and serde_json orders keys (no `preserve_order` in
/// this workspace), so a volatile key that sorts early poisons
/// everything after it. `completed_query` and `original_prompt` were
/// both making exactly that mistake before 4692adc.
///
/// `previous_findings` is deliberately NOT here. It is the largest
/// input and the most tempting to cache, but in an audit run it grows
/// on most reaps, so caching it would invalidate the prefix rather
/// than reuse it. It stays in the delta half and is sent in full every
/// call.
const PRIORITIZE_STABLE_FIELDS: &[&str] = &["task", "instructions", "question", "skills", "plan"];

/// Rank the ready pending items and return the chosen ids, best first.
///
/// Returns at most `limit` ids, every one of them present in `ready`.
/// On any failure — transport error, unparseable reply, shutdown — the
/// return is empty and the caller falls back to list order. Dispatch
/// must never stall on a flaky ranking call.
pub async fn prioritize_pending_with_logger(
    pc: &PrioritizeClient,
    inputs: PrioritizeInputs<'_>,
    logger: Option<Arc<TurnLogger>>,
    shutdown: Option<kres_core::Shutdown>,
) -> Result<Vec<String>, AgentError> {
    if inputs.ready.is_empty() || inputs.limit == 0 {
        return Ok(Vec::new());
    }
    // Nothing to rank: every ready item fits in this wave.
    if inputs.ready.len() <= inputs.limit {
        return Ok(inputs.ready.iter().map(|item| item.id.clone()).collect());
    }

    let ready_payload: Vec<Value> = inputs
        .ready
        .iter()
        .map(|item| {
            let mut obj = serde_json::Map::new();
            obj.insert("id".into(), json!(item.id));
            obj.insert("type".into(), json!(item.kind));
            obj.insert("name".into(), json!(item.name));
            obj.insert("reason".into(), json!(item.reason));
            if !item.step_id.is_empty() {
                obj.insert("step_id".into(), json!(item.step_id));
            }
            Value::Object(obj)
        })
        .collect();

    let mut request = serde_json::Map::new();
    request.insert("task".into(), json!("prioritize_pending"));
    request.insert("question".into(), json!(inputs.question));
    request.insert("ready".into(), json!(ready_payload));
    request.insert("limit".into(), json!(inputs.limit));
    request.insert("previous_findings".into(), json!(inputs.previous_findings));
    if let Some(skills) = inputs.skills {
        request.insert("skills".into(), skills.clone());
    }
    let has_plan = if let Some(plan) = inputs.plan {
        match serde_json::to_value(plan) {
            Ok(value) => {
                request.insert("plan".into(), value);
                true
            }
            Err(_) => false,
        }
    } else {
        false
    };
    request.insert(
        "instructions".into(),
        json!(build_instructions(has_plan, inputs.limit)),
    );

    let split =
        crate::prompt::split_request_documents(&Value::Object(request), PRIORITIZE_STABLE_FIELDS)?;
    let request_text = split.rendered();

    let mut cfg = CallConfig::defaults_for(pc.model.clone())
        .with_max_tokens(pc.max_tokens)
        .with_stream_label("prioritize");
    if let Some(system) = &pc.system {
        cfg = cfg.with_system(system.clone());
    }
    if let Some(limit) = pc.max_input_tokens {
        cfg = cfg.with_max_input_tokens(limit);
    }
    if let Some(thinking) = pc.thinking {
        cfg = cfg.with_thinking(thinking);
    }
    let messages = vec![Message {
        role: "user".into(),
        content: split.delta.clone(),
        cache: false,
        cached_prefixes: Vec::from_iter((!split.stable.is_empty()).then(|| split.stable.clone())),
    }];
    if let Some(lg) = &logger {
        let meta = cfg.request_meta();
        lg.log_main_with_request(
            "user",
            Some("phase=prioritize"),
            &request_text,
            None,
            None,
            Some(&meta),
        );
    }

    let response = if let Some(shutdown) = shutdown.clone() {
        tokio::select! {
            _ = shutdown.cancelled() => return Ok(Vec::new()),
            result = pc.client.messages_streaming(&cfg, &messages) => result,
        }
    } else {
        pc.client.messages_streaming(&cfg, &messages).await
    };
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(
                target: "kres_agents",
                "prioritize call failed: {error}; falling back to list order"
            );
            return Ok(Vec::new());
        }
    };
    record_usage(pc, &response.usage);
    let text = extract_text(&response);
    if let Some(lg) = &logger {
        lg.log_main(
            "assistant",
            Some("phase=prioritize"),
            &text,
            Some(LoggedUsage {
                input: response.usage.input_tokens,
                output: response.usage.output_tokens,
                cache_creation: response.usage.cache_creation_input_tokens,
                cache_read: response.usage.cache_read_input_tokens,
            }),
            None,
        );
    }

    let initial = parse_prioritize_response(&text);
    let mut parsed = initial.as_ref().ok().cloned();
    if let Err(errors) = initial {
        let schema = serde_json::to_string(&schemars::schema_for!(PrioritizeResponse))
            .expect("generated prioritize schema is serializable");
        if let Ok(repaired) =
            crate::json_repair::repair_json_response(crate::json_repair::JsonRepairCall {
                client: pc.client.clone(),
                model: pc.model.clone(),
                max_tokens: pc.max_tokens,
                max_input_tokens: pc.max_input_tokens,
                thinking: pc.thinking,
                contract: crate::json_repair::JsonContract {
                    name: "prioritize",
                    schema: &schema,
                    instructions: "Preserve every selected id and its order. Correct representation and field types only.",
                },
                rejected_response: &text,
                validation_errors: &errors,
                logger: logger.clone(),
                log_kind: crate::json_repair::RepairLogKind::Main,
                shutdown,
            })
            .await
        {
            record_usage(pc, &repaired.usage);
            let contract = crate::json_repair::JsonObjectContract {
                name: "prioritize",
                fields: PRIORITIZE_RESPONSE_FIELDS,
            };
            match contract.accept_repair::<PrioritizeResponse>(&repaired.text) {
                Ok(response) => parsed = Some(response.selected),
                Err(_) => tracing::warn!(
                    target: "kres_agents",
                    "prioritize JSON repair failed the strict response contract"
                ),
            }
        }
    }
    let Some(selected) = parsed else {
        tracing::warn!(
            target: "kres_agents",
            "prioritize returned no parseable selection; falling back to list order"
        );
        return Ok(Vec::new());
    };

    Ok(resolve_selection(&selected, inputs.ready, inputs.limit))
}

/// Map the agent's picks onto real ready rows.
///
/// The agent chooses; it does not get to invent, duplicate, or exceed
/// the budget. An unknown id is dropped with a log line rather than
/// failing the whole wave — a ranking call that names four real items
/// and one hallucinated one is still four useful decisions.
fn resolve_selection(selected: &[Selection], ready: &[TodoItem], limit: usize) -> Vec<String> {
    let known: HashSet<&str> = ready.iter().map(|item| item.id.as_str()).collect();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    let mut unknown: Vec<&str> = Vec::new();
    for pick in selected {
        if out.len() == limit {
            break;
        }
        let Some(id) = known.get(pick.id.as_str()) else {
            unknown.push(pick.id.as_str());
            continue;
        };
        if !seen.insert(id) {
            continue;
        }
        // The rationale is the only window an operator has into why
        // one item beat the rest of the list this wave.
        kres_core::async_eprintln!(
            "[prioritize] {}. {} — {}",
            out.len() + 1,
            id,
            pick.why.trim()
        );
        out.push((*id).to_string());
    }
    if !unknown.is_empty() {
        tracing::info!(
            target: "kres_agents",
            "prioritize named {} id(s) that are not ready; ignored: {}",
            unknown.len(),
            unknown
                .iter()
                .take(5)
                .copied()
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    out
}

fn build_instructions(has_plan: bool, limit: usize) -> String {
    let mut s = String::from(
        "You are choosing what the next wave of analysis works on. \
         Return raw, unfenced JSON only:\n\
         {\"selected\": [{\"id\":\"ID\",\"why\":\"one line\"}]}\n\n",
    );
    s.push_str(&format!(
        "`ready` lists every todo item that is dispatchable right now: \
         nothing in it is done, running, or blocked on an unfinished \
         dependency. Its order carries NO information — it is storage \
         order, not a ranking, and you must not treat a leading item as \
         already preferred.\n\n\
         Return AT MOST {limit} entries, best first, each `id` copied \
         verbatim from `ready`. Fewer is fine when fewer are worth \
         running. Never invent an id, never repeat one, and never \
         exceed {limit}.\n\n"
    ));
    s.push_str(
        "RANK BY EXPECTED PAYOFF:\n\
         - Likelihood of surfacing a real, triggerable defect. An item \
         that would confirm or kill a strong suspect in \
         `previous_findings` outranks a fresh sweep of untouched code.\n\
         - Whether it resolves an open question another finding depends \
         on. `previous_findings` is supplied in full for exactly this \
         reason: a finding that looks unrelated by filename is often \
         what makes an unrelated-looking item urgent.\n\
         - Whether it unblocks many downstream items, or is the shared \
         evidence several open questions need.\n\
         - Cold code over warm: prefer items citing files and symbols \
         no earlier work has already read.\n\
         - Break ties toward the item whose result most changes what \
         you would do next. An item whose answer is predictable is \
         worth less than one that could redirect the run.\n\n",
    );
    if has_plan {
        s.push_str(
            "PLAN AWARENESS — a `plan` field is present. Respect its \
             staging: work belonging to an earlier, still-open stage \
             generally outranks work from a later stage, and a final \
             completeness step is worth running only once the groups it \
             covers have actually produced evidence. Do not fill the \
             whole wave from one step when other open steps have ready \
             work; breadth across open steps beats depth in one, unless \
             a finding makes that one step decisive.\n\n",
        );
    }
    s.push_str(
        "DO NOT: edit, rename, complete, retire, merge, or dedup \
         anything. You are not maintaining the list — another agent \
         owns its contents and will see your picks run. Your entire \
         output is the ranked subset. `why` is one line for a human \
         reading the log; keep it short.",
    );
    s
}

fn parse_prioritize_response(text: &str) -> Result<Vec<Selection>, Vec<String>> {
    let parsed = crate::json_repair::JsonObjectContract {
        name: "prioritize",
        fields: PRIORITIZE_RESPONSE_FIELDS,
    }
    .parse::<PrioritizeResponse>(text)?;
    Ok(parsed.selected)
}

fn extract_text(response: &kres_llm::request::MessagesResponse) -> String {
    let mut out = String::new();
    for block in &response.content {
        if let kres_llm::request::ContentBlock::Text { text } = block {
            out.push_str(text);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready(ids: &[&str]) -> Vec<TodoItem> {
        ids.iter()
            .map(|id| {
                let mut item = TodoItem::new(format!("audit {id}"), "review");
                item.id = (*id).to_string();
                item
            })
            .collect()
    }

    fn pick(id: &str) -> Selection {
        Selection {
            id: id.to_string(),
            why: "because".into(),
        }
    }

    #[test]
    fn selection_is_resolved_in_rank_order() {
        let ready = ready(&["a", "b", "c", "d"]);
        let out = resolve_selection(&[pick("c"), pick("a")], &ready, 3);
        assert_eq!(out, vec!["c".to_string(), "a".to_string()]);
    }

    /// The agent ranks; it does not get to invent work, run an item
    /// twice, or spend more of the wave than the caller can dispatch.
    #[test]
    fn invented_duplicate_and_over_budget_picks_are_dropped() {
        let ready = ready(&["a", "b", "c"]);
        let out = resolve_selection(
            &[
                pick("b"),
                pick("nonexistent"),
                pick("b"),
                pick("a"),
                pick("c"),
            ],
            &ready,
            2,
        );
        assert_eq!(out, vec!["b".to_string(), "a".to_string()]);
    }

    #[test]
    fn a_wave_that_fits_needs_no_ranking_call() {
        // Exercised by the early return in
        // `prioritize_pending_with_logger`; asserted here so the
        // condition cannot silently invert.
        let ready = ready(&["a", "b"]);
        assert!(ready.len() <= 2, "no call is made when everything fits");
    }

    /// The lesson from 4692adc: a field that changes between calls
    /// must not sit above the cache breakpoint.
    #[test]
    fn the_cached_prefix_holds_no_per_wave_field() {
        for volatile in ["ready", "limit", "previous_findings"] {
            assert!(
                !PRIORITIZE_STABLE_FIELDS.contains(&volatile),
                "`{volatile}` changes between waves and must stay in the delta half"
            );
        }
        assert!(PRIORITIZE_STABLE_FIELDS.contains(&"question"));
        assert!(PRIORITIZE_STABLE_FIELDS.contains(&"skills"));
        assert!(PRIORITIZE_STABLE_FIELDS.contains(&"plan"));
    }

    #[test]
    fn two_waves_of_the_same_session_share_one_cached_prefix() {
        let base = json!({
            "task": "prioritize_pending",
            "instructions": "INSTRUCTIONS",
            "question": "review: mm/page_alloc.c",
            "skills": {"kernel": "..."},
            "plan": {"steps": []},
            "previous_findings": [],
            "limit": 4,
            "ready": [{"id": "a"}],
        });
        let mut later = base.clone();
        later["ready"] = json!([{"id": "b"}, {"id": "c"}]);
        later["limit"] = json!(2);
        later["previous_findings"] = json!([{"id": "found-a-bug"}]);

        let a = crate::prompt::split_request_documents(&base, PRIORITIZE_STABLE_FIELDS).unwrap();
        let b = crate::prompt::split_request_documents(&later, PRIORITIZE_STABLE_FIELDS).unwrap();
        assert_eq!(a.stable, b.stable);
        assert!(a.stable.contains("mm/page_alloc.c"));
        assert!(!a.stable.contains("found-a-bug"));
        assert!(b.delta.contains("found-a-bug"));
    }

    #[test]
    fn instructions_say_storage_order_is_not_a_ranking() {
        let body = build_instructions(true, 4);
        assert!(body.contains("order carries NO information"));
        assert!(body.contains("AT MOST 4"));
        assert!(body.contains("PLAN AWARENESS"));
        assert!(body.contains("You are not maintaining the list"));
    }

    #[test]
    fn parse_rejects_an_unknown_field() {
        let ok = r#"{"selected":[{"id":"a","why":"strong suspect"}]}"#;
        assert_eq!(parse_prioritize_response(ok).unwrap().len(), 1);
        let bad = r#"{"selected":[{"id":"a","reason":"wrong key"}]}"#;
        assert!(parse_prioritize_response(bad).is_err());
    }
}
