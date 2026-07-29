//! One-shot schema repair for malformed Finding records.
//!
//! This is deliberately a formatting pass: the model receives the rejected
//! raw objects and exact serde errors, may only correct their shape, and must
//! preserve ids and substantive claims. Rust validates the result again.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde::Serialize;
use tokio::sync::Notify;

use kres_core::findings::Finding;
use kres_core::log::{LoggedUsage, TurnLogger};
use kres_llm::{client::Client, config::CallConfig, request::Message, Model};

use crate::error::AgentError;
use crate::response::{parse_code_response, InvalidFinding};

const REPAIR_SYSTEM: &str = "You repair malformed JSON Finding records. Return JSON only with exactly one top-level `findings` array. Correct schema and type errors only. Preserve every id and substantive claim. Do not add evidence, findings, code paths, or conclusions.";

#[derive(Debug, Default)]
pub struct FindingRepairOutcome {
    pub findings: Vec<Finding>,
    pub unrepaired: Vec<InvalidFinding>,
}

#[derive(Serialize)]
struct RepairRequest<'a> {
    task: &'static str,
    schema: &'static str,
    invalid_findings: &'a [InvalidFinding],
    instructions: &'static str,
}

pub async fn repair_invalid_findings(
    client: Arc<Client>,
    model: Model,
    max_tokens: u32,
    max_input_tokens: Option<u32>,
    invalid: Vec<InvalidFinding>,
    logger: Option<Arc<TurnLogger>>,
    cancel: Option<Arc<Notify>>,
) -> Result<FindingRepairOutcome, AgentError> {
    if invalid.is_empty() {
        return Ok(FindingRepairOutcome::default());
    }
    let request = RepairRequest {
        task: "repair_invalid_findings",
        schema: "{id:string,title:string,severity:low|medium|high,status:active|unconfirmed|fixed|invalidated,relevant_symbols:[{name:string,filename:string,line:u32,definition:string}],relevant_file_sections:[{filename:string,line_start:u32,line_end:u32,content:string}],summary:string,reproducer_sketch:string,impact:string,mechanism_detail?:string,fix_sketch?:string,open_questions?:[string],related_finding_ids?:[string],reactivate?:bool}",
        invalid_findings: &invalid,
        instructions: "Repair each object using its exact error. Keep the same id. Never invent a line number or other evidence. When a relevant_symbols or relevant_file_sections entry lacks a required numeric location, remove that entire evidence entry; the finding may retain the prose claim and open question. Convert a scalar to an array only when doing so preserves the supplied text. Return one repaired entry per input, in input order. JSON only.",
    };
    let body = serde_json::to_string(&request)?;
    let mut cfg = CallConfig::defaults_for(model)
        .with_max_tokens(max_tokens.min(16_000))
        .with_system(REPAIR_SYSTEM.to_string())
        .with_stream_label("repair invalid findings");
    if let Some(limit) = max_input_tokens {
        cfg = cfg.with_max_input_tokens(limit);
    }
    let messages = vec![Message {
        role: "user".into(),
        content: body.clone(),
        cache: false,
        cached_prefix: None,
    }];
    if let Some(log) = &logger {
        log.log_code_labeled("user", Some("phase=finding-repair"), &body, None, None);
    }
    let response = match cancel {
        Some(notify) => tokio::select! {
            biased;
            _ = notify.notified() => return Ok(FindingRepairOutcome { findings: vec![], unrepaired: invalid }),
            result = client.messages_streaming(&cfg, &messages) => result,
        },
        None => client.messages_streaming(&cfg, &messages).await,
    }
    .map_err(|error| AgentError::Other(error.to_string()))?;
    let text = response
        .content
        .iter()
        .filter_map(|block| match block {
            kres_llm::request::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    if let Some(log) = &logger {
        log.log_code_labeled(
            "assistant",
            Some("phase=finding-repair"),
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

    let parsed = parse_code_response(&text);
    Ok(accept_repaired_findings(invalid, parsed.findings))
}

fn accept_repaired_findings(
    invalid: Vec<InvalidFinding>,
    candidate_findings: Vec<Finding>,
) -> FindingRepairOutcome {
    let expected_ids: BTreeSet<String> = invalid
        .iter()
        .filter_map(|item| item.raw.get("id").and_then(|id| id.as_str()))
        .map(str::to_string)
        .collect();
    let mut repaired_ids = BTreeSet::new();
    let findings = candidate_findings
        .into_iter()
        .filter(|finding| {
            let accepted = expected_ids.contains(&finding.id) && repaired_ids.insert(finding.id.clone());
            if !accepted {
                tracing::warn!(target: "kres_agents", id = %finding.id, "finding repair changed or duplicated an id; rejecting entry");
            }
            accepted
        })
        .collect();
    let unrepaired = invalid
        .into_iter()
        .filter(|item| {
            item.raw
                .get("id")
                .and_then(|id| id.as_str())
                .map_or(true, |id| !repaired_ids.contains(id))
        })
        .collect();
    FindingRepairOutcome {
        findings,
        unrepaired,
    }
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
    use kres_core::findings::{Severity, Status};
    use serde_json::json;

    fn finding(id: &str) -> Finding {
        Finding {
            id: id.into(),
            title: "title".into(),
            severity: Severity::Medium,
            status: Status::Active,
            relevant_symbols: vec![],
            relevant_file_sections: vec![],
            summary: "summary".into(),
            reproducer_sketch: "reproducer".into(),
            impact: "impact".into(),
            mechanism_detail: None,
            fix_sketch: None,
            open_questions: vec!["question".into()],
            first_seen_task: None,
            last_updated_task: None,
            first_seen_at: None,
            related_finding_ids: vec![],
            details: vec![],
            reactivate: false,
            introduced_by: None,
        }
    }

    #[test]
    fn accepts_same_id_repair_and_rejects_changed_id() {
        let invalid = vec![InvalidFinding {
            index: 0,
            raw: json!({"id":"stale_file_end","open_questions":"question"}),
            error: "invalid type: string, expected a sequence".into(),
        }];
        let outcome = accept_repaired_findings(
            invalid.clone(),
            vec![finding("stale_file_end"), finding("invented")],
        );
        assert_eq!(outcome.findings.len(), 1);
        assert_eq!(outcome.findings[0].id, "stale_file_end");
        assert!(outcome.unrepaired.is_empty());

        let changed = accept_repaired_findings(invalid, vec![finding("renamed")]);
        assert!(changed.findings.is_empty());
        assert_eq!(changed.unrepaired.len(), 1);
    }

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
}
