//! One-shot schema repair for malformed Finding records.
//!
//! This is deliberately a formatting pass: the model receives the rejected
//! raw objects and exact serde errors, may only correct their shape, and must
//! preserve ids and substantive claims. Rust validates the result again.

use std::collections::VecDeque;
use std::sync::Arc;

use serde::Deserialize;
use tokio::sync::Notify;

use kres_core::findings::Finding;
use kres_core::log::TurnLogger;
use kres_core::UsageTracker;
use kres_llm::{client::Client, model::ThinkingBudget, Model};

use crate::error::AgentError;
use crate::json_repair::{repair_json_response, JsonContract, JsonRepairCall, RepairLogKind};
use crate::response::InvalidFinding;

#[derive(Debug, Default)]
pub struct FindingRepairOutcome {
    pub findings: Vec<RepairedFinding>,
    pub unrepaired: Vec<InvalidFinding>,
}

#[derive(Debug)]
pub struct RepairedFinding {
    pub index: usize,
    pub finding: Finding,
}

pub enum FindingRepairCancel {
    Notify(Arc<Notify>),
    Shutdown(kres_core::Shutdown),
}

pub struct FindingRepairRuntime {
    pub logger: Option<Arc<TurnLogger>>,
    pub thinking: Option<ThinkingBudget>,
    pub cancel: Option<FindingRepairCancel>,
    pub usage: Option<Arc<UsageTracker>>,
    pub role: &'static str,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct FindingRepairResponse {
    findings: [Finding; 1],
}

pub async fn repair_invalid_findings(
    client: Arc<Client>,
    model: Model,
    max_tokens: u32,
    max_input_tokens: Option<u32>,
    invalid: Vec<InvalidFinding>,
    existing_findings: &[Finding],
    runtime: FindingRepairRuntime,
) -> Result<FindingRepairOutcome, AgentError> {
    let FindingRepairRuntime {
        logger,
        thinking,
        cancel,
        usage,
        role,
    } = runtime;
    if invalid.is_empty() {
        return Ok(FindingRepairOutcome::default());
    }
    let schema = serde_json::to_string(&schemars::schema_for!(FindingRepairResponse))?;
    let mut outcome = FindingRepairOutcome::default();
    let mut pending = VecDeque::from(invalid);
    while let Some(mut item) = pending.pop_front() {
        let Some(expected_id) = item
            .raw
            .get("id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
        else {
            outcome.unrepaired.push(item);
            continue;
        };
        // Hydration is a shortcut past the LLM: an id we already know
        // can be completed from the stored record. It is only a valid
        // shortcut if the result passes the check that rejected the
        // raw in the first place — otherwise the fast path launders
        // exactly the defect the slow path exists to fix.
        if let Some(finding) = hydrate_existing_finding(&item.raw, existing_findings) {
            match crate::response::unresolved_citation(&finding) {
                None => {
                    outcome.findings.push(RepairedFinding {
                        index: item.index,
                        finding,
                    });
                    continue;
                }
                Some(reason) => {
                    // Fall through to the model. Carry the surviving
                    // reason so it is asked about what is still wrong.
                    tracing::info!(
                        target: "kres_agents",
                        "hydrating '{expected_id}' from the store did not clear its citation: {reason}"
                    );
                    item.error = reason;
                }
            }
        }
        let rejected_response = serde_json::json!({"findings": [item.raw.clone()]}).to_string();
        let validation_errors = vec![format!("findings[0]: {}", item.error)];
        let shutdown = match &cancel {
            Some(FindingRepairCancel::Shutdown(shutdown)) => Some(shutdown.clone()),
            _ => None,
        };
        let repair = repair_json_response(JsonRepairCall {
            client: client.clone(),
            model: model.clone(),
            max_tokens,
            max_input_tokens,
            thinking,
            contract: JsonContract {
                name: "finding-repair",
                schema: &schema,
                instructions: "Return exactly one Finding with the same id. Correct field representation only; do not invent evidence or conclusions.",
            },
            rejected_response: &rejected_response,
            validation_errors: &validation_errors,
            logger: logger.clone(),
            log_kind: RepairLogKind::Code,
            shutdown,
        });
        let repaired = match match &cancel {
            Some(FindingRepairCancel::Notify(notify)) => tokio::select! {
                biased;
                _ = notify.notified() => {
                    outcome.unrepaired.push(item);
                    outcome.unrepaired.extend(pending);
                    return Ok(outcome);
                },
                result = repair => result,
            },
            _ => repair.await,
        } {
            Ok(repaired) => repaired,
            Err(error) => {
                item.error
                    .push_str(&format!("; repair request failed: {error}"));
                outcome.unrepaired.push(item);
                outcome.unrepaired.extend(pending);
                return Ok(outcome);
            }
        };
        if let Some(usage) = &usage {
            usage.record(
                role,
                model.id.clone(),
                repaired.usage.input_tokens,
                repaired.usage.output_tokens,
                repaired.usage.cache_creation_input_tokens,
                repaired.usage.cache_read_input_tokens,
            );
        }
        match accept_repaired_finding(&expected_id, &repaired.text) {
            Some(finding) => outcome.findings.push(RepairedFinding {
                index: item.index,
                finding,
            }),
            None => outcome.unrepaired.push(item),
        }
    }
    Ok(outcome)
}

/// Existing-id responses are deltas even though the wire schema describes a
/// complete Finding. Overlay the supplied fields on the stored record so an
/// update such as `{id, status, summary}` does not need an LLM call merely to
/// repeat the unchanged title, severity, and evidence fields.
/// Complete an id-matching raw finding from the stored record.
///
/// The raw's fields win, EXCEPT that an unresolvable citation never
/// displaces a resolved one. Without that carve-out a later delta
/// naming the same id — typically an invalidation, which needs no new
/// evidence — silently replaces a good `file:line` with a
/// placeholder. On the 2026-08-06 mm/page_alloc.c run all six `:0`
/// citations arrived this way: `fallbacks_table_row_index_mt3` held
/// `find_suitable_fallback:2254` until a promoter invalidation
/// overwrote it with `gfp_migratetype:0`.
fn hydrate_existing_finding(raw: &serde_json::Value, existing: &[Finding]) -> Option<Finding> {
    let raw_object = raw.as_object()?;
    let id = raw_object.get("id")?.as_str()?;
    let prior = existing.iter().find(|finding| finding.id == id)?;
    let redacted_prior = prior.redacted_for_agent();
    let mut hydrated = serde_json::to_value(&redacted_prior)
        .ok()?
        .as_object()
        .cloned()?;
    let prior_citation_is_resolved =
        crate::response::unresolved_citation(&redacted_prior).is_none();
    for (key, value) in raw_object.clone() {
        if prior_citation_is_resolved && CITATION_FIELDS.contains(&key.as_str()) {
            // Probe whether taking the raw's version would unresolve
            // the citation; keep the stored one when it would.
            let mut probe = hydrated.clone();
            probe.insert(key.clone(), value.clone());
            let would_unresolve =
                match serde_json::from_value::<Finding>(serde_json::Value::Object(probe)) {
                    Ok(candidate) => crate::response::unresolved_citation(&candidate).is_some(),
                    // Unparseable is not an improvement either.
                    Err(_) => true,
                };
            if would_unresolve {
                tracing::info!(
                    target: "kres_agents",
                    "kept the stored `{key}` for finding '{id}': the incoming one cites no line"
                );
                continue;
            }
        }
        hydrated.insert(key, value);
    }
    serde_json::from_value::<Finding>(serde_json::Value::Object(hydrated))
        .ok()
        .map(|finding| finding.redacted_for_agent())
}

/// Fields `unresolved_citation` inspects.
const CITATION_FIELDS: &[&str] = &["relevant_symbols", "relevant_file_sections"];

fn accept_repaired_finding(expected_id: &str, text: &str) -> Option<Finding> {
    let parsed =
        crate::json_repair::parse_strict_json::<FindingRepairResponse>("finding-repair", text)
            .ok()?;
    let [finding] = parsed.findings;
    (finding.id == expected_id).then(|| finding.redacted_for_agent())
}

pub fn format_unrepaired_findings(items: &[InvalidFinding]) -> String {
    let mut output =
        String::from("Malformed Finding records remained after one schema-repair attempt:\n");
    for item in items {
        output.push_str(&format!(
            "- findings[{}]: {}\n  raw: {}\n",
            item.index, item.error, item.raw
        ));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unrepaired_note_preserves_error_and_raw_object() {
        let invalid = InvalidFinding {
            index: 2,
            raw: json!({"id":"bad","line":null}),
            error: "invalid type: null, expected u32".into(),
        };
        let note = format_unrepaired_findings(&[invalid]);
        assert!(note.contains("findings[2]"));
        assert!(note.contains("expected u32"));
        assert!(note.contains("\"line\":null"));
    }

    fn valid_finding(id: &str) -> serde_json::Value {
        json!({
            "id": id,
            "title": "title",
            "severity": "medium",
            "summary": "summary"
        })
    }

    /// The 2026-08-06 mm/page_alloc.c run, exactly.
    /// `fallbacks_table_row_index_mt3` held
    /// `find_suitable_fallback:2254` until a promoter invalidation
    /// arrived naming `gfp_migratetype:0`. Hydration let the raw's
    /// keys win wholesale, so the placeholder replaced the resolved
    /// citation — and the result was pushed as "repaired" without
    /// re-running the check that had sent it to repair. All six `:0`
    /// citations in that run's findings.json arrived this way.
    #[test]
    fn an_unresolved_citation_never_displaces_a_resolved_one() {
        let prior: Finding = serde_json::from_value(json!({
            "id": "fallbacks_table_row_index_mt3",
            "title": "fallbacks[] row index confuses MIGRATE_HIGHATOMIC",
            "severity": "high",
            "summary": "original",
            "relevant_symbols": [{
                "name": "find_suitable_fallback",
                "filename": "mm/page_alloc.c",
                "line": 2254,
                "definition": "enum fallback_result find_suitable_fallback(...)"
            }]
        }))
        .unwrap();

        let hydrated = hydrate_existing_finding(
            &json!({
                "id": "fallbacks_table_row_index_mt3",
                "status": "invalidated",
                "summary": "MIGRATE_HIGHATOMIC is unreachable here",
                "relevant_symbols": [{
                    "name": "gfp_migratetype",
                    "filename": "include/linux/gfp.h",
                    "line": 0,
                    "definition": "VM_WARN_ON(...)"
                }]
            }),
            &[prior],
        )
        .expect("hydrates");

        // The invalidation's prose lands.
        assert_eq!(hydrated.status, kres_core::findings::Status::Invalidated);
        assert_eq!(hydrated.summary, "MIGRATE_HIGHATOMIC is unreachable here");
        // The resolved citation survives it.
        assert_eq!(hydrated.relevant_symbols.len(), 1);
        assert_eq!(hydrated.relevant_symbols[0].name, "find_suitable_fallback");
        assert_eq!(hydrated.relevant_symbols[0].line, 2254);
        assert!(crate::response::unresolved_citation(&hydrated).is_none());
    }

    /// A better citation must still be allowed to replace a worse one:
    /// the carve-out is about losing a line number, not about freezing
    /// the field.
    #[test]
    fn a_resolved_citation_may_replace_another_resolved_one() {
        let prior: Finding = serde_json::from_value(json!({
            "id": "f1", "title": "t", "severity": "medium", "summary": "s",
            "relevant_symbols": [{"name": "old", "filename": "mm/page_alloc.c", "line": 100, "definition": "d"}]
        }))
        .unwrap();
        let hydrated = hydrate_existing_finding(
            &json!({
                "id": "f1",
                "relevant_symbols": [{"name": "new", "filename": "mm/page_alloc.c", "line": 2266, "definition": "d"}]
            }),
            &[prior],
        )
        .unwrap();
        assert_eq!(hydrated.relevant_symbols[0].name, "new");
        assert_eq!(hydrated.relevant_symbols[0].line, 2266);
    }

    /// When the stored record is ALSO unresolved there is nothing to
    /// protect, so hydration cannot clear the citation and the item
    /// must go to the model rather than be accepted as repaired.
    #[test]
    fn hydration_that_cannot_clear_the_citation_is_not_accepted() {
        let prior: Finding = serde_json::from_value(json!({
            "id": "f1", "title": "t", "severity": "medium", "summary": "s",
            "relevant_symbols": [{"name": "a", "filename": "mm/page_alloc.c", "line": 0, "definition": "d"}]
        }))
        .unwrap();
        let hydrated =
            hydrate_existing_finding(&json!({"id": "f1", "status": "invalidated"}), &[prior])
                .expect("hydrates");
        assert!(
            crate::response::unresolved_citation(&hydrated).is_some(),
            "the repair loop must see this is still broken and call the model"
        );
    }

    #[test]
    fn repaired_finding_requires_exactly_one_same_id_record() {
        let one = json!({"findings":[valid_finding("f1")]}).to_string();
        assert!(accept_repaired_finding("f1", &one).is_some());
        assert!(accept_repaired_finding("other", &one).is_none());
        assert!(accept_repaired_finding("f1", r#"{"findings":[]}"#).is_none());
        let two = json!({"findings":[valid_finding("f1"), valid_finding("f1")]}).to_string();
        assert!(accept_repaired_finding("f1", &two).is_none());
    }

    #[test]
    fn existing_finding_supplies_fields_omitted_from_delta() {
        let prior: Finding = serde_json::from_value(json!({
            "id": "f1",
            "title": "Original title",
            "severity": "medium",
            "summary": "Original summary",
            "impact": "Original impact"
        }))
        .unwrap();
        let hydrated = hydrate_existing_finding(
            &json!({
                "id": "f1",
                "status": "invalidated",
                "summary": "New evidence disproves the trigger"
            }),
            &[prior],
        )
        .unwrap();

        assert_eq!(hydrated.title, "Original title");
        assert_eq!(hydrated.severity, kres_core::findings::Severity::Medium);
        assert_eq!(hydrated.summary, "New evidence disproves the trigger");
        assert_eq!(hydrated.impact, "Original impact");
        assert_eq!(hydrated.status, kres_core::findings::Status::Invalidated);
    }

    #[test]
    fn new_finding_cannot_be_hydrated() {
        assert!(hydrate_existing_finding(&json!({"id": "new", "status": "active"}), &[]).is_none());
    }

    #[tokio::test]
    async fn cancelled_repair_preserves_current_and_remaining_records() {
        let shutdown = kres_core::Shutdown::new();
        shutdown.cancel();
        let invalid = vec![
            InvalidFinding {
                index: 0,
                raw: json!({"id":"one"}),
                error: "missing title".into(),
            },
            InvalidFinding {
                index: 1,
                raw: json!({"id":"two"}),
                error: "missing title".into(),
            },
        ];
        let outcome = repair_invalid_findings(
            Arc::new(Client::new("unused").unwrap()),
            Model::opus_4_7(),
            1_000,
            None,
            invalid,
            &[],
            FindingRepairRuntime {
                logger: None,
                thinking: None,
                cancel: Some(FindingRepairCancel::Shutdown(shutdown)),
                usage: None,
                role: "test",
            },
        )
        .await
        .unwrap();
        assert!(outcome.findings.is_empty());
        assert_eq!(outcome.unrepaired.len(), 2);
        assert_eq!(outcome.unrepaired[0].raw["id"], "one");
        assert_eq!(outcome.unrepaired[1].raw["id"], "two");
    }
}
