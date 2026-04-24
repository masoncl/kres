//! Prose-to-findings promotion pass.
//!
//! Closes two silent-loss gaps in the pipeline:
//!
//! 1. The slow-agent / consolidator PROMOTION RULE is instructional
//!    only. If a lens or the consolidator describes a bug in prose
//!    but forgets to emit the matching Finding, the bug reaches
//!    report.md + the accumulated ledger but never enters
//!    `findings.json`.
//! 2. When a slow-agent or consolidator response has no parseable
//!    JSON, `parse_code_response` falls back to
//!    `ParseStrategy::RawText`, setting `analysis = text` and
//!    `findings = []`. Every bug the model described in that text
//!    is lost to the findings pipeline. (The per-slow-call
//!    translation at pipeline.rs handles RawText too, so this path
//!    is a belt-and-braces catch.)
//!
//! This pass runs once per reaped Analysis/Generic task, after all
//! the slow-agent and consolidator work is done, with the task's
//! effective analysis prose + a prose-relevant narrowing of the
//! current findings universe as input. It returns ONLY the
//! net-new findings — the reaper extends the task's delta with
//! these before handing it to `FindingsStore::apply_delta`.
//!
//! Failure-mode hierarchy (best → worst):
//!   - Network error, empty prose, parse failure → empty promotion
//!     list, no bug added.
//!   - Promoter hits a real prose-only bug but emits an id that
//!     collides with an entry the search narrowing missed →
//!     `filter_net_new` renames the id to `<id>__promoted_<n>` and
//!     lets it through. Cost is a duplicate row in `findings.json`
//!     that a human can reconcile.
//!   - Only empty ids are ever dropped — there's no useful record
//!     to keep in that case.
//!
//! Losing a finding to a silent drop is NOT on the failure list:
//! we'd rather store a duplicate than miss.

use std::sync::Arc;

use serde::Serialize;
use tokio::sync::Notify;

use kres_core::findings::Finding;
use kres_core::log::{LoggedUsage, TurnLogger};
use kres_llm::{client::Client, config::CallConfig, request::Message, Model};

use crate::{
    error::AgentError,
    response::{parse_code_response, ParseStrategy},
};

pub const PROMOTE_INSTRUCTIONS: &str = include_str!("prompts/promote.txt");

/// Dedicated system prompt for the promoter. Mirrors the reasoning
/// of the retired `merger_system.txt`: inheriting the fast-code-
/// agent's system prompt pushes the model toward the fast-agent
/// schema (ready_for_slow / skill_reads / <action> tags), which
/// parse_code_response can't lift into a findings list. A judge-
/// mode system that hard-restricts output to `{"findings": [...]}`
/// removes that drift surface. The call is already paid for; the
/// dedicated system adds zero network cost.
pub const PROMOTE_SYSTEM: &str = include_str!("prompts/promote_system.txt");

#[derive(Debug, Serialize)]
struct PromoteRequest<'a> {
    task: &'static str,
    task_brief: &'a str,
    existing_findings: &'a [Finding],
    analysis: &'a str,
    instructions: &'a str,
}

/// Run the promotion pass against a configured fast-agent client.
///
/// - `prose_relevant_existing`: the findings sent to the LLM as
///   `existing_findings`. Callers should narrow this via
///   [`kres_core::relevant_subset`] so the prompt doesn't balloon
///   with findings the audit can't plausibly dedup against. It is
///   always safe to pass the full store here — you just pay the
///   tokens.
/// - `dedup_against`: the universe of known ids used by the
///   post-response filter. Callers should pass the FULL store ∪
///   delta here, regardless of how aggressively the LLM-bound list
///   was narrowed. The filter renames colliding ids; it doesn't
///   drop, so a false-negative in the narrowing never costs us a
///   finding — it costs a duplicate row a human can reconcile.
/// - `cancel`: when `Some`, the HTTP round-trip is wrapped in a
///   `tokio::select!` on `notify.notified()`. A `notify_waiters()`
///   from the REPL's /stop handler abandons the call and returns
///   an empty extras list. Pass `None` from tests or call sites
///   that don't need operator-driven cancellation.
///
/// Returns the NET-NEW findings discovered in the prose (with any
/// colliding id renamed to `<id>__promoted_<n>`). Returns an empty
/// list when cancelled — abandonment is a safe, non-fatal outcome.
#[allow(clippy::too_many_arguments)]
pub async fn promote_prose_bugs_with_logger(
    client: Arc<Client>,
    model: Model,
    system: Option<&str>,
    max_tokens: u32,
    max_input_tokens: Option<u32>,
    task_brief: &str,
    analysis: &str,
    prose_relevant_existing: &[Finding],
    dedup_against: &[Finding],
    cancel: Option<Arc<Notify>>,
    logger: Option<Arc<TurnLogger>>,
) -> Result<Vec<Finding>, AgentError> {
    // Prose nothing to audit.
    if analysis.trim().is_empty() {
        return Ok(vec![]);
    }

    // Cap task_brief like the consolidator does so a long operator
    // prompt doesn't dominate the context window.
    let brief_capped: String = task_brief.chars().take(300).collect();
    let request = PromoteRequest {
        task: "promote_prose_bugs",
        task_brief: &brief_capped,
        existing_findings: prose_relevant_existing,
        analysis,
        instructions: PROMOTE_INSTRUCTIONS,
    };
    let request_text = serde_json::to_string(&request)?;

    let mut cfg = CallConfig::defaults_for(model)
        .with_max_tokens(max_tokens)
        .with_stream_label("promote prose");
    if let Some(s) = system {
        cfg = cfg.with_system(s.to_string());
    }
    if let Some(n) = max_input_tokens {
        cfg = cfg.with_max_input_tokens(n);
    }

    // One-shot per task — tail cache would never be read.
    let messages = vec![Message {
        role: "user".into(),
        content: request_text,
        cache: false,
        cached_prefix: None,
    }];
    if let Some(lg) = &logger {
        lg.log_code("user", &messages[0].content, None, None);
    }
    let resp = match cancel.clone() {
        Some(notify) => tokio::select! {
            biased;
            _ = notify.notified() => {
                tracing::info!(
                    target: "kres_agents",
                    "promote pass cancelled mid-call"
                );
                return Ok(vec![]);
            }
            r = client.messages_streaming(&cfg, &messages) => r,
        },
        None => client.messages_streaming(&cfg, &messages).await,
    }
    .map_err(|e| AgentError::Other(e.to_string()))?;

    let text = extract_text(&resp);
    if let Some(lg) = &logger {
        lg.log_code(
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

    let parsed = parse_code_response(&text);
    // A RawText strategy on the promoter's OWN reply means the
    // dedicated PROMOTE_SYSTEM judge-mode prompt didn't hold — the
    // model emitted free-form prose instead of the required
    // `{"findings":[...]}` shape. We still degrade to an empty
    // extras list (safe), but the drift is worth a warning: if it
    // fires repeatedly the prompt (or the model) needs attention.
    // bytes_out is included so operators can spot a huge silent
    // dump vs a truly empty reply.
    if parsed.strategy == ParseStrategy::RawText {
        tracing::warn!(
            target: "kres_agents",
            bytes_out = text.len(),
            "promoter reply had no parseable JSON; PROMOTE_SYSTEM drift suspected, returning empty"
        );
    }
    Ok(filter_net_new(parsed.findings, dedup_against))
}

/// Ensure every promoted Finding has an id distinct from both the
/// `existing` set and every other entry in `promoted`. On a
/// collision, RENAME the id by appending a `__promoted_<n>` suffix
/// rather than dropping the record. Empty ids are still dropped —
/// there's no useful bug to keep.
///
/// Policy rationale: it is much better to store a duplicate than to
/// miss a finding. Once we start narrowing the `existing` universe
/// by prose-relevance (to shrink the prompt), a search miss would
/// leave the promoter unaware of a store entry and free to re-emit
/// its id. Dropping on collision would then LOSE the promoted bug.
/// Renaming keeps the record, at the cost of a duplicate row that a
/// human reviewer or a later cleanup pass can reconcile.
///
/// `apply_delta_to_list` matches ids against the full store, so a
/// renamed id always lands as a fresh append; the original store
/// entry is untouched.
fn filter_net_new(promoted: Vec<Finding>, existing: &[Finding]) -> Vec<Finding> {
    use std::collections::BTreeSet;
    let mut seen: BTreeSet<String> = existing.iter().map(|f| f.id.clone()).collect();
    let mut out = Vec::with_capacity(promoted.len());
    for mut p in promoted {
        if p.id.is_empty() {
            continue;
        }
        if seen.contains(&p.id) {
            let original = p.id.clone();
            let mut suffix = 2u32;
            loop {
                let candidate = format!("{original}__promoted_{suffix}");
                if !seen.contains(&candidate) {
                    p.id = candidate;
                    break;
                }
                suffix += 1;
            }
        }
        seen.insert(p.id.clone());
        out.push(p);
    }
    out
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
    use kres_core::findings::{Severity, Status};

    fn f(id: &str) -> Finding {
        Finding {
            id: id.to_string(),
            title: id.to_string(),
            severity: Severity::Medium,
            status: Status::Active,
            relevant_symbols: vec![],
            relevant_file_sections: vec![],
            summary: "s".into(),
            reproducer_sketch: "r".into(),
            impact: "i".into(),
            mechanism_detail: None,
            fix_sketch: None,
            open_questions: vec![],
            first_seen_task: None,
            last_updated_task: None,
            related_finding_ids: vec![],
            reactivate: false,
            details: vec![],
            introduced_by: None,
            first_seen_at: None,
        }
    }

    #[test]
    fn filter_renames_ids_already_in_existing() {
        // Losing a record is worse than storing a duplicate — a
        // collision gets a __promoted_<n> suffix, not a drop.
        let existing = vec![f("a"), f("b")];
        let promoted = vec![f("a"), f("c"), f("b"), f("d")];
        let out = filter_net_new(promoted, &existing);
        let ids: Vec<&str> = out.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(ids, vec!["a__promoted_2", "c", "b__promoted_2", "d"]);
    }

    #[test]
    fn filter_renames_within_promoted_output() {
        // Two promoted entries sharing an id also get renamed so
        // both records survive into the store.
        let out = filter_net_new(vec![f("c"), f("c"), f("d")], &[]);
        let ids: Vec<&str> = out.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(ids, vec!["c", "c__promoted_2", "d"]);
    }

    #[test]
    fn filter_renames_with_escalating_suffix_when_needed() {
        // Collision with a pre-existing `x__promoted_2` must escalate
        // past 2 rather than re-colliding.
        let mut pre = f("x__promoted_2");
        pre.title = "pre-existing renamed".into();
        let existing = vec![f("x"), pre];
        let promoted = vec![f("x")];
        let out = filter_net_new(promoted, &existing);
        let ids: Vec<&str> = out.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(ids, vec!["x__promoted_3"]);
    }

    #[test]
    fn filter_drops_empty_ids() {
        // Empty id still drops — there's no useful record to keep.
        let mut weird = f("");
        weird.title = "no id".into();
        let out = filter_net_new(vec![weird, f("legit")], &[]);
        let ids: Vec<&str> = out.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(ids, vec!["legit"]);
    }

    #[tokio::test]
    async fn empty_analysis_returns_empty_without_api_call() {
        // The function must short-circuit on empty prose so we don't
        // waste an API round-trip on no-op inputs.
        let c = Arc::new(Client::new("sk-unused").unwrap());
        let out = promote_prose_bugs_with_logger(
            c,
            Model::opus_4_7(),
            None,
            8_000,
            None,
            "brief",
            "",
            &[],
            &[],
            None,
            None,
        )
        .await
        .unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn cancel_before_http_roundtrip_short_circuits() {
        // When /stop fires (notify.notify_waiters()) before
        // messages_streaming can resolve, the promoter must return
        // Ok(vec![]) immediately. We pre-notify so the select!'s
        // `biased` branch wins deterministically — the real HTTP
        // call with sk-unused would otherwise fail with an auth
        // error after a network round-trip. Combined with `biased`,
        // this test runs synchronously after the notify without
        // making any network traffic.
        let notify = Arc::new(tokio::sync::Notify::new());
        // notify_waiters() only wakes currently-registered waiters;
        // to guarantee the select! sees a pending notification we
        // pre-permit via notify_one() which stores a permit for the
        // next notified() call. biased ordering still prefers the
        // cancel branch.
        notify.notify_one();
        let c = Arc::new(Client::new("sk-unused").unwrap());
        let out = promote_prose_bugs_with_logger(
            c,
            Model::opus_4_7(),
            None,
            8_000,
            None,
            "brief",
            "some prose naming cpu_mask in lib/cpumask.c:42",
            &[],
            &[],
            Some(notify),
            None,
        )
        .await
        .unwrap();
        assert!(
            out.is_empty(),
            "cancel path must return an empty extras list"
        );
    }
}
