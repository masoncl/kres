use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Map, Value};

use kres_agents::workflow_exec::{
    active_lenses, run_resume, run_with_cap, run_with_observer, run_with_persistence,
    EventObserver, ExecContext, Trace, WorkflowSnapshot, WorkflowStatus,
};
use kres_agents::workflow_runner::{derive_inputs, LlmDriver};
use kres_agents::{
    workflow::{lens_to_spec, load_workflow, lookup_workflow, Workflow},
    PromptFile,
};

pub struct ReviewPromptConfig {
    pub source: String,
    pub prompt_file: PromptFile,
    pub consolidate_rules: Option<String>,
}

/// Resolve a workflow either from a path or from the `workflow-id:<id>`
/// sentinel used by batch `--prompt` dispatch.
pub fn load_workflow_path_or_id(path: &Path, kres_dir: Option<&Path>) -> Result<Workflow> {
    if let Some(id) = path.to_str().and_then(|s| s.strip_prefix("workflow-id:")) {
        let override_dir = kres_dir.map(|d| d.join("workflows"));
        lookup_workflow(override_dir.as_deref(), id)
            .with_context(|| format!("looking up workflow id '{id}'"))
    } else {
        load_workflow(path).with_context(|| format!("loading workflow {}", path.display()))
    }
}

/// Pick the workflow input that receives slash-command target text.
///
/// Convention:
/// - workflow with `target` input: use `target`
/// - workflow with one required input: use that input
/// - otherwise use `target`
pub fn target_input_key(workflow: &Workflow) -> String {
    if workflow.inputs.contains_key("target") {
        return "target".to_string();
    }
    let required: Vec<&String> = workflow
        .inputs
        .iter()
        .filter(|(_, v)| v.get("required").and_then(|r| r.as_bool()).unwrap_or(false))
        .map(|(k, _)| k)
        .collect();
    match required.len() {
        1 => required[0].clone(),
        _ => "target".to_string(),
    }
}

pub fn inputs_for_target(workflow: &Workflow, target: &str) -> Map<String, Value> {
    let mut inputs = Map::new();
    inputs.insert(
        target_input_key(workflow),
        Value::String(target.to_string()),
    );
    derive_inputs(workflow, inputs)
}

pub fn workflow_prompt_invocation(raw: &str) -> Option<(&str, &str)> {
    let trimmed = raw.trim();
    let (id, rest) = if let Some(after_slash) = trimmed.strip_prefix('/') {
        match after_slash.split_once(char::is_whitespace) {
            Some((h, r)) => (h.trim(), r.trim()),
            None => (after_slash.trim(), ""),
        }
    } else if let Some((h, r)) = trimmed.split_once(':') {
        (h.trim(), r.trim())
    } else {
        return None;
    };
    if id.is_empty() {
        None
    } else {
        Some((id, rest))
    }
}

pub fn review_prompt_file_from_prompt(
    raw: &str,
    kres_dir: Option<&Path>,
) -> Result<Option<ReviewPromptConfig>> {
    let Some((id, target)) = workflow_prompt_invocation(raw) else {
        return Ok(None);
    };
    if id != "review" {
        return Ok(None);
    }
    Ok(Some(review_prompt_file_from_target(target, kres_dir)?))
}

pub fn review_prompt_file_from_target(
    target: &str,
    kres_dir: Option<&Path>,
) -> Result<ReviewPromptConfig> {
    let workflow = load_workflow_path_or_id(Path::new("workflow-id:review"), kres_dir)?;
    let inputs = inputs_for_target(&workflow, target);
    let step = workflow
        .steps
        .first()
        .ok_or_else(|| anyhow::anyhow!("review workflow has no steps"))?;

    let mut prompt = String::new();
    prompt.push_str("Run the JSON-defined review workflow through the task/todo loop.\n\n");
    prompt.push_str("TARGET: ");
    prompt.push_str(target);
    prompt.push_str("\n\n");
    prompt.push_str(
        "Target semantics: if TARGET is a git ref such as HEAD, a commit SHA, \
or a range, review the changes introduced by that ref/range. Start by \
fetching `git show --stat` and `git show`/`git diff` for the target, then \
gather the changed files and symbols. Do not survey or audit the whole \
repository unless the operator explicitly asks for a whole-tree audit.\n\n",
    );
    for include in &step.include {
        if let Some(key) = include
            .strip_prefix("{{globals.")
            .and_then(|s| s.strip_suffix("}}"))
        {
            if let Some(s) = workflow.globals.get(key).and_then(|v| v.as_str()) {
                prompt.push_str(s);
                prompt.push_str("\n\n");
            }
        }
    }
    if let Some(p) = step.prompt.as_deref() {
        for line in p.lines() {
            if line.contains("{{lens.")
                || line.contains("{{workflow.target}}")
                || line.contains("LENS:")
            {
                continue;
            }
            prompt.push_str(line);
            prompt.push('\n');
        }
    }

    let steps = HashMap::new();
    let ctx = ExecContext {
        workflow_inputs: &inputs,
        steps: &steps,
    };
    let lenses = active_lenses(step, &ctx)
        .map_err(|e| anyhow::anyhow!("selecting review lenses: {e}"))?
        .into_iter()
        .map(lens_to_spec)
        .collect();

    Ok(ReviewPromptConfig {
        source: "workflow-id:review task loop".to_string(),
        prompt_file: PromptFile { prompt, lenses },
        consolidate_rules: step.consolidate.as_ref().map(|c| c.prompt.clone()),
    })
}

pub struct WorkflowRunOptions {
    pub iteration_cap: usize,
    pub state_dir: Option<PathBuf>,
    pub resume: bool,
    pub results_dir: Option<PathBuf>,
    pub observer: Option<EventObserver>,
}

impl Default for WorkflowRunOptions {
    fn default() -> Self {
        Self {
            iteration_cap: 200,
            state_dir: None,
            resume: false,
            results_dir: None,
            observer: None,
        }
    }
}

pub struct WorkflowRunResult {
    pub trace: Trace,
    pub written_artifacts: Vec<PathBuf>,
}

pub async fn run_workflow_driver(
    workflow: &Workflow,
    driver: &mut LlmDriver,
    inputs: Map<String, Value>,
    options: WorkflowRunOptions,
) -> Result<WorkflowRunResult> {
    let WorkflowRunOptions {
        iteration_cap,
        state_dir,
        resume,
        results_dir,
        observer,
    } = options;

    let trace = match (resume, state_dir.as_ref(), observer) {
        (true, Some(state_dir), None) => {
            let snap = WorkflowSnapshot::load(state_dir, &workflow.id).ok_or_else(|| {
                anyhow::anyhow!(
                    "no snapshot found at {}/workflow-{}.json",
                    state_dir.display(),
                    workflow.id
                )
            })?;
            run_resume(
                workflow,
                driver,
                snap,
                Some(state_dir.clone()),
                iteration_cap,
            )
            .await
        }
        (true, None, _) => {
            return Err(anyhow::anyhow!("--resume requires --state-dir DIR"));
        }
        (false, Some(state_dir), None) => {
            run_with_persistence(workflow, driver, inputs, iteration_cap, state_dir.clone()).await
        }
        (false, None, Some(observer)) => {
            run_with_observer(workflow, driver, inputs, iteration_cap, observer).await
        }
        (false, Some(_), Some(_)) => {
            return Err(anyhow::anyhow!(
                "workflow observer cannot be combined with persisted state yet"
            ));
        }
        (true, Some(_), Some(_)) => {
            return Err(anyhow::anyhow!(
                "workflow observer cannot be combined with resume yet"
            ));
        }
        (false, None, None) => run_with_cap(workflow, driver, inputs, iteration_cap).await,
    };

    let written_artifacts = if let Some(results_dir) = results_dir.as_ref() {
        kres_agents::workflow_runner::write_workflow_artefacts(results_dir, workflow, &trace)?
    } else {
        Vec::new()
    };

    Ok(WorkflowRunResult {
        trace,
        written_artifacts,
    })
}

pub fn workflow_status_result(status: &WorkflowStatus) -> Result<()> {
    match status {
        WorkflowStatus::Success | WorkflowStatus::TerminalSuccess(_) => Ok(()),
        WorkflowStatus::Failure(msg) => Err(anyhow::anyhow!("workflow failed: {msg}")),
        WorkflowStatus::IterationCap(cap) => {
            Err(anyhow::anyhow!("workflow hit iteration cap of {cap} steps"))
        }
    }
}

pub fn workflow_status_label(status: &WorkflowStatus) -> String {
    match status {
        WorkflowStatus::Success => "Success".to_string(),
        WorkflowStatus::TerminalSuccess(reason) => format!("TerminalSuccess ({reason})"),
        WorkflowStatus::Failure(msg) => format!("Failure — {msg}"),
        WorkflowStatus::IterationCap(cap) => format!("IterationCap ({cap})"),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Arc;

    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;

    use super::*;
    use kres_agents::workflow_runner::{AgentEnv, LlmDriver};
    use kres_llm::client::Client;

    fn workflow_with_inputs(inputs: Map<String, Value>) -> Workflow {
        Workflow {
            schema_version: 1,
            schema_url: None,
            format: None,
            id: "test".to_string(),
            title: None,
            description: None,
            inputs,
            skills: Vec::new(),
            globals: Map::new(),
            defaults: Default::default(),
            steps: Vec::new(),
            completion: None,
            persistence: None,
        }
    }

    #[test]
    fn target_input_key_prefers_target() {
        let mut inputs = Map::new();
        inputs.insert("target".to_string(), json!({"required": true}));
        inputs.insert("path".to_string(), json!({"required": true}));
        let workflow = workflow_with_inputs(inputs);
        assert_eq!(target_input_key(&workflow), "target");
    }

    #[test]
    fn target_input_key_uses_single_required_input() {
        let mut inputs = Map::new();
        inputs.insert("path".to_string(), json!({"required": true}));
        inputs.insert("mode".to_string(), json!({"required": false}));
        let workflow = workflow_with_inputs(inputs);
        assert_eq!(target_input_key(&workflow), "path");
    }

    #[test]
    fn target_input_key_falls_back_to_target() {
        let workflow = workflow_with_inputs(Map::new());
        assert_eq!(target_input_key(&workflow), "target");
    }

    #[test]
    fn review_prompt_file_uses_embedded_json_lenses() {
        let cfg = review_prompt_file_from_target("HEAD", None).expect("embedded review workflow");
        assert_eq!(cfg.prompt_file.lenses.len(), 6);
        assert_eq!(cfg.prompt_file.lenses[0].id, "lifetime");
        assert!(cfg.prompt_file.lenses.iter().any(|l| l.id == "assertions"));
        assert!(cfg.prompt_file.prompt.contains("TARGET: HEAD"));
        assert!(cfg.prompt_file.prompt.contains("full Finding records"));
        assert!(cfg.prompt_file.prompt.contains("target diff/stat"));
        assert!(cfg.prompt_file.prompt.contains("Do not enumerate"));
        assert!(!cfg.prompt_file.prompt.contains("Knot Resolver"));
        assert!(!cfg.prompt_file.prompt.contains("{{lens."));
        assert!(!cfg.prompt_file.prompt.contains("{{workflow.target}}"));
        assert!(cfg
            .consolidate_rules
            .as_deref()
            .unwrap_or_default()
            .contains("Merge the per-lens outputs"));
    }

    #[test]
    fn review_prompt_file_omits_commit_assertions_for_file_targets() {
        let cfg =
            review_prompt_file_from_target("drivers/example/example.c", None).expect("workflow");
        assert_eq!(cfg.prompt_file.lenses.len(), 5);
        assert!(!cfg.prompt_file.lenses.iter().any(|l| l.id == "assertions"));
    }

    #[test]
    fn review_prompt_file_from_prompt_only_handles_review() {
        assert!(review_prompt_file_from_prompt("/review HEAD", None)
            .unwrap()
            .is_some());
        assert!(review_prompt_file_from_prompt("fix: HEAD", None)
            .unwrap()
            .is_none());
        assert!(review_prompt_file_from_prompt("plain question", None)
            .unwrap()
            .is_none());
    }

    fn fake_messages_response(text: &str) -> Value {
        json!({
            "id": "msg_test",
            "type": "message",
            "role": "assistant",
            "model": "claude-test",
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": {
                "input_tokens": 10,
                "output_tokens": 20,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0
            },
            "content": [{"type": "text", "text": text}]
        })
    }

    async fn spawn_mock(responses: VecDeque<Value>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let queue = Arc::new(Mutex::new(responses));
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 65536];
                let _ = socket.read(&mut buf).await;
                let body = {
                    let mut q = queue.lock().await;
                    match q.pop_front() {
                        Some(v) => v.to_string(),
                        None => json!({"error": "no more canned responses"}).to_string(),
                    }
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(resp.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        port
    }

    fn fast_env_pointing_at(port: u16) -> AgentEnv {
        let client = Client::builder("test-key")
            .base_url(format!("http://127.0.0.1:{port}"))
            .no_proxy()
            .build()
            .unwrap();
        AgentEnv::new(
            Arc::new(client),
            "claude-haiku-4-5-20251001",
            4_096,
            Some("test system prompt".to_string()),
        )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_workflow_driver_writes_results_dir_artifacts() {
        let wf_json = json!({
            "$schema_version": 1,
            "id": "repl-results-test",
            "steps": [{
                "id": "scan",
                "agent": "fast",
                "prompt": "scan the target",
                "outputs": {
                    "findings": {"type": "array<object>"},
                    "analysis": {"type": "string"}
                }
            }]
        });
        let workflow = kres_agents::workflow::parse_workflow(&wf_json.to_string()).unwrap();
        let responses = VecDeque::from(vec![fake_messages_response(
            "found one\n\
             {\"findings\": [{\"file\": \"kernel/a.c:7\", \"what\": \"leak\"}], \
              \"analysis\": \"one leak\"}",
        )]);
        let port = spawn_mock(responses).await;

        let mut driver = LlmDriver::new(std::env::temp_dir(), workflow.clone())
            .with_fast(fast_env_pointing_at(port));
        let results = {
            let mut p = std::env::temp_dir();
            p.push(format!("kres-repl-results-test-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            p
        };
        let run = run_workflow_driver(
            &workflow,
            &mut driver,
            Map::new(),
            WorkflowRunOptions {
                iteration_cap: 20,
                results_dir: Some(results.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert!(matches!(
            run.trace.status,
            WorkflowStatus::Success | WorkflowStatus::TerminalSuccess(_)
        ));
        assert_eq!(run.written_artifacts.len(), 2);
        assert!(results.join("findings.json").exists());
        assert!(results.join("report.md").exists());
        let report = std::fs::read_to_string(results.join("report.md")).unwrap();
        assert!(report.contains("repl-results-test"));
        assert!(report.contains("one leak"));
        let _ = std::fs::remove_dir_all(results);
    }
}
