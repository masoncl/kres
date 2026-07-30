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

    #[test]
    fn repaired_finding_requires_exactly_one_same_id_record() {
        let one = json!({"findings":[valid_finding("f1")]}).to_string();
        assert!(accept_repaired_finding("f1", &one).is_some());
        assert!(accept_repaired_finding("other", &one).is_none());
        assert!(accept_repaired_finding("f1", r#"{"findings":[]}"#).is_none());
        let two = json!({"findings":[valid_finding("f1"), valid_finding("f1")]}).to_string();
        assert!(accept_repaired_finding("f1", &two).is_none());
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
