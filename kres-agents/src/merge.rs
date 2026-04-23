//! Cross-task findings merge pass.
//!
//! Contract owed to bugs.md#H1: the CALLER is responsible for NOT
//! holding the findings-extract lock across this function. The
//! function itself performs a single fast-agent API call and returns
//! the merged list. Inside kres-core::TaskManager::with_findings_
//! extract_lock, you should call this function BEFORE taking the
//! lock, then take the lock only for the subsequent disk write.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use kres_core::findings::Finding;
use kres_core::log::{LoggedUsage, TurnLogger};
use kres_llm::{client::Client, config::CallConfig, request::Message, Model};

use crate::{error::AgentError, response::parse_code_response};

pub const MERGER_INSTRUCTIONS: &str = include_str!("prompts/merger.txt");

/// Dedicated system prompt for the merger. The merger used to
/// inherit the fast-code-agent's system prompt (via
/// ConsolidatorClient.system, which is cloned from fast_cfg.system)
/// and rely entirely on MERGER_INSTRUCTIONS embedded in the user
/// message to switch modes. Observed in session cddd1764:
/// occasionally the model ignored the embedded instructions and
/// responded as the fast-code-agent's system prompt directed —
/// emitting {"goal":"..."} shapes or <action> tags — which
/// parse_code_response couldn't lift into a findings list,
/// triggering the empty-list retry. Swapping in a judge-mode
/// system prompt that hard-restricts the merger to {"findings":
/// [...]} eliminates that drift surface.
pub const MERGER_SYSTEM: &str = include_str!("prompts/merger_system.txt");

#[derive(Debug, Serialize)]
struct MergeRequest<'a> {
    task: &'static str,
    task_brief: &'a str,
    task_findings: &'a [Finding],
    current_findings: &'a [Finding],
    instructions: &'a str,
}

#[derive(Debug, Deserialize)]
struct MergeResponse {
    #[serde(default)]
    findings: Vec<Finding>,
}

pub async fn merge_findings(
    client: Arc<Client>,
    model: Model,
    system: Option<&str>,
    max_tokens: u32,
    task_brief: &str,
    task_findings: &[Finding],
    current_findings: &[Finding],
) -> Result<Vec<Finding>, AgentError> {
    merge_findings_with_logger(
        client,
        model,
        system,
        max_tokens,
        None,
        task_brief,
        task_findings,
        current_findings,
        None,
    )
    .await
}

/// Same as [`merge_findings`] but logs user+assistant turns on the
/// provided TurnLogger's main.jsonl.
#[allow(clippy::too_many_arguments)]
pub async fn merge_findings_with_logger(
    client: Arc<Client>,
    model: Model,
    system: Option<&str>,
    max_tokens: u32,
    max_input_tokens: Option<u32>,
    task_brief: &str,
    task_findings: &[Finding],
    current_findings: &[Finding],
    logger: Option<Arc<TurnLogger>>,
) -> Result<Vec<Finding>, AgentError> {
    // No task-delta → nothing to merge. Skip the API call entirely.
    if task_findings.is_empty() {
        return Ok(current_findings.to_vec());
    }

    // Cap task_brief at 300 chars.
    let brief_capped: String = task_brief.chars().take(300).collect();
    let request = MergeRequest {
        task: "merge_findings",
        task_brief: &brief_capped,
        task_findings,
        current_findings,
        instructions: MERGER_INSTRUCTIONS,
    };
    let request_text = serde_json::to_string(&request)?;

    let mut cfg = CallConfig::defaults_for(model.clone())
        .with_max_tokens(max_tokens)
        .with_stream_label("merge findings");
    if let Some(s) = system {
        cfg = cfg.with_system(s.to_string());
    }
    if let Some(n) = max_input_tokens {
        cfg = cfg.with_max_input_tokens(n);
    }

    // bugs.md#M2: one retry on transient flake before falling back to
    // the deterministic union. Each attempt is a full API call.
    // Common case is a single successful call, so the tail cache
    // almost never gets a second reader — skip the +25% write tax.
    let messages = vec![Message {
        role: "user".into(),
        content: request_text,
        cache: false,
        cached_prefix: None,
    }];
    for attempt in 0..2 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
        if let Some(lg) = &logger {
            lg.log_main("user", &messages[0].content, None, None);
        }
        let resp_result = client.messages_streaming(&cfg, &messages).await;
        let resp = match resp_result {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    target: "kres_agents",
                    attempt,
                    "merge_findings api call failed: {e}"
                );
                continue;
            }
        };
        let text = extract_text(&resp);
        if let Some(lg) = &logger {
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
        let parsed = parse_code_response(&text);
        if !parsed.findings.is_empty() {
            return Ok(parsed.findings);
        }
        if let Ok(r) = serde_json::from_str::<MergeResponse>(&text) {
            if !r.findings.is_empty() {
                return Ok(r.findings);
            }
        }
        tracing::warn!(
            target: "kres_agents",
            attempt,
            "merge_findings parsed to empty list, retrying"
        );
    }
    // Never silently drop the task delta because the merge failed.
    // Union the inputs as a safe fallback so operators can reconcile.
    tracing::warn!(
        target: "kres_agents",
        "merge_findings fell back to deterministic union after retries"
    );
    Ok(naive_union(current_findings, task_findings))
}

/// Deterministic fallback: current ∪ task (task-wins on id collision).
pub fn naive_union(current: &[Finding], task: &[Finding]) -> Vec<Finding> {
    use std::collections::BTreeMap;
    let mut by_id: BTreeMap<String, Finding> = BTreeMap::new();
    // Preserve order: current first, then task overrides.
    let mut order: Vec<String> = Vec::new();
    for f in current {
        if by_id.insert(f.id.clone(), f.clone()).is_none() {
            order.push(f.id.clone());
        }
    }
    for f in task {
        if by_id.insert(f.id.clone(), f.clone()).is_none() {
            order.push(f.id.clone());
        }
    }
    order
        .into_iter()
        .filter_map(|id| by_id.remove(&id))
        .collect()
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
            severity: Severity::Low,
            status: Status::Active,
            relevant_symbols: vec![],
            relevant_file_sections: vec![],
            summary: String::new(),
            reproducer_sketch: "r".into(),
            impact: "i".into(),
            mechanism_detail: None,
            fix_sketch: None,
            open_questions: vec![],
            first_seen_task: None,
            last_updated_task: None,
            related_finding_ids: vec![],
        }
    }

    #[tokio::test]
    async fn empty_task_returns_current_unchanged() {
        let c = Arc::new(Client::new("sk-unused").unwrap());
        let current = vec![f("a"), f("b")];
        let out = merge_findings(c, Model::opus_4_7(), None, 16_000, "brief", &[], &current)
            .await
            .unwrap();
        let ids: Vec<&str> = out.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn naive_union_task_wins_on_id_collision() {
        let mut current = vec![f("a"), f("b")];
        current[0].summary = "current-a".into();
        let mut task = vec![f("a"), f("c")];
        task[0].summary = "task-a".into();
        let out = naive_union(&current, &task);
        let ids: Vec<&str> = out.iter().map(|f| f.id.as_str()).collect();
        // Order: a (task-overwrites) — preserving current's slot, b, c.
        assert_eq!(ids, vec!["a", "b", "c"]);
        assert_eq!(out[0].summary, "task-a");
    }

    #[test]
    fn naive_union_preserves_current_only_entries() {
        let current = vec![f("a"), f("b")];
        let task: Vec<Finding> = vec![];
        let out = naive_union(&current, &task);
        let ids: Vec<&str> = out.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"]);
    }
}
