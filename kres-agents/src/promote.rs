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
//! It also closes the symmetric stale-truth gap: later prose can
//! disprove or reactivate an existing finding, and that must update
//! the store instead of leaving the old row active forever.
//!
//! This pass runs once per reaped Analysis/Generic task, after all
//! the slow-agent and consolidator work is done, with the task's
//! effective analysis prose + a prose-relevant narrowing of the
//! current findings universe as input. It returns finding deltas:
//! new findings, same-id invalidations, and same-id reactivations.
//! The reaper extends the task's delta with these before handing it
//! to `FindingsStore::apply_delta`.
//!
//! Failure-mode hierarchy (best → worst):
//!   - Network error, empty prose, parse failure → empty promotion
//!     list, no bug added.
//!   - Promoter hits a real prose-only bug but emits an id that
//!     collides with an active entry the search narrowing missed →
//!     `filter_promoted_delta` renames the id to
//!     `<id>__promoted_<n>` and lets it through. Cost is a
//!     duplicate row in `findings.json` that a human can reconcile.
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
use kres_core::UsageTracker;
use kres_llm::{
    client::Client, config::CallConfig, model::ThinkingBudget, request::Message, Model,
};

use crate::{
    error::AgentError,
    response::{CodeResponseContract, InvalidFinding},
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

/// What the promoter needs to know about an already-recorded finding.
///
/// Enough to RECOGNISE a bug, not to act on one. The promoter decides
/// three things from this list: whether a bug in the prose is already
/// covered, whether the prose disproves a finding, and whether it
/// revives an invalidated one. Each of those needs the claim and its
/// identity; none needs the finding's source bodies, reproducer, fix
/// sketch or open questions.
///
/// `status` is here because the reactivation rule requires spotting
/// which entries are invalidated. Dropping it would silently retire
/// that behaviour.
///
/// Measured on the 2026-08-07 kernel/sched/fair.c review: a promote
/// request carrying 134 full findings was 1,350 KB, of which 1,328 KB
/// was `existing_findings` and 5.6 KB was the prose being audited —
/// 99.6% context for 0.4% subject. One finding alone was 25 KB, 10 KB
/// of it verbatim function source. Promote runs once per reaped task
/// and caches nothing.
#[derive(Debug, Serialize)]
struct FindingIdentity<'a> {
    id: &'a str,
    title: &'a str,
    status: kres_core::findings::Status,
    summary: &'a str,
}

impl<'a> From<&'a Finding> for FindingIdentity<'a> {
    fn from(f: &'a Finding) -> Self {
        Self {
            id: &f.id,
            title: &f.title,
            status: f.status,
            summary: &f.summary,
        }
    }
}

#[derive(Debug, Serialize)]
struct PromoteRequest<'a> {
    task: &'static str,
    task_brief: &'a str,
    existing_findings: Vec<FindingIdentity<'a>>,
    analysis: &'a str,
    instructions: &'a str,
}

/// Per-call inputs threaded into `promote_prose_bugs_with_logger`.
/// Bundles the auditing-side parameters so the function signature
/// stays focused on the endpoint config (client/model/system/limits).
pub struct PromoteInputs<'a> {
    pub task_brief: &'a str,
    pub analysis: &'a str,
    pub prose_relevant_existing: &'a [Finding],
    pub dedup_against: &'a [Finding],
    pub cancel: Option<Arc<Notify>>,
    pub usage: Option<Arc<UsageTracker>>,
    pub thinking: Option<ThinkingBudget>,
}

#[derive(Debug, Default)]
pub struct PromoteOutcome {
    pub findings: Vec<Finding>,
    pub unrepaired: Vec<InvalidFinding>,
}

/// Run the promotion pass against a configured fast-agent client.
///
/// - `prose_relevant_existing`: the findings the promoter dedups
///   against. Only `{id, title, status, summary}` of each is sent —
///   see [`FindingIdentity`] — so passing the full store costs the
///   claims, not their evidence. Callers may still narrow via
///   [`kres_core::relevant_subset`]; note that narrowing is close to
///   a no-op for a whole-file review, where every finding cites the
///   target file and every task's prose names it.
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
/// Returns finding deltas discovered in the prose. New active
/// findings with colliding ids are renamed to `<id>__promoted_<n>`.
/// Same-id invalidations and reactivations are preserved so the
/// store can update the existing row. Returns an empty list when
/// cancelled — abandonment is a safe, non-fatal outcome.
pub async fn promote_prose_bugs_with_logger(
    client: Arc<Client>,
    model: Model,
    system: Option<&str>,
    max_tokens: u32,
    max_input_tokens: Option<u32>,
    inputs: PromoteInputs<'_>,
    logger: Option<Arc<TurnLogger>>,
) -> Result<PromoteOutcome, AgentError> {
    let PromoteInputs {
        task_brief,
        analysis,
        prose_relevant_existing,
        dedup_against,
        cancel,
        usage,
        thinking,
    } = inputs;
    // Prose nothing to audit.
    if analysis.trim().is_empty() {
        return Ok(PromoteOutcome::default());
    }

    let request = PromoteRequest {
        task: "promote_prose_bugs",
        task_brief,
        existing_findings: prose_relevant_existing
            .iter()
            .map(FindingIdentity::from)
            .collect(),
        analysis,
        instructions: PROMOTE_INSTRUCTIONS,
    };
    let request_text = serde_json::to_string(&request)?;

    let mut cfg = CallConfig::defaults_for(model.clone())
        .with_max_tokens(max_tokens)
        .with_stream_label("promote prose");
    if let Some(s) = system {
        cfg = cfg.with_system(s.to_string());
    }
    if let Some(n) = max_input_tokens {
        cfg = cfg.with_max_input_tokens(n);
    }
    if let Some(thinking) = thinking {
        cfg = cfg.with_thinking(thinking);
    }

    // One-shot per task — tail cache would never be read.
    let messages = vec![Message {
        role: "user".into(),
        content: request_text,
        cache: false,
        cached_prefixes: Vec::new(),
    }];
    if let Some(lg) = &logger {
        let request = cfg.request_meta();
        lg.log_code_labeled_with_request(
            "user",
            Some("phase=promote"),
            &messages[0].content,
            None,
            None,
            Some(&request),
        );
    }
    let resp = match cancel.clone() {
        Some(notify) => tokio::select! {
            biased;
            _ = notify.notified() => {
                tracing::info!(
                    target: "kres_agents",
                    "promote pass cancelled mid-call"
                );
                return Ok(PromoteOutcome::default());
            }
            r = client.messages_streaming(&cfg, &messages) => r,
        },
        None => client.messages_streaming(&cfg, &messages).await,
    }
    .map_err(AgentError::from)?;
    if let Some(usage) = &usage {
        usage.record(
            "promote",
            model.id.clone(),
            resp.usage.input_tokens,
            resp.usage.output_tokens,
            resp.usage.cache_creation_input_tokens,
            resp.usage.cache_read_input_tokens,
        );
    }

    let text = extract_text(&resp);
    if let Some(lg) = &logger {
        // Same label as the user record above; see the note in consolidate.rs.
        lg.log_code_labeled_with_model(
            "assistant",
            Some("phase=promote"),
            &text,
            Some(LoggedUsage {
                input: resp.usage.input_tokens,
                output: resp.usage.output_tokens,
                cache_creation: resp.usage.cache_creation_input_tokens,
                cache_read: resp.usage.cache_read_input_tokens,
            }),
            None,
            resp.model.as_deref(),
        );
    }
    let response_contract = CodeResponseContract::default().requiring(["findings"]);
    let tolerant_contract = response_contract.clone().allowing_invalid_findings();
    let mut parsed = match tolerant_contract.validate(&text) {
        Ok(parsed) => parsed,
        Err(errors) => {
            let schema = response_contract.schema_json_for(&["findings"]).to_string();
            let repair = crate::json_repair::repair_json_response(
                crate::json_repair::JsonRepairCall {
                    client: client.clone(),
                    model: model.clone(),
                    max_tokens,
                    max_input_tokens,
                    thinking,
                    contract: crate::json_repair::JsonContract {
                        name: "promoter",
                        schema: &schema,
                        instructions: "Preserve every candidate claim and Finding id. Return the required findings array and correct representation only.",
                    },
                    rejected_response: &text,
                    validation_errors: &errors,
                    logger: logger.clone(),
                    log_kind: crate::json_repair::RepairLogKind::Code,
                    shutdown: None,
                },
            );
            let repaired = match &cancel {
                Some(notify) => tokio::select! {
                    biased;
                    _ = notify.notified() => return Ok(PromoteOutcome::default()),
                    result = repair => result,
                },
                None => repair.await,
            }?;
            if let Some(usage) = &usage {
                usage.record(
                    "promote",
                    model.id.clone(),
                    repaired.usage.input_tokens,
                    repaired.usage.output_tokens,
                    repaired.usage.cache_creation_input_tokens,
                    repaired.usage.cache_read_input_tokens,
                );
            }
            match tolerant_contract.accept_repair(&repaired.text) {
                Ok(parsed) => parsed,
                Err(repair_errors) => {
                    tracing::warn!(
                        target: "kres_agents",
                        errors = %repair_errors.join("; "),
                        "promoter JSON repair remained invalid"
                    );
                    return Ok(PromoteOutcome::default());
                }
            }
        }
    };
    crate::response::log_json_normalization(logger.as_deref(), &parsed, "promoter");
    // A RawText strategy on the promoter's OWN reply means the
    // dedicated PROMOTE_SYSTEM judge-mode prompt didn't hold — the
    // model emitted free-form prose instead of the required
    // `{"findings":[...]}` shape. We still degrade to an empty
    // extras list (safe), but the drift is worth a warning: if it
    // fires repeatedly the prompt (or the model) needs attention.
    // bytes_out is included so operators can spot a huge silent
    // dump vs a truly empty reply.
    let mut unrepaired = Vec::new();
    if !parsed.invalid_findings.is_empty() {
        let outcome = crate::finding_repair::repair_invalid_findings(
            client,
            model,
            max_tokens,
            max_input_tokens,
            std::mem::take(&mut parsed.invalid_findings),
            dedup_against,
            crate::finding_repair::FindingRepairRuntime {
                logger,
                thinking,
                cancel: cancel.map(crate::finding_repair::FindingRepairCancel::Notify),
                usage,
                role: "promote",
            },
        )
        .await?;
        parsed.merge_repaired_findings(outcome.findings);
        unrepaired = outcome.unrepaired;
    }
    Ok(PromoteOutcome {
        findings: filter_promoted_delta(parsed.findings, dedup_against),
        unrepaired,
    })
}

/// Filter and normalize promoted finding deltas.
///
/// Same-id `status: invalidated` entries and `reactivate: true`
/// entries are preserved: they intentionally update existing rows.
/// New active findings must have ids distinct from both the
/// `existing` set and every other entry in `promoted`; on collision,
/// rename by appending a `__promoted_<n>` suffix rather than
/// dropping the record. Empty ids are still dropped — there's no
/// useful bug to keep.
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
fn filter_promoted_delta(promoted: Vec<Finding>, existing: &[Finding]) -> Vec<Finding> {
    use std::collections::BTreeSet;
    let mut seen: BTreeSet<String> = existing.iter().map(|f| f.id.clone()).collect();
    let mut out = Vec::with_capacity(promoted.len());
    for mut p in promoted {
        if p.id.is_empty() {
            continue;
        }
        if let Some(prior) = existing.iter().find(|e| e.id == p.id) {
            if p.status == kres_core::findings::Status::Invalidated
                || (p.reactivate && prior.status == kres_core::findings::Status::Invalidated)
            {
                out.push(p);
                continue;
            }
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
            resolved_questions: vec![],
            details: vec![],
            introduced_by: None,
            first_seen_at: None,
        }
    }

    /// Promote must recognise bugs, not re-derive them. Sending whole
    /// findings made one request 1,350 KB of which 99.6% was context
    /// for 5.6 KB of prose. Note this is NOT the slice `finding_repair`
    /// sees — that one still gets whole findings, because it hydrates
    /// fields a malformed delta omitted.
    #[test]
    fn the_promoter_sees_claims_not_evidence() {
        let mut finding = f("preempt_short_skips_delayed_pse");
        finding.title = "PREEMPT_SHORT bypasses the WF_FORK guard".into();
        finding.summary = "fair.c:9845 jumps past the guard at fair.c:9857".into();
        finding.reproducer_sketch = "REPRODUCER THAT MUST NOT BE SENT".into();
        finding.fix_sketch = Some("FIX THAT MUST NOT BE SENT".into());
        finding.open_questions = vec!["QUESTION THAT MUST NOT BE SENT".into()];
        finding.relevant_symbols = vec![kres_core::findings::RelevantSymbol {
            name: "wakeup_preempt_fair".into(),
            filename: "kernel/sched/fair.c".into(),
            line: 9770,
            definition: "SOURCE THAT MUST NOT BE SENT".into(),
        }];

        let request = PromoteRequest {
            task: "promote_prose_bugs",
            task_brief: "brief",
            existing_findings: std::slice::from_ref(&finding)
                .iter()
                .map(FindingIdentity::from)
                .collect(),
            analysis: "prose",
            instructions: "instructions",
        };
        let wire = serde_json::to_string(&request).expect("serializes");

        for kept in [
            "preempt_short_skips_delayed_pse",
            "PREEMPT_SHORT bypasses the WF_FORK guard",
            "fair.c:9845",
            "active",
        ] {
            assert!(wire.contains(kept), "identity lost {kept}");
        }
        for dropped in [
            "MUST NOT BE SENT",
            "wakeup_preempt_fair",
            "kernel/sched/fair.c\"",
        ] {
            assert!(!wire.contains(dropped), "evidence leaked: {dropped}");
        }
    }

    #[test]
    fn filter_renames_ids_already_in_existing() {
        // Losing a record is worse than storing a duplicate — a
        // collision gets a __promoted_<n> suffix, not a drop.
        let existing = vec![f("a"), f("b")];
        let promoted = vec![f("a"), f("c"), f("b"), f("d")];
        let out = filter_promoted_delta(promoted, &existing);
        let ids: Vec<&str> = out.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(ids, vec!["a__promoted_2", "c", "b__promoted_2", "d"]);
    }

    #[test]
    fn filter_renames_within_promoted_output() {
        // Two promoted entries sharing an id also get renamed so
        // both records survive into the store.
        let out = filter_promoted_delta(vec![f("c"), f("c"), f("d")], &[]);
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
        let out = filter_promoted_delta(promoted, &existing);
        let ids: Vec<&str> = out.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(ids, vec!["x__promoted_3"]);
    }

    #[test]
    fn filter_drops_empty_ids() {
        // Empty id still drops — there's no useful record to keep.
        let mut weird = f("");
        weird.title = "no id".into();
        let out = filter_promoted_delta(vec![weird, f("legit")], &[]);
        let ids: Vec<&str> = out.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(ids, vec!["legit"]);
    }

    #[test]
    fn filter_preserves_same_id_invalidations_and_reactivations() {
        let existing = vec![f("active"), {
            let mut inv = f("invalidated");
            inv.status = Status::Invalidated;
            inv
        }];
        let mut invalidate = f("active");
        invalidate.status = Status::Invalidated;
        let mut reactivate = f("invalidated");
        reactivate.reactivate = true;
        let out = filter_promoted_delta(vec![invalidate, reactivate], &existing);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, "active");
        assert_eq!(out[0].status, Status::Invalidated);
        assert_eq!(out[1].id, "invalidated");
        assert!(out[1].reactivate);
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
            PromoteInputs {
                task_brief: "brief",
                analysis: "",
                prose_relevant_existing: &[],
                dedup_against: &[],
                cancel: None,
                usage: None,
                thinking: None,
            },
            None,
        )
        .await
        .unwrap();
        assert!(out.findings.is_empty());
        assert!(out.unrepaired.is_empty());
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
            PromoteInputs {
                task_brief: "brief",
                analysis: "some prose naming cpu_mask in lib/cpumask.c:42",
                prose_relevant_existing: &[],
                dedup_against: &[],
                cancel: Some(notify),
                usage: None,
                thinking: None,
            },
            None,
        )
        .await
        .unwrap();
        assert!(
            out.findings.is_empty() && out.unrepaired.is_empty(),
            "cancel path must return an empty extras list"
        );
    }
}
