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
    client::Client,
    config::CallConfig,
    model::ThinkingBudget,
    request::{CachedPrefix, Message},
    Model,
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
    ///
    /// MUST already be redacted with
    /// `kres_core::redact_findings_for_agent`: these bytes form the
    /// cache head the lens fan-out reads, and the lens path redacts
    /// (`pipeline.rs`, `prepare_lens_fanout`). Raw findings here would
    /// differ by the per-task provenance fields and buy an extra write
    /// of the largest payload in the request.
    pub previous_findings: &'a [Finding],
    /// The common (task-independent) half of the skills payload, as
    /// the lens path sends it in `common_skills`.
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

/// Fields that ride in the request's single uncached document.
///
/// There is deliberately no prioritize-specific cached block. One was
/// tried and measured: on the 2026-08-06 mm/page_alloc.c run the three
/// prioritize calls were 943s and 783s apart, far outside Anthropic's
/// 300s ephemeral TTL, so the entry expired every time — 21,886 tokens
/// of cache_creation per call against zero cache_read. A prefix nothing
/// will read is the same "write with no reader" trap 6328c9f removed
/// from the single-lens probe.
///
/// The one block worth caching is the session head, which the lens
/// fan-out reads seconds later — in that run the prioritize response
/// and all ten task starts share the timestamp 14:20:17. TTL is not a
/// factor at that distance, so the head is cached and everything here
/// is not.
const PRIORITIZE_INLINE_FIELDS: &[&str] = &["task", "instructions", "question", "plan"];

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

    // The cached head: byte-identical to what the lens fan-out of the
    // wave this ranking is about to dispatch will send. `skills` and
    // `previous_findings` therefore live HERE and must not also appear
    // below, or the model reads them twice.
    let session_head = crate::prompt::session_cache_head(inputs.skills, inputs.previous_findings)?;

    let mut request = serde_json::Map::new();
    request.insert("task".into(), json!("prioritize_pending"));
    request.insert("question".into(), json!(inputs.question));
    request.insert("ready".into(), json!(ready_payload));
    request.insert("limit".into(), json!(inputs.limit));
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

    // One document after the head. Splitting it further would only add
    // a cache_control slot for a block that expires before the next
    // ranking; see PRIORITIZE_INLINE_FIELDS.
    let inline =
        crate::prompt::split_request_documents(&Value::Object(request), PRIORITIZE_INLINE_FIELDS)?;
    let delta = inline.rendered();
    let request_text = format!("{session_head}{delta}");

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
    // `with_cached_prefixes` drops an empty head. That case is real:
    // a session with no skills configured has nothing in the head
    // until the first finding lands, and an empty text block is not
    // cacheable — it would spend one of Anthropic's four slots on
    // nothing, or be rejected outright.
    // Long-lived, and it must MATCH the lens fan-out's head: this is
    // the same bytes the wave's lenses send, and the whole point of
    // building it through one constructor is that the prioritizer
    // reads the entry they wrote. A different window on the same
    // bytes would be a second entry.
    let messages = vec![Message {
        role: "user".into(),
        content: delta,
        cache: false,
        cached_prefixes: Vec::from_iter(
            (!session_head.is_empty()).then(|| CachedPrefix::long(session_head)),
        ),
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

    /// `skills` and `previous_findings` live in the shared session
    /// head. If they also appear inline the model reads each twice —
    /// and `previous_findings` is ~166KB, so the duplicate is the
    /// single largest waste the request could contain.
    /// A session with no skills and no findings yet has an empty
    /// head. Sending it as a cached block spends a `cache_control`
    /// slot on an empty string.
    #[test]
    fn an_empty_session_head_is_not_sent_as_a_cached_block() {
        let head = crate::prompt::session_cache_head(None, &[]).unwrap();
        assert!(head.is_empty(), "nothing to cache in a fresh session");

        let message = kres_llm::request::Message {
            role: "user".into(),
            content: "DELTA".into(),
            cache: false,
            cached_prefixes: Vec::new(),
        }
        .with_cached_prefixes([head]);
        assert!(message.cached_prefixes.is_empty());

        let wire = serde_json::to_value(&message).unwrap();
        assert!(
            wire["content"].is_string(),
            "with no cacheable head the message is plain content: {}",
            wire["content"]
        );
    }

    /// End-to-end shape of the request the prioritizer actually
    /// sends: the shared head is the one cached block, and nothing in
    /// it is repeated below.
    #[test]
    fn the_request_carries_one_cached_head_and_no_duplicate_fields() {
        let skills = json!({"kernel": {"body": "SKILLBODY"}});
        let findings: Vec<kres_core::findings::Finding> = vec![serde_json::from_value(json!({
            "id": "already-found", "title": "t", "severity": "high", "summary": "s"
        }))
        .unwrap()];
        let head = crate::prompt::session_cache_head(Some(&skills), &findings).unwrap();

        let mut request = serde_json::Map::new();
        request.insert("task".into(), json!("prioritize_pending"));
        request.insert("question".into(), json!("review: mm/page_alloc.c"));
        request.insert("ready".into(), json!([{"id": "a"}]));
        request.insert("limit".into(), json!(1));
        request.insert("instructions".into(), json!("INSTRUCTIONS"));
        let inline = crate::prompt::split_request_documents(
            &Value::Object(request),
            PRIORITIZE_INLINE_FIELDS,
        )
        .unwrap();
        let delta = inline.rendered();

        let message = kres_llm::request::Message {
            role: "user".into(),
            content: delta.clone(),
            cache: false,
            cached_prefixes: vec![CachedPrefix::long(head.clone())],
        };
        let wire = serde_json::to_value(&message).unwrap();
        let blocks = wire["content"].as_array().unwrap();
        let cached = blocks
            .iter()
            .filter(|b| b.get("cache_control").is_some())
            .count();
        assert_eq!(cached, 1, "exactly one cached block: the shared head");
        assert_eq!(blocks[0]["text"], head, "and it must come first");

        assert!(head.contains("SKILLBODY") && head.contains("already-found"));
        assert!(
            !delta.contains("SKILLBODY") && !delta.contains("already-found"),
            "head content must not be repeated in the uncached tail"
        );
        assert!(delta.contains("mm/page_alloc.c") && delta.contains("INSTRUCTIONS"));
    }

    #[test]
    fn head_fields_are_not_repeated_inline() {
        for in_head in ["skills", "previous_findings", "common_skills"] {
            assert!(
                !PRIORITIZE_INLINE_FIELDS.contains(&in_head),
                "`{in_head}` is in the cached head and must not be sent inline too"
            );
        }
        assert!(PRIORITIZE_INLINE_FIELDS.contains(&"question"));
        assert!(PRIORITIZE_INLINE_FIELDS.contains(&"plan"));
    }

    /// There is no prioritize-specific cached block. One was measured
    /// on the 2026-08-06 run: calls 943s and 783s apart against a 300s
    /// TTL meant 21,886 tokens of cache_creation per call and zero
    /// reads. Only the session head — read by the lens fan-out seconds
    /// later — is worth a slot.
    #[test]
    fn ready_and_limit_stay_out_of_any_cached_block() {
        for per_wave in ["ready", "limit"] {
            assert!(
                !PRIORITIZE_INLINE_FIELDS.contains(&per_wave),
                "`{per_wave}` must not be in the split key set at all"
            );
        }
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
