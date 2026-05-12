use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use kres_core::io::async_println;
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

pub fn inputs_for_target_with_results(
    workflow: &Workflow,
    target: &str,
    results_dir: Option<&Path>,
) -> Map<String, Value> {
    let mut inputs = Map::new();
    inputs.insert(
        target_input_key(workflow),
        Value::String(target.to_string()),
    );
    apply_results_artifact_dir(workflow, &mut inputs, results_dir);
    derive_inputs(workflow, inputs)
}

pub fn apply_results_artifact_dir(
    workflow: &Workflow,
    inputs: &mut Map<String, Value>,
    results_dir: Option<&Path>,
) {
    if !workflow.inputs.contains_key("target_artifact_dir")
        || inputs.contains_key("target_artifact_dir")
    {
        return;
    }
    let Some(dir) = results_dir else {
        return;
    };

    let derived = derive_inputs(workflow, inputs.clone());
    if derived.get("target_kind").and_then(Value::as_str) == Some("prose") {
        inputs.insert(
            "target_artifact_dir".into(),
            Value::String(absolute_path(dir).display().to_string()),
        );
    }
}

fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
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

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct FixSeriesTodo {
    id: String,
    title: String,
    scope: String,
    #[serde(default)]
    affected_files: Vec<String>,
    #[serde(default)]
    affected_symbols: Vec<String>,
    fix_contract: String,
    rationale: String,
    #[serde(default)]
    depends_on: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FixTodoStatus {
    Pending,
    InProgress,
    Done,
    Failed,
}

#[derive(Debug, Clone)]
struct TrackedFixTodo {
    todo: FixSeriesTodo,
    status: FixTodoStatus,
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

    if workflow.id == "fix"
        && !inputs.contains_key("current_fix_todo")
        && !inputs.contains_key("fix_series_plan")
    {
        if resume || state_dir.is_some() {
            return Err(anyhow::anyhow!(
                "fix series workflow does not support workflow snapshot --resume/--state-dir yet"
            ));
        }
        let trace = run_fix_series_driver(
            workflow,
            driver,
            inputs.clone(),
            iteration_cap,
            observer.clone(),
        )
        .await;
        let written_artifacts = if let Some(results_dir) = results_dir.as_ref() {
            kres_agents::workflow_runner::write_workflow_artefacts(results_dir, workflow, &trace)?
        } else {
            Vec::new()
        };
        return Ok(WorkflowRunResult {
            trace,
            written_artifacts,
        });
    }

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

async fn run_fix_series_driver(
    workflow: &Workflow,
    driver: &mut LlmDriver,
    inputs: Map<String, Value>,
    iteration_cap: usize,
    observer: Option<EventObserver>,
) -> Trace {
    let mut planning_workflow = workflow.clone();
    planning_workflow
        .steps
        .retain(|s| matches!(s.id.as_str(), "research" | "invalidate" | "unconfirm"));
    planning_workflow.completion = None;
    let mut planning_inputs = inputs.clone();
    planning_inputs.insert("fix_run_mode".into(), Value::String("planning".to_string()));

    let planning_trace = match observer.clone() {
        Some(obs) => {
            run_with_observer(
                &planning_workflow,
                driver,
                planning_inputs,
                iteration_cap,
                obs,
            )
            .await
        }
        None => run_with_cap(&planning_workflow, driver, planning_inputs, iteration_cap).await,
    };

    if !matches!(
        planning_trace.status,
        WorkflowStatus::Success | WorkflowStatus::TerminalSuccess(_)
    ) {
        return planning_trace;
    }

    let research_status = step_output_string(&planning_trace, "research", "research_status");
    if research_status.as_deref() != Some("confirmed") {
        async_println(format!(
            "[fix series] planning stopped with research_status={}",
            research_status
                .as_deref()
                .unwrap_or("<missing research_status>")
        ));
        return planning_trace;
    }

    let plan = match parse_fix_plan(&planning_trace) {
        Ok(plan) => plan,
        Err(e) => {
            let mut trace = planning_trace;
            trace.status = WorkflowStatus::Failure(format!("research.fix_plan invalid: {e}"));
            return trace;
        }
    };
    if plan.is_empty() {
        let mut trace = planning_trace;
        trace.status = WorkflowStatus::Failure(
            "research.fix_plan invalid: confirmed research must return at least one fix todo"
                .to_string(),
        );
        return trace;
    }
    async_println(format!(
        "[fix series] research confirmed; {} fix todo(s) planned",
        plan.len()
    ));
    for (idx, todo) in plan.iter().enumerate() {
        async_println(format!(
            "[fix series] plan {}/{} {}",
            idx + 1,
            plan.len(),
            summarize_fix_todo(todo)
        ));
    }

    let mut events = planning_trace.events;
    let mut final_state = planning_trace.final_state;
    let mut status = WorkflowStatus::Success;
    let mut tracked: Vec<TrackedFixTodo> = plan
        .into_iter()
        .map(|todo| TrackedFixTodo {
            todo,
            status: FixTodoStatus::Pending,
        })
        .collect();
    let fix_series_plan = serde_json::to_value(
        tracked
            .iter()
            .map(|tracked| &tracked.todo)
            .collect::<Vec<_>>(),
    )
    .unwrap_or(Value::Array(Vec::new()));

    for (idx, tracked_todo) in tracked.iter_mut().enumerate() {
        tracked_todo.status = FixTodoStatus::InProgress;
        async_println(format!(
            "[fix series] start {}/{} {}",
            idx + 1,
            fix_series_plan.as_array().map(Vec::len).unwrap_or_default(),
            summarize_fix_todo(&tracked_todo.todo)
        ));
        let mut item_inputs = inputs.clone();
        item_inputs.insert("fix_series_plan".into(), fix_series_plan.clone());
        item_inputs.insert(
            "current_fix_todo".into(),
            serde_json::to_value(&tracked_todo.todo).unwrap_or(Value::Null),
        );
        item_inputs.insert(
            "fix_index".into(),
            Value::Number(serde_json::Number::from((idx + 1) as u64)),
        );
        item_inputs.insert("fix_run_mode".into(), Value::String("todo".to_string()));

        let item_trace = run_with_optional_observer(
            workflow,
            driver,
            item_inputs,
            iteration_cap,
            observer.clone(),
        )
        .await;
        let item_research_status = step_output_string(&item_trace, "research", "research_status");
        let item_status = item_trace.status;
        let item_workflow_ok = matches!(
            item_status,
            WorkflowStatus::Success | WorkflowStatus::TerminalSuccess(_)
        );
        let item_research_confirmed = item_research_status.as_deref() == Some("confirmed");
        let item_ok = item_workflow_ok && item_research_confirmed;
        tracked_todo.status = if item_ok {
            FixTodoStatus::Done
        } else {
            FixTodoStatus::Failed
        };
        let item_label = if item_ok { "done" } else { "failed" };
        async_println(format!(
            "[fix series] {item_label} {}/{} {}",
            idx + 1,
            fix_series_plan.as_array().map(Vec::len).unwrap_or_default(),
            tracked_todo.todo.id
        ));
        events.extend(item_trace.events);
        final_state = item_trace.final_state;
        status = if item_workflow_ok && !item_research_confirmed {
            WorkflowStatus::Failure(format!(
                "fix todo '{}' research_status was {}, expected confirmed",
                tracked_todo.todo.id,
                item_research_status
                    .as_deref()
                    .unwrap_or("<missing research_status>")
            ))
        } else {
            item_status
        };
        if !item_ok {
            break;
        }
    }

    Trace {
        events,
        status,
        final_state,
    }
}

async fn run_with_optional_observer(
    workflow: &Workflow,
    driver: &mut LlmDriver,
    inputs: Map<String, Value>,
    iteration_cap: usize,
    observer: Option<EventObserver>,
) -> Trace {
    match observer {
        Some(obs) => run_with_observer(workflow, driver, inputs, iteration_cap, obs).await,
        None => run_with_cap(workflow, driver, inputs, iteration_cap).await,
    }
}

fn parse_fix_plan(trace: &Trace) -> Result<Vec<FixSeriesTodo>, String> {
    let Some(raw) = trace
        .final_state
        .get("research")
        .and_then(|st| st.outputs.get("fix_plan"))
    else {
        return Ok(Vec::new());
    };
    let todos: Vec<FixSeriesTodo> = serde_json::from_value(raw.clone())
        .map_err(|e| format!("expected array of fix todo objects: {e}"))?;
    validate_fix_plan(&todos)?;
    Ok(todos)
}

fn step_output_string(trace: &Trace, step: &str, field: &str) -> Option<String> {
    trace
        .final_state
        .get(step)
        .and_then(|st| st.outputs.get(field))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn validate_fix_plan(todos: &[FixSeriesTodo]) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for (idx, todo) in todos.iter().enumerate() {
        if todo.id.trim().is_empty() {
            return Err(format!("todo {} has empty id", idx + 1));
        }
        for (field, value) in [
            ("title", &todo.title),
            ("scope", &todo.scope),
            ("fix_contract", &todo.fix_contract),
            ("rationale", &todo.rationale),
        ] {
            if value.trim().is_empty() {
                return Err(format!("todo '{}' has empty {field}", todo.id));
            }
        }
        for dep in &todo.depends_on {
            if !seen.contains(dep) {
                return Err(format!(
                    "todo '{}' depends_on '{}' which is not an earlier todo",
                    todo.id, dep
                ));
            }
        }
        if !seen.insert(todo.id.clone()) {
            return Err(format!("duplicate todo id '{}'", todo.id));
        }
    }
    Ok(())
}

fn summarize_fix_todo(todo: &FixSeriesTodo) -> String {
    let mut out = format!(
        "{} - {}",
        todo.id,
        truncate_display(&one_line(&todo.title), 160)
    );
    if !todo.scope.trim().is_empty() {
        out.push_str(&format!(
            "\n    scope: {}",
            truncate_display(&one_line(&todo.scope), 220)
        ));
    }
    if !todo.fix_contract.trim().is_empty() {
        out.push_str(&format!(
            "\n    fix: {}",
            truncate_display(&one_line(&todo.fix_contract), 220)
        ));
    }
    if !todo.affected_files.is_empty() {
        let files = todo
            .affected_files
            .iter()
            .take(5)
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let tail = if todo.affected_files.len() > 5 {
            format!(", +{} more", todo.affected_files.len() - 5)
        } else {
            String::new()
        };
        out.push_str(&format!("\n    files: {files}{tail}"));
    }
    out
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_display(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{}...", truncated.trim_end())
    } else {
        truncated
    }
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
    fn inputs_for_target_with_results_sets_artifact_dir_for_prose() {
        let workflow = lookup_workflow(None, "fix").unwrap();
        let results = std::env::temp_dir().join("kres-results-artifacts");
        let derived = inputs_for_target_with_results(&workflow, "fix this bug", Some(&results));
        assert_eq!(derived.get("target_kind"), Some(&json!("prose")));
        assert_eq!(
            derived.get("target_artifact_dir"),
            Some(&json!(absolute_path(&results).display().to_string()))
        );
    }

    #[test]
    fn inputs_for_target_with_results_uses_finding_dir_over_results() {
        let workflow = lookup_workflow(None, "fix").unwrap();
        let tmp = std::env::temp_dir().join(format!(
            "kres-finding-dir-input-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("metadata.yaml"), "id: x\n").unwrap();
        std::fs::write(tmp.join("FINDING.md"), "# x\n").unwrap();
        let results = std::env::temp_dir().join("kres-results-artifacts");
        let derived =
            inputs_for_target_with_results(&workflow, tmp.to_str().unwrap(), Some(&results));
        assert_eq!(derived.get("target_kind"), Some(&json!("finding_dir")));
        assert_eq!(derived.get("target_artifact_dir"), derived.get("target"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn inputs_for_target_with_results_uses_tilde_finding_dir_over_results() {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return;
        };
        let workflow = lookup_workflow(None, "fix").unwrap();
        let tmp = home.join(format!(
            ".kres-finding-dir-input-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("metadata.yaml"), "id: x\n").unwrap();
        std::fs::write(tmp.join("FINDING.md"), "# x\n").unwrap();

        let suffix = tmp.strip_prefix(&home).unwrap();
        let target = format!("~/{}", suffix.display());
        let results = std::env::temp_dir().join("kres-results-artifacts");
        let derived = inputs_for_target_with_results(&workflow, &target, Some(&results));
        assert_eq!(derived.get("target_kind"), Some(&json!("finding_dir")));
        assert_eq!(derived.get("target_artifact_dir"), derived.get("target"));
        assert_ne!(
            derived.get("target_artifact_dir"),
            Some(&json!(absolute_path(&results).display().to_string()))
        );

        let _ = std::fs::remove_dir_all(&tmp);
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

    fn series_todo(id: &str, depends_on: Vec<String>) -> FixSeriesTodo {
        FixSeriesTodo {
            id: id.to_string(),
            title: format!("todo {id}"),
            scope: "scope".to_string(),
            affected_files: Vec::new(),
            affected_symbols: Vec::new(),
            fix_contract: "contract".to_string(),
            rationale: "rationale".to_string(),
            depends_on,
        }
    }

    #[test]
    fn fix_series_plan_validation_rejects_brittle_shapes() {
        assert!(validate_fix_plan(&[series_todo("first", Vec::new())]).is_ok());
        assert!(validate_fix_plan(&[
            series_todo("first", Vec::new()),
            series_todo("second", vec!["first".to_string()])
        ])
        .is_ok());
        assert!(validate_fix_plan(&[
            series_todo("dup", Vec::new()),
            series_todo("dup", Vec::new())
        ])
        .unwrap_err()
        .contains("duplicate"));
        assert!(
            validate_fix_plan(&[series_todo("late", vec!["missing".to_string()])])
                .unwrap_err()
                .contains("not an earlier todo")
        );
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
