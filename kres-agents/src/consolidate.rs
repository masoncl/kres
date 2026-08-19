//! Cross-lens consolidation pass.
//!
//! Input: N per-lens outputs for one task. Output: one unified
//! analysis narrative + one deduplicated findings list.
//!
//! The instructions match `_LENS_CONSOLIDATOR_INSTRUCTIONS` in the
//! and include the recent COMPLETENESS CHECK rule
//! (promote prose-only bugs to Findings or drop them from prose).

use std::sync::Arc;

use serde::Serialize;
use serde_json::{json, Value};

use kres_core::findings::Finding;
use kres_core::log::{LoggedUsage, TurnLogger};
use kres_llm::{config::CallConfig, request::Message};

use crate::{
    error::AgentError,
    followup::Followup,
    response::{log_json_normalization, CodeResponseContract},
};

pub const CONSOLIDATOR_INSTRUCTIONS: &str = include_str!("prompts/consolidator.txt");

#[derive(Debug, Serialize)]
pub struct LensOutput<'a> {
    pub lens: &'a Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slow_model: Option<&'a str>,
    pub analysis: &'a str,
    pub findings: &'a [Finding],
    pub followups: &'a [Followup],
}

#[derive(Debug, Serialize)]
struct ConsolidatorRequest<'a> {
    task: &'static str,
    task_brief: &'a str,
    lens_outputs: &'a [LensOutput<'a>],
    instructions: &'a str,
}

#[derive(Debug, Clone)]
pub struct ConsolidatedTask {
    pub analysis: String,
    pub findings: Vec<Finding>,
    pub followups: Vec<Followup>,
    pub comparison: Option<Value>,
}

/// Run the consolidator against a configured fast-agent client.
///
/// Falls back to a naive concat + findings-union on any failure so
/// a flaky consolidator call doesn't kill the task's output.
pub async fn consolidate_lenses(
    consolidator: &crate::pipeline::ConsolidatorClient,
    task_brief: &str,
    lens_outputs: &[LensOutput<'_>],
) -> Result<ConsolidatedTask, AgentError> {
    consolidate_lenses_with_logger(consolidator, task_brief, lens_outputs, None, None, None).await
}

/// Same as [`consolidate_lenses`] but appends user+assistant turns
/// to the provided TurnLogger's code.jsonl.
pub async fn consolidate_lenses_with_logger(
    consolidator: &crate::pipeline::ConsolidatorClient,
    task_brief: &str,
    lens_outputs: &[LensOutput<'_>],
    workflow_rules: Option<&str>,
    logger: Option<Arc<TurnLogger>>,
    shutdown: Option<kres_core::Shutdown>,
) -> Result<ConsolidatedTask, AgentError> {
    if lens_outputs.is_empty() {
        return Ok(ConsolidatedTask {
            analysis: String::new(),
            findings: vec![],
            followups: vec![],
            comparison: None,
        });
    }
    let client = consolidator.client.clone();
    let model = consolidator.model.clone();
    let system = consolidator.system.as_deref();
    let max_tokens = consolidator.max_tokens;
    let max_input_tokens = consolidator.max_input_tokens;

    let instructions = consolidator_instructions(workflow_rules);
    let request = ConsolidatorRequest {
        task: "consolidate_lenses",
        task_brief,
        lens_outputs,
        instructions: &instructions,
    };
    let request_text = serde_json::to_string(&request)?;

    let mut cfg = CallConfig::defaults_for(model.clone())
        .with_max_tokens(max_tokens)
        .with_stream_label("consolidator");
    if let Some(s) = system {
        cfg = cfg.with_system(s.to_string());
    }
    if let Some(n) = max_input_tokens {
        cfg = cfg.with_max_input_tokens(n);
    }
    if let Some(thinking) = consolidator.thinking {
        cfg = cfg.with_thinking(thinking);
    }

    // Consolidator is one-shot per task; tail cache would never be
    // read. Skip the +25% write tax.
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
            Some("phase=consolidate"),
            &messages[0].content,
            None,
            None,
            Some(&request),
        );
    }
    kres_core::async_eprintln!(
        "[consolidator] merging {} lens output(s)",
        lens_outputs.len()
    );
    let resp = if let Some(shutdown) = shutdown.clone() {
        tokio::select! {
            _ = shutdown.cancelled() => return Err(AgentError::Other("cancelled during consolidation".into())),
            result = client.messages_streaming(&cfg, &messages) => result,
        }
    } else {
        client.messages_streaming(&cfg, &messages).await
    }
    .map_err(AgentError::from)?;
    if let Some(usage) = &consolidator.usage {
        usage.record(
            "consolidator",
            model.id.clone(),
            resp.usage.input_tokens,
            resp.usage.output_tokens,
            resp.usage.cache_creation_input_tokens,
            resp.usage.cache_read_input_tokens,
        );
    }

    let mut text = extract_text(&resp);
    if let Some(lg) = &logger {
        let mut thinking = String::new();
        for b in &resp.content {
            if let kres_llm::request::ContentBlock::Thinking { thinking: t } = b {
                thinking.push_str(t);
            }
        }
        // Same label as the user record above. Without it the response is
        // unattributable: a by-stage token accounting silently folded every
        // consolidate and promote call into the goal/todo bucket.
        lg.log_code_labeled_with_model(
            "assistant",
            Some("phase=consolidate"),
            &text,
            Some(LoggedUsage {
                input: resp.usage.input_tokens,
                output: resp.usage.output_tokens,
                cache_creation: resp.usage.cache_creation_input_tokens,
                cache_read: resp.usage.cache_read_input_tokens,
            }),
            if thinking.is_empty() {
                None
            } else {
                Some(&thinking)
            },
            resp.model.as_deref(),
        );
    }
    let response_contract =
        CodeResponseContract::new(["comparison".to_string(), "comparison_details".to_string()]);
    let response_schema = response_contract
        .schema_json_for(&[
            "analysis",
            "findings",
            "followups",
            "comparison",
            "comparison_details",
        ])
        .to_string();
    // A structurally valid envelope owns malformed Finding entries at the
    // per-record repair boundary. Other envelope failures get exactly one
    // whole-response repair and never fall through to a second repair path.
    let tolerant_contract = response_contract.clone().allowing_invalid_findings();
    let mut parsed = match tolerant_contract.validate(&text) {
        Ok(parsed) => parsed,
        Err(envelope_errors) => {
            let repaired = match crate::json_repair::repair_json_response(crate::json_repair::JsonRepairCall {
            client: client.clone(),
            model: model.clone(),
            max_tokens,
            max_input_tokens,
            thinking: consolidator.thinking,
            contract: crate::json_repair::JsonContract {
                name: "lens-consolidator",
                schema: &response_schema,
                instructions: "Preserve every lens conclusion, finding id, followup, and comparison. Correct representation and field types only.",
            },
            rejected_response: &text,
            validation_errors: &envelope_errors,
            logger: logger.clone(),
            log_kind: crate::json_repair::RepairLogKind::Code,
            shutdown: shutdown.clone(),
        })
        .await
            {
                Ok(repaired) => repaired,
                Err(error) => {
                    tracing::warn!(target: "kres_agents", "consolidator JSON repair failed: {error}; using deterministic lens union");
                    return Ok(naive_fallback(lens_outputs));
                }
            };
            if let Some(usage) = &consolidator.usage {
                usage.record(
                    "consolidator",
                    model.id.clone(),
                    repaired.usage.input_tokens,
                    repaired.usage.output_tokens,
                    repaired.usage.cache_creation_input_tokens,
                    repaired.usage.cache_read_input_tokens,
                );
            }
            let repaired_parsed = match tolerant_contract.accept_repair(&repaired.text) {
                Ok(parsed) => parsed,
                Err(errors) => {
                    tracing::warn!(target: "kres_agents", "consolidator JSON repair remained invalid: {}; using deterministic lens union", errors.join("; "));
                    return Ok(naive_fallback(lens_outputs));
                }
            };
            text = repaired.text;
            repaired_parsed
        }
    };
    log_json_normalization(logger.as_deref(), &parsed, "lens-consolidator");
    if !parsed.invalid_findings.is_empty() {
        let existing_findings = lens_outputs
            .iter()
            .flat_map(|output| output.findings.iter().cloned())
            .collect::<Vec<_>>();
        let outcome = crate::finding_repair::repair_invalid_findings(
            client.clone(),
            model.clone(),
            max_tokens,
            max_input_tokens,
            std::mem::take(&mut parsed.invalid_findings),
            &existing_findings,
            crate::finding_repair::FindingRepairRuntime {
                logger: logger.clone(),
                thinking: consolidator.thinking,
                cancel: shutdown.map(crate::finding_repair::FindingRepairCancel::Shutdown),
                usage: consolidator.usage.clone(),
                role: "consolidator",
            },
        )
        .await?;
        parsed.merge_repaired_findings(outcome.findings);
        if !outcome.unrepaired.is_empty() {
            let note = crate::finding_repair::format_unrepaired_findings(&outcome.unrepaired);
            if !parsed.analysis.is_empty() {
                parsed.analysis.push_str("\n\n");
            }
            parsed.analysis.push_str(&note);
            parsed.followups.push(Followup {
                kind: "question".into(),
                name: "Repair the malformed Finding records preserved in the preceding analysis"
                    .into(),
                reason: "[MISSING] Rust rejected Finding fields after one schema-repair attempt; re-emit the same claims with valid typed fields before completion"
                    .into(),
                path: None,
                required_for_progress: true,
            });
        }
    }
    let comparison = extract_comparison(&text);
    // §20g: when findings parsed OK but analysis is empty, fall back
    // to the naive-concat narrative while keeping the parsed
    // findings. Prevents the operator from seeing an empty prose block
    // alongside a populated findings list.
    if !parsed.findings.is_empty() && parsed.analysis.is_empty() {
        let naive = naive_fallback(lens_outputs);
        return Ok(ConsolidatedTask {
            analysis: naive.analysis,
            findings: parsed.findings,
            followups: parsed.followups,
            comparison: comparison.or(naive.comparison),
        });
    }
    if !parsed.analysis.is_empty() || !parsed.findings.is_empty() || !parsed.followups.is_empty() {
        return Ok(ConsolidatedTask {
            analysis: parsed.analysis,
            findings: parsed.findings,
            followups: parsed.followups,
            comparison,
        });
    }
    Ok(naive_fallback(lens_outputs))
}

fn consolidator_instructions(workflow_rules: Option<&str>) -> String {
    match workflow_rules {
        Some(rules) if !rules.trim().is_empty() => format!(
            "{}\n\nWORKFLOW-SPECIFIC CONSOLIDATION RULES:\n{}",
            CONSOLIDATOR_INSTRUCTIONS,
            rules.trim()
        ),
        _ => CONSOLIDATOR_INSTRUCTIONS.to_string(),
    }
}

/// Deterministic fallback: concat per-lens analyses with `## Lens:
/// [type] name` headers, union findings by id (first-lens-wins).
///
/// keeps duplicate findings so the consolidator's
/// DEDUP-ACROSS-LENSES rule fires; we dedup here to
/// match the kres AgentRunner's "consolidator-optional" design where
/// the fallback result is what actually reaches the operator. If you
/// switch to calling an LLM consolidator unconditionally, drop this
/// dedup so duplicates reach the merge step.
pub fn naive_fallback(lens_outputs: &[LensOutput<'_>]) -> ConsolidatedTask {
    let mut parts = Vec::new();
    for out in lens_outputs.iter() {
        if !out.analysis.is_empty() {
            let kind = out
                .lens
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("investigate");
            let name = out.lens.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let model = out.slow_model.unwrap_or("unknown");
            parts.push(format!(
                "## Lens: [{kind}] {name} ({model})\n\n{}",
                out.analysis
            ));
        }
    }
    let mut seen_ids = std::collections::BTreeSet::new();
    let mut unified = Vec::new();
    for out in lens_outputs {
        for f in out.findings {
            if seen_ids.insert(f.id.clone()) {
                unified.push(f.clone());
            }
        }
    }
    let mut seen_followups = std::collections::BTreeSet::new();
    let mut followups = Vec::new();
    for out in lens_outputs {
        for f in out.followups {
            if seen_followups.insert(f.cache_key()) {
                followups.push(f.clone());
            }
        }
    }
    ConsolidatedTask {
        analysis: parts.join("\n\n---\n\n"),
        findings: unified,
        followups,
        comparison: Some(naive_comparison(lens_outputs)),
    }
}

fn extract_comparison(text: &str) -> Option<Value> {
    let normalized = crate::response::normalized_code_response_json(text).ok()?;
    let value =
        crate::json_repair::parse_strict_json::<Value>("lens-consolidator", &normalized).ok()?;
    value
        .get("comparison")
        .cloned()
        .or_else(|| value.get("comparison_details").cloned())
}

fn naive_comparison(lens_outputs: &[LensOutput<'_>]) -> Value {
    let per_output: Vec<Value> = lens_outputs
        .iter()
        .map(|out| {
            json!({
                "lens": out.lens,
                "slow_model": out.slow_model.unwrap_or("unknown"),
                "analysis_chars": out.analysis.len(),
                "finding_count": out.findings.len(),
                "followup_count": out.followups.len(),
            })
        })
        .collect();
    json!({
        "mode": "deterministic_fallback",
        "note": "LLM comparison unavailable; recorded structural output counts.",
        "outputs": per_output,
    })
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
    use kres_core::findings::{Finding, Severity, Status};
    use kres_llm::{client::Client, Model};
    use serde_json::json;

    fn f(id: &str) -> Finding {
        Finding {
            id: id.to_string(),
            title: id.to_string(),
            severity: Severity::Low,
            status: Status::Active,
            relevant_symbols: vec![],
            relevant_file_sections: vec![],
            summary: "".into(),
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

    #[test]
    fn fallback_unions_findings_by_id() {
        let a = f("a");
        let b1 = f("b");
        let b2 = f("b");
        let lens1_findings = vec![a, b1];
        let lens2_findings = vec![b2, f("c")];
        let no_followups = Vec::new();
        let lens1 = json!({"name": "memory"});
        let lens2 = json!({"name": "races"});
        let outs = vec![
            LensOutput {
                lens: &lens1,
                slow_model: Some("model-a"),
                analysis: "A narrative",
                findings: &lens1_findings,
                followups: &no_followups,
            },
            LensOutput {
                lens: &lens2,
                slow_model: Some("model-b"),
                analysis: "B narrative",
                findings: &lens2_findings,
                followups: &no_followups,
            },
        ];
        let ct = naive_fallback(&outs);
        assert_eq!(ct.findings.len(), 3); // a, b (first wins), c
        let ids: Vec<&str> = ct.findings.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
        assert!(ct.analysis.contains("A narrative"));
        assert!(ct.analysis.contains("B narrative"));
    }

    #[test]
    fn fallback_unions_followups_by_cache_key() {
        let f1 = Followup {
            kind: "read".into(),
            name: "kernel/a.c:1+20".into(),
            reason: "needed by lens".into(),
            path: None,
            required_for_progress: true,
        };
        let f2 = f1.clone();
        let f3 = Followup {
            kind: "source".into(),
            name: "foo".into(),
            reason: "needed by lens".into(),
            path: None,
            required_for_progress: true,
        };
        let empty_findings = Vec::new();
        let lens1_followups = vec![f1, f3.clone()];
        let lens2_followups = vec![f2];
        let lens1 = json!({"name": "memory"});
        let lens2 = json!({"name": "bounds"});
        let outs = vec![
            LensOutput {
                lens: &lens1,
                slow_model: Some("model-a"),
                analysis: "A",
                findings: &empty_findings,
                followups: &lens1_followups,
            },
            LensOutput {
                lens: &lens2,
                slow_model: Some("model-b"),
                analysis: "B",
                findings: &empty_findings,
                followups: &lens2_followups,
            },
        ];

        let ct = naive_fallback(&outs);
        assert_eq!(ct.followups.len(), 2);
        assert!(ct.followups.iter().any(|f| f.cache_key() == f3.cache_key()));
    }

    #[test]
    fn comparison_extraction_uses_normalized_response() {
        let text = r#"{"analysis":"done","comparison":{"winner":"actual"}}"#;
        assert_eq!(extract_comparison(text), Some(json!({"winner":"actual"})));
        let fenced = format!("```json\n{text}\n```");
        assert_eq!(
            extract_comparison(&fenced),
            Some(json!({"winner":"actual"}))
        );
        assert_eq!(
            extract_comparison(&format!("prose\n{fenced}")),
            Some(json!({"winner":"actual"}))
        );
    }

    #[test]
    fn consolidator_rejects_transport_wrapper() {
        assert!(CodeResponseContract::default()
            .validate(r#"{"result":{"analysis":"real","findings":[],"followups":[]}}"#)
            .is_err());
    }

    #[test]
    fn workflow_rules_extend_consolidator_instructions() {
        let rules = "Return full Finding records.";
        let instructions = consolidator_instructions(Some(rules));

        assert!(instructions.contains(CONSOLIDATOR_INSTRUCTIONS.trim()));
        assert!(instructions.contains("WORKFLOW-SPECIFIC CONSOLIDATION RULES"));
        assert!(instructions.contains(rules));
        assert!(instructions.contains("unsupported negative coverage claims"));
    }

    #[test]
    fn consolidate_empty_input_returns_empty() {
        let _ct = futures::executor::block_on(async {
            let consolidator = crate::pipeline::ConsolidatorClient {
                client: Arc::new(Client::new("sk-unused").unwrap()),
                model: Model::opus_4_7(),
                system: None,
                max_tokens: 32_000,
                max_input_tokens: None,
                thinking: None,
                usage: None,
            };
            consolidate_lenses(&consolidator, "test", &[]).await
        })
        .unwrap();
    }
}
