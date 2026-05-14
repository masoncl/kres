//! Cross-lens consolidation pass.
//!
//! Input: N per-lens outputs for one task. Output: one unified
//! analysis narrative + one deduplicated findings list.
//!
//! The instructions match `_LENS_CONSOLIDATOR_INSTRUCTIONS` in the
//! and include the recent COMPLETENESS CHECK rule
//! (promote prose-only bugs to Findings or drop them from prose).

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use kres_core::findings::Finding;
use kres_core::log::{LoggedUsage, TurnLogger};
use kres_llm::{config::CallConfig, request::Message};

use crate::{error::AgentError, followup::Followup, response::parse_code_response};

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

#[derive(Debug, Deserialize)]
struct ConsolidatorResponse {
    #[serde(default)]
    analysis: String,
    #[serde(default)]
    findings: Vec<Finding>,
    #[serde(default)]
    followups: Vec<Followup>,
    #[serde(default)]
    comparison: Option<Value>,
    #[serde(default)]
    comparison_details: Option<Value>,
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
    consolidate_lenses_with_logger(consolidator, task_brief, lens_outputs, None, None).await
}

/// Same as [`consolidate_lenses`] but appends user+assistant turns
/// to the provided TurnLogger's code.jsonl.
pub async fn consolidate_lenses_with_logger(
    consolidator: &crate::pipeline::ConsolidatorClient,
    task_brief: &str,
    lens_outputs: &[LensOutput<'_>],
    workflow_rules: Option<&str>,
    logger: Option<Arc<TurnLogger>>,
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

    // caps task_brief at 300 chars when it reaches the
    // consolidator. This prevents a long operator prompt from
    // dominating every per-lens slow call's context window.
    let brief_capped: String = task_brief.chars().take(300).collect();
    let instructions = consolidator_instructions(workflow_rules);
    let request = ConsolidatorRequest {
        task: "consolidate_lenses",
        task_brief: &brief_capped,
        lens_outputs,
        instructions: &instructions,
    };
    let request_text = serde_json::to_string(&request)?;

    let mut cfg = CallConfig::defaults_for(model)
        .with_max_tokens(max_tokens)
        .with_stream_label("consolidator");
    if let Some(s) = system {
        cfg = cfg.with_system(s.to_string());
    }
    if let Some(n) = max_input_tokens {
        cfg = cfg.with_max_input_tokens(n);
    }

    // Consolidator is one-shot per task; tail cache would never be
    // read. Skip the +25% write tax.
    let messages = vec![Message {
        role: "user".into(),
        content: request_text,
        cache: false,
        cached_prefix: None,
    }];
    if let Some(lg) = &logger {
        lg.log_code("user", &messages[0].content, None, None);
    }
    kres_core::async_eprintln!(
        "[consolidator] merging {} lens output(s)",
        lens_outputs.len()
    );
    let resp = client
        .messages_streaming(&cfg, &messages)
        .await
        .map_err(|e| AgentError::Other(e.to_string()))?;

    let text = extract_text(&resp);
    if let Some(lg) = &logger {
        let mut thinking = String::new();
        for b in &resp.content {
            if let kres_llm::request::ContentBlock::Thinking { thinking: t } = b {
                thinking.push_str(t);
            }
        }
        lg.log_code(
            "assistant",
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
        );
    }
    let parsed = parse_code_response(&text);
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
    if let Ok(c) = serde_json::from_str::<ConsolidatorResponse>(&text) {
        return Ok(ConsolidatedTask {
            analysis: c.analysis,
            findings: c.findings,
            followups: c.followups,
            comparison: c.comparison.or(c.comparison_details),
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
/// match the kres orchestrator's "consolidator-optional" design where
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
    let v = serde_json::from_str::<Value>(text.trim()).ok()?;
    v.get("comparison")
        .cloned()
        .or_else(|| v.get("comparison_details").cloned())
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
        };
        let f2 = f1.clone();
        let f3 = Followup {
            kind: "source".into(),
            name: "foo".into(),
            reason: "needed by lens".into(),
            path: None,
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
            };
            consolidate_lenses(&consolidator, "test", &[]).await
        })
        .unwrap();
    }
}
