use std::collections::{BTreeSet, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use kres_core::io::async_println;
use serde_json::{Map, Value};

use kres_agents::workflow_exec::{
    active_lenses, run_resume, run_with_cap, run_with_observer, run_with_persistence,
    run_with_persistence_and_observer, EventObserver, ExecContext, StepStatus, Trace,
    WorkflowSnapshot, WorkflowStatus,
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
    pub file_scan_target: Option<String>,
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

fn load_fix_series_original_artifacts(inputs: &Map<String, Value>) -> Result<String, String> {
    if inputs.get("target_kind").and_then(Value::as_str) != Some("finding_dir") {
        return Ok(String::new());
    }
    let Some(dir) = inputs
        .get("target_artifact_dir")
        .and_then(Value::as_str)
        .filter(|dir| !dir.trim().is_empty())
    else {
        return Err("finding-dir assessment is missing target_artifact_dir".to_string());
    };
    let dir = Path::new(dir);
    let canonical_dir = std::fs::canonicalize(dir).map_err(|err| {
        format!(
            "could not resolve finding directory {}: {err}",
            dir.display()
        )
    })?;
    let mut artifacts = String::new();
    for (name, required) in [
        ("metadata.yaml", true),
        ("FINDING.md", true),
        ("summary.md", false),
    ] {
        let path = dir.join(name);
        match std::fs::File::open(&path) {
            Ok(mut file) => {
                let resolved = opened_file_path(&file, &path)?;
                if !resolved.starts_with(&canonical_dir) {
                    return Err(format!(
                        "original fix artifact {} resolves outside finding directory {}",
                        path.display(),
                        canonical_dir.display()
                    ));
                }
                let mut body = String::new();
                file.read_to_string(&mut body).map_err(|err| {
                    format!(
                        "could not read original fix artifact {}: {err}",
                        path.display()
                    )
                })?;
                artifacts.push_str(&format!("--- ORIGINAL ARTIFACT: {name} ---\n"));
                artifacts.push_str(body.trim_end());
                artifacts.push_str("\n\n");
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound && !required => {}
            Err(err) => {
                return Err(format!(
                    "could not preload original fix artifact {}: {err}",
                    path.display()
                ));
            }
        }
    }
    Ok(artifacts)
}

#[cfg(target_os = "linux")]
fn opened_file_path(file: &std::fs::File, original: &Path) -> Result<PathBuf, String> {
    use std::os::fd::AsRawFd;

    std::fs::canonicalize(format!("/proc/self/fd/{}", file.as_raw_fd())).map_err(|err| {
        format!(
            "could not resolve opened original fix artifact {}: {err}",
            original.display()
        )
    })
}

#[cfg(target_os = "macos")]
fn opened_file_path(file: &std::fs::File, original: &Path) -> Result<PathBuf, String> {
    use std::ffi::CStr;
    use std::os::fd::AsRawFd;

    let mut path = [0 as libc::c_char; libc::PATH_MAX as usize];
    let result = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, path.as_mut_ptr()) };
    if result == -1 {
        return Err(format!(
            "could not resolve opened original fix artifact {}: {}",
            original.display(),
            std::io::Error::last_os_error()
        ));
    }
    let resolved = unsafe { CStr::from_ptr(path.as_ptr()) };
    let resolved = PathBuf::from(resolved.to_string_lossy().into_owned());
    std::fs::canonicalize(&resolved).map_err(|err| {
        format!(
            "could not resolve opened original fix artifact {}: {err}",
            original.display()
        )
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn opened_file_path(_file: &std::fs::File, original: &Path) -> Result<PathBuf, String> {
    Err(format!(
        "secure opened-file resolution is unsupported on this platform for {}",
        original.display()
    ))
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
    let target_is_commit = inputs
        .get("target_is_commit")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let target_is_file = !target_is_commit && looks_like_source_file(target);
    if target_is_commit {
        prompt.push_str(
            "\nTARGET KIND: git commit or range\n\n\
Review the changes introduced by exactly this ref/range. Start by fetching \
`git show --stat` and `git show`/`git diff` for TARGET, then gather the changed \
files, symbols, and unchanged contract consumers.\n\n",
        );
    } else if target_is_file {
        prompt.push_str(
            "\nTARGET KIND: current-workspace source scope (not a git ref)\n\n\
Review the current source named by TARGET. There is no implied commit, range, \
base revision, or target diff. Do not invent one and do not request `git show` \
or `git diff` merely to establish scope. Before goal and plan creation, generate \
one rename-aware target-file diff covering the last six months, assess that net diff \
with one low-effort change survey (chunking it when necessary), survey the file \
exactly once, then have one non-lensed slow call combine the structural and change \
ratings. Use that ranking to build the initial semantic \
coverage plan. Later review tasks \
gather targeted function bodies, types, callers, and line ranges. Request git history only for a specific semantic \
question that source alone cannot answer.\n\n",
        );
    } else {
        prompt.push_str(
            "\nTARGET KIND: current-workspace source scope (not a git ref)\n\n\
Review the current source scope named by TARGET. There is no implied commit, range, \
base revision, or target diff. Do not invent one. Gather a bounded source inventory \
appropriate to the target, then obtain targeted bodies, types, callers, and line ranges. \
Request git history only for a specific semantic question source cannot answer.\n\n",
        );
    }
    prompt.push_str(
        "Do not survey or audit the whole repository unless the operator \
explicitly asks for a whole-tree audit.\n\n",
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
        file_scan_target: target_is_file.then(|| target.to_string()),
    })
}

fn looks_like_source_file(target: &str) -> bool {
    let path = Path::new(target);
    !target.chars().any(char::is_whitespace)
        && path.file_name().is_some()
        && path.extension().is_some()
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

const MAX_FIX_TODO_REVISIONS: usize = 3;
const MAX_PLAN_UPDATE_REVISIONS: usize = 3;
const MAX_FINAL_PLAN_REVISIONS: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct FixSeriesBug {
    id: String,
    description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum FixTodoStatus {
    Pending,
    InProgress,
    Done,
    Failed,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct TrackedFixTodo {
    todo: FixSeriesTodo,
    status: FixTodoStatus,
    #[serde(default)]
    commit_sha: Option<String>,
    #[serde(default)]
    outcomes: Vec<Value>,
    #[serde(default)]
    is_latent: bool,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FixPlanUpdate {
    expected_revision: u64,
    operations: Vec<FixPlanOperation>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum FixPlanOperation {
    ReplaceCurrent { todo: FixSeriesTodo },
    SplitCurrent { todos: Vec<FixSeriesTodo> },
    AppendAfterCurrent { todos: Vec<FixSeriesTodo> },
    RevisePending { todo: FixSeriesTodo },
    RemovePending { id: String },
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct FixSeriesState {
    schema_version: u32,
    target: String,
    plan_revision: u64,
    plan_update_revisions: usize,
    final_revisions: usize,
    base_head: String,
    original_bugs: Vec<FixSeriesBug>,
    #[serde(default)]
    original_artifacts: String,
    tracked: Vec<TrackedFixTodo>,
    todo_revisions: Vec<usize>,
}

fn validate_original_bugs(bugs: &[FixSeriesBug]) -> Result<(), String> {
    if bugs.is_empty() {
        return Err("fix series must preserve at least one original bug".to_string());
    }
    let mut ids = std::collections::HashSet::new();
    for bug in bugs {
        if bug.id.trim().is_empty() {
            return Err("original bug id must not be empty".to_string());
        }
        if !ids.insert(&bug.id) {
            return Err(format!("duplicate original bug id '{}'", bug.id));
        }
    }
    Ok(())
}

fn parse_original_bugs(
    trace: &Trace,
    inputs: &Map<String, Value>,
) -> Result<Vec<FixSeriesBug>, String> {
    if inputs.get("target_kind").and_then(Value::as_str) == Some("finding_dir") {
        if let Some(dir) = inputs
            .get("target_artifact_dir")
            .and_then(Value::as_str)
            .filter(|dir| !dir.is_empty())
        {
            let existing = kres_core::read_finding_bugs(Path::new(dir))
                .map_err(|e| format!("reading original bugs from {dir}: {e}"))?;
            if !existing.is_empty() {
                let bugs = existing
                    .into_iter()
                    .map(|bug| FixSeriesBug {
                        id: bug.id,
                        description: bug.description,
                    })
                    .collect::<Vec<_>>();
                validate_original_bugs(&bugs)?;
                return Ok(bugs);
            }
        }
    }
    let value = trace
        .final_state
        .get("research")
        .and_then(|state| state.outputs.get("bug_inventory"))
        .ok_or_else(|| "confirmed planning research must return bug_inventory".to_string())?;
    let bugs: Vec<FixSeriesBug> = serde_json::from_value(value.clone())
        .map_err(|e| format!("research.bug_inventory invalid: {e}"))?;
    validate_original_bugs(&bugs)?;
    Ok(bugs)
}

impl FixSeriesState {
    const SCHEMA_VERSION: u32 = 2;

    fn path(dir: &Path) -> PathBuf {
        dir.join("fix-series.json")
    }

    fn load(dir: &Path, target: &str) -> Result<Self, String> {
        let path = Self::path(dir);
        let body = std::fs::read_to_string(&path)
            .map_err(|e| format!("reading {}: {e}", path.display()))?;
        let mut state: Self =
            serde_json::from_str(&body).map_err(|e| format!("parsing {}: {e}", path.display()))?;
        if state.schema_version != Self::SCHEMA_VERSION {
            return Err(format!(
                "fix series snapshot schema {} is unsupported (expected {})",
                state.schema_version,
                Self::SCHEMA_VERSION
            ));
        }
        if state.target != target {
            return Err("fix series snapshot target does not match this run".to_string());
        }
        if state.todo_revisions.len() != state.tracked.len() {
            return Err("fix series snapshot revision vector has wrong length".to_string());
        }
        if state.base_head.trim().is_empty() {
            return Err("fix series snapshot has empty base_head".to_string());
        }
        validate_original_bugs(&state.original_bugs)?;
        for item in &mut state.tracked {
            if item.status == FixTodoStatus::InProgress {
                item.status = FixTodoStatus::Pending;
            }
        }
        validate_fix_plan(
            &state
                .tracked
                .iter()
                .map(|item| item.todo.clone())
                .collect::<Vec<_>>(),
        )?;
        if let Some(item) = state
            .tracked
            .iter()
            .find(|item| item.status == FixTodoStatus::Done && item.commit_sha.is_none())
        {
            return Err(format!(
                "completed todo '{}' has no persisted commit SHA",
                item.todo.id
            ));
        }
        Ok(state)
    }

    fn save(&self, dir: &Path) -> Result<(), String> {
        use std::io::Write;

        std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
        let path = Self::path(dir);
        let tmp = dir.join("fix-series.json.tmp");
        let body = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)
            .map_err(|e| format!("opening {}: {e}", tmp.display()))?;
        file.write_all(&body)
            .and_then(|_| file.sync_all())
            .map_err(|e| format!("writing {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &path)
            .map_err(|e| format!("renaming {} to {}: {e}", tmp.display(), path.display()))?;
        std::fs::File::open(dir)
            .and_then(|file| file.sync_all())
            .map_err(|e| format!("syncing {}: {e}", dir.display()))
    }
}

fn reconcile_fix_series(
    state: &FixSeriesState,
    state_dir: Option<&Path>,
    inputs: &Map<String, Value>,
) -> Result<(), String> {
    if let Some(dir) = state_dir {
        state.save(dir)?;
    }
    let Some(artifact_dir) = inputs
        .get("target_artifact_dir")
        .and_then(Value::as_str)
        .filter(|dir| !dir.is_empty())
    else {
        return Ok(());
    };
    let bugs = state
        .original_bugs
        .iter()
        .map(|bug| kres_core::FindingBug {
            id: bug.id.clone(),
            description: bug.description.clone(),
        })
        .collect::<Vec<_>>();
    kres_core::set_finding_bugs(Path::new(artifact_dir), &bugs)
        .map_err(|e| format!("reconciling fix series bugs in {artifact_dir}: {e}"))?;
    Ok(())
}

async fn git_rev_parse(workspace: &Path, revision: &str) -> Result<String, String> {
    let output = tokio::process::Command::new("git")
        .current_dir(workspace)
        .args(["rev-parse", revision])
        .output()
        .await
        .map_err(|e| format!("git rev-parse {revision} spawn: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git rev-parse {revision} exited {}: {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn validate_series_commit_chain(
    workspace: &Path,
    state: &FixSeriesState,
) -> Result<Vec<String>, String> {
    let commits: Vec<String> = state
        .tracked
        .iter()
        .map(|item| {
            item.commit_sha
                .clone()
                .filter(|sha| !sha.is_empty())
                .ok_or_else(|| {
                    format!(
                        "completed todo '{}' has no recorded commit SHA",
                        item.todo.id
                    )
                })
        })
        .collect::<Result<_, _>>()?;
    let mut expected_parent = state.base_head.clone();
    for commit in &commits {
        let parent = git_rev_parse(workspace, &format!("{commit}^")).await?;
        if parent != expected_parent {
            return Err(format!(
                "fix commit {commit} has parent {parent}, expected {expected_parent}"
            ));
        }
        expected_parent = commit.clone();
    }
    let head = git_rev_parse(workspace, "HEAD").await?;
    if head != expected_parent {
        return Err(format!(
            "workspace HEAD {head} does not match final fix commit {expected_parent}"
        ));
    }
    Ok(commits)
}

async fn validate_completed_prefix(
    workspace: &Path,
    state: &FixSeriesState,
    current_snapshot: Option<&WorkflowSnapshot>,
) -> Result<(), String> {
    let mut expected_head = state.base_head.clone();
    for item in state
        .tracked
        .iter()
        .take_while(|item| item.status == FixTodoStatus::Done)
    {
        let commit = item
            .commit_sha
            .as_deref()
            .filter(|sha| !sha.is_empty())
            .ok_or_else(|| format!("completed todo '{}' has no commit SHA", item.todo.id))?;
        let parent = git_rev_parse(workspace, &format!("{commit}^")).await?;
        if parent != expected_head {
            return Err(format!(
                "completed fix commit {commit} has parent {parent}, expected {expected_head}"
            ));
        }
        expected_head = commit.to_string();
    }
    let head = git_rev_parse(workspace, "HEAD").await?;
    if head == expected_head {
        return Ok(());
    }
    let snapshot_owns_head = current_snapshot.is_some_and(|snapshot| {
        snapshot.steps.iter().any(|step| {
            (step.id == "commit"
                && step.outputs.get("commit_sha").and_then(Value::as_str) == Some(head.as_str()))
                || (step.id == "write-patch"
                    && step
                        .outputs
                        .get("review_dispute")
                        .and_then(Value::as_str)
                        .is_some_and(|dispute| !dispute.is_empty())
                    && snapshot
                        .steps
                        .iter()
                        .any(|step| step.id == "commit" && step.status == StepStatus::Skipped))
        })
    });
    if snapshot_owns_head {
        let parent = git_rev_parse(workspace, &format!("{head}^")).await?;
        if parent == expected_head {
            return Ok(());
        }
    }
    Err(format!(
        "workspace HEAD {head} does not match completed fix prefix {expected_head}"
    ))
}

fn todo_snapshot_key(id: &str) -> String {
    let slug: String = id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '-'
            }
        })
        .take(48)
        .collect();
    let hash = id
        .as_bytes()
        .iter()
        .fold(0xcbf29ce484222325u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        });
    format!("{slug}-{hash:016x}")
}

fn apply_fix_plan_update(
    state: &mut FixSeriesState,
    current_idx: usize,
    update: FixPlanUpdate,
) -> Result<(), String> {
    if state.plan_update_revisions >= MAX_PLAN_UPDATE_REVISIONS {
        return Err(format!(
            "fix plan exceeded structural revision budget ({MAX_PLAN_UPDATE_REVISIONS})"
        ));
    }
    if update.expected_revision != state.plan_revision {
        return Err(format!(
            "stale fix plan update: expected revision {}, current revision is {}",
            update.expected_revision, state.plan_revision
        ));
    }
    if update.operations.is_empty() {
        return Err("fix plan update has no operations".to_string());
    }

    let original_current = state.tracked[current_idx].todo.clone();
    let mut tracked = state.tracked.clone();
    let mut revisions = state.todo_revisions.clone();
    let mut current_revised = false;
    let mut append_at = current_idx + 1;
    for operation in update.operations {
        match operation {
            FixPlanOperation::ReplaceCurrent { todo } => {
                if current_revised {
                    return Err("fix plan update may revise or split current only once".to_string());
                }
                if todo.id != tracked[current_idx].todo.id {
                    return Err("replace_current must preserve the current todo id".to_string());
                }
                tracked[current_idx].todo = todo;
                tracked[current_idx].status = FixTodoStatus::Pending;
                current_revised = true;
            }
            FixPlanOperation::SplitCurrent { todos } => {
                if current_revised {
                    return Err("fix plan update may revise or split current only once".to_string());
                }
                if todos.is_empty() {
                    return Err("split_current requires at least one todo".to_string());
                }
                let count = todos.len();
                tracked.splice(
                    current_idx..=current_idx,
                    todos.into_iter().map(|todo| TrackedFixTodo {
                        todo,
                        status: FixTodoStatus::Pending,
                        commit_sha: None,
                        outcomes: Vec::new(),
                        is_latent: false,
                    }),
                );
                revisions.splice(current_idx..=current_idx, std::iter::repeat(0).take(count));
                current_revised = true;
                append_at = current_idx + count;
            }
            FixPlanOperation::AppendAfterCurrent { todos } => {
                if !current_revised {
                    return Err(
                        "append_after_current must follow replace_current or split_current"
                            .to_string(),
                    );
                }
                if todos.is_empty() {
                    return Err("append_after_current requires at least one todo".to_string());
                }
                let count = todos.len();
                tracked.splice(
                    append_at..append_at,
                    todos.into_iter().map(|todo| TrackedFixTodo {
                        todo,
                        status: FixTodoStatus::Pending,
                        commit_sha: None,
                        outcomes: Vec::new(),
                        is_latent: false,
                    }),
                );
                revisions.splice(append_at..append_at, std::iter::repeat(0).take(count));
                append_at += count;
            }
            FixPlanOperation::RevisePending { todo } => {
                let Some((idx, item)) = tracked
                    .iter_mut()
                    .enumerate()
                    .skip(current_idx + 1)
                    .find(|(_, item)| item.todo.id == todo.id)
                else {
                    return Err(format!(
                        "revise_pending names unknown pending todo '{}'",
                        todo.id
                    ));
                };
                if item.status != FixTodoStatus::Pending {
                    return Err(format!("todo '{}' is not pending", todo.id));
                }
                item.todo = todo;
                revisions[idx] = revisions[idx].saturating_add(1);
            }
            FixPlanOperation::RemovePending { id } => {
                let Some(idx) = tracked
                    .iter()
                    .enumerate()
                    .skip(current_idx + 1)
                    .find_map(|(idx, item)| (item.todo.id == id).then_some(idx))
                else {
                    return Err(format!("remove_pending names unknown pending todo '{id}'"));
                };
                if tracked[idx].status != FixTodoStatus::Pending {
                    return Err(format!("todo '{id}' is not pending"));
                }
                tracked.remove(idx);
                revisions.remove(idx);
            }
        }
    }
    if !current_revised {
        return Err("fix plan update must revise or split the current todo".to_string());
    }
    validate_fix_plan(
        &tracked
            .iter()
            .map(|item| item.todo.clone())
            .collect::<Vec<_>>(),
    )?;
    if tracked
        .get(current_idx)
        .is_some_and(|item| item.todo == original_current)
    {
        return Err("fix plan update did not revise or split the current todo".to_string());
    }
    if tracked[..current_idx]
        .iter()
        .any(|item| item.status != FixTodoStatus::Done)
    {
        return Err("fix plan update modified completed-prefix state".to_string());
    }
    state.tracked = tracked;
    state.todo_revisions = revisions;
    state.plan_revision = state.plan_revision.saturating_add(1);
    state.plan_update_revisions += 1;
    Ok(())
}

fn apply_final_fix_plan_update(
    state: &mut FixSeriesState,
    update: FixPlanUpdate,
) -> Result<(), String> {
    if state.final_revisions >= MAX_FINAL_PLAN_REVISIONS {
        return Err(format!(
            "final fix plan exceeded revision budget ({MAX_FINAL_PLAN_REVISIONS})"
        ));
    }
    if update.expected_revision != state.plan_revision {
        return Err(format!(
            "stale final fix plan update: expected revision {}, current revision is {}",
            update.expected_revision, state.plan_revision
        ));
    }
    if update.operations.is_empty() {
        return Err("final fix plan update has no operations".to_string());
    }
    let mut appended = Vec::new();
    for operation in update.operations {
        match operation {
            FixPlanOperation::AppendAfterCurrent { todos } if !todos.is_empty() => {
                appended.extend(todos);
            }
            _ => {
                return Err(
                    "final fix plan update may contain only non-empty append_after_current operations"
                        .to_string(),
                );
            }
        }
    }
    let mut tracked = state.tracked.clone();
    tracked.extend(appended.into_iter().map(|todo| TrackedFixTodo {
        todo,
        status: FixTodoStatus::Pending,
        commit_sha: None,
        outcomes: Vec::new(),
        is_latent: false,
    }));
    validate_fix_plan(
        &tracked
            .iter()
            .map(|item| item.todo.clone())
            .collect::<Vec<_>>(),
    )?;
    let added = tracked.len() - state.tracked.len();
    state.tracked = tracked;
    state
        .todo_revisions
        .extend(std::iter::repeat(0).take(added));
    state.plan_revision = state.plan_revision.saturating_add(1);
    state.final_revisions += 1;
    Ok(())
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
    let fallback_state_dir = driver.workspace().join(".kres/workflow-state");
    let default_state_dir = state_dir
        .clone()
        .or_else(|| results_dir.clone())
        .unwrap_or(fallback_state_dir);

    if workflow.id == "fix"
        && !inputs.contains_key("current_fix_todo")
        && !inputs.contains_key("fix_series_plan")
    {
        let trace = run_fix_series_driver(
            workflow,
            driver,
            inputs.clone(),
            iteration_cap,
            observer.clone(),
            Some(&default_state_dir),
            resume,
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

    let trace = match (resume, Some(&default_state_dir), observer) {
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
        (false, Some(state_dir), None) => {
            run_with_persistence(workflow, driver, inputs, iteration_cap, state_dir.clone()).await
        }
        (false, Some(state_dir), Some(observer)) => {
            run_with_persistence_and_observer(
                workflow,
                driver,
                inputs,
                iteration_cap,
                state_dir.clone(),
                observer,
            )
            .await
        }
        (true, Some(_), Some(_)) => {
            return Err(anyhow::anyhow!(
                "workflow observer cannot be combined with resume yet"
            ));
        }
        (true, None, _) | (false, None, _) => unreachable!("default state directory is set"),
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
    results_dir: Option<&Path>,
    resume: bool,
) -> Trace {
    let target = inputs
        .get("target")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut events = Vec::new();
    let mut final_state = HashMap::new();
    let mut status = WorkflowStatus::Success;
    let outer_state_exists = results_dir.is_some_and(|dir| FixSeriesState::path(dir).exists());
    let mut series_state = if resume && outer_state_exists {
        match FixSeriesState::load(results_dir.expect("state directory checked above"), target) {
            Ok(mut state) => {
                if state.original_artifacts.is_empty()
                    && inputs.get("target_kind").and_then(Value::as_str) == Some("finding_dir")
                {
                    match load_fix_series_original_artifacts(&inputs) {
                        Ok(artifacts) => state.original_artifacts = artifacts,
                        Err(e) => {
                            return Trace {
                                events,
                                status: WorkflowStatus::Failure(e),
                                final_state,
                            };
                        }
                    }
                }
                state
            }
            Err(e) => {
                return Trace {
                    events,
                    status: WorkflowStatus::Failure(e),
                    final_state,
                };
            }
        }
    } else {
        let mut planning_workflow = workflow.clone();
        planning_workflow
            .steps
            .retain(|s| matches!(s.id.as_str(), "research" | "invalidate" | "unconfirm"));
        planning_workflow.completion = None;
        let mut planning_inputs = inputs.clone();
        planning_inputs.insert("fix_run_mode".into(), Value::String("planning".to_string()));

        let planning_trace = run_with_optional_resume(
            &planning_workflow,
            driver,
            planning_inputs,
            iteration_cap,
            observer.clone(),
            results_dir.map(|dir| dir.join("workflow-state/planning")),
            resume,
        )
        .await;
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
            Ok(plan) if !plan.is_empty() => plan,
            Ok(_) => {
                let mut trace = planning_trace;
                trace.status = WorkflowStatus::Failure(
                    "research.fix_plan invalid: confirmed research must return at least one fix todo"
                        .to_string(),
                );
                return trace;
            }
            Err(e) => {
                let mut trace = planning_trace;
                trace.status = WorkflowStatus::Failure(format!("research.fix_plan invalid: {e}"));
                return trace;
            }
        };
        let original_bugs = match parse_original_bugs(&planning_trace, &inputs) {
            Ok(bugs) => bugs,
            Err(e) => {
                let mut trace = planning_trace;
                trace.status = WorkflowStatus::Failure(e);
                return trace;
            }
        };
        let original_artifacts = match load_fix_series_original_artifacts(&inputs) {
            Ok(artifacts) => artifacts,
            Err(e) => {
                let mut trace = planning_trace;
                trace.status = WorkflowStatus::Failure(e);
                return trace;
            }
        };
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
        events = planning_trace.events;
        final_state = planning_trace.final_state;
        let base_head = match git_rev_parse(driver.workspace(), "HEAD").await {
            Ok(head) => head,
            Err(e) => {
                return Trace {
                    events,
                    status: WorkflowStatus::Failure(e),
                    final_state,
                };
            }
        };
        let state = FixSeriesState {
            schema_version: FixSeriesState::SCHEMA_VERSION,
            target: target.to_string(),
            plan_revision: 1,
            plan_update_revisions: 0,
            final_revisions: 0,
            base_head,
            original_bugs,
            original_artifacts,
            todo_revisions: vec![0; plan.len()],
            tracked: plan
                .into_iter()
                .map(|todo| TrackedFixTodo {
                    todo,
                    status: FixTodoStatus::Pending,
                    commit_sha: None,
                    outcomes: Vec::new(),
                    is_latent: false,
                })
                .collect(),
        };
        if let Err(e) = reconcile_fix_series(&state, results_dir, &inputs) {
            return Trace {
                events,
                status: WorkflowStatus::Failure(e),
                final_state,
            };
        }
        state
    };

    if let Err(e) = reconcile_fix_series(&series_state, results_dir, &inputs) {
        return Trace {
            events,
            status: WorkflowStatus::Failure(e),
            final_state,
        };
    }

    let mut idx = 0usize;
    'series: loop {
        while idx < series_state.tracked.len() {
            if series_state.tracked[idx].status == FixTodoStatus::Done {
                idx += 1;
                continue;
            }
            let mut revisions = series_state.todo_revisions[idx];
            loop {
                let item_state_dir = results_dir.map(|dir| {
                    dir.join(format!(
                        "workflow-state/todo-{}-{}-plan-{}-revision-{revisions}",
                        idx + 1,
                        todo_snapshot_key(&series_state.tracked[idx].todo.id),
                        series_state.plan_revision
                    ))
                });
                let current_snapshot = resume
                    .then_some(item_state_dir.as_deref())
                    .flatten()
                    .and_then(|dir| WorkflowSnapshot::load(dir, &workflow.id));
                if let Err(e) = validate_completed_prefix(
                    driver.workspace(),
                    &series_state,
                    current_snapshot.as_ref(),
                )
                .await
                {
                    status = WorkflowStatus::Failure(e);
                    break;
                }
                series_state.tracked[idx].status = FixTodoStatus::InProgress;
                if let Err(e) = reconcile_fix_series(&series_state, results_dir, &inputs) {
                    status = WorkflowStatus::Failure(e);
                    break;
                }
                let fix_series_plan = fix_series_plan_value(&series_state.tracked);
                async_println(format!(
                    "[fix series] start {}/{} {}",
                    idx + 1,
                    fix_series_plan.as_array().map(Vec::len).unwrap_or_default(),
                    summarize_fix_todo(&series_state.tracked[idx].todo)
                ));
                let mut item_inputs = inputs.clone();
                item_inputs.insert("fix_series_plan".into(), fix_series_plan.clone());
                item_inputs.insert(
                    "current_fix_todo".into(),
                    serde_json::to_value(&series_state.tracked[idx].todo).unwrap_or(Value::Null),
                );
                item_inputs.insert(
                    "fix_index".into(),
                    Value::Number(serde_json::Number::from((idx + 1) as u64)),
                );
                item_inputs.insert("fix_run_mode".into(), Value::String("todo".to_string()));
                // Series publication is gated by the final whole-series assessment.
                item_inputs.insert("target_artifact_dir".into(), Value::String(String::new()));
                item_inputs.insert(
                    "fix_plan_revision".into(),
                    Value::Number(series_state.plan_revision.into()),
                );

                let item_trace = run_with_optional_resume(
                    workflow,
                    driver,
                    item_inputs,
                    iteration_cap,
                    observer.clone(),
                    item_state_dir,
                    resume,
                )
                .await;
                let item_research_status =
                    step_output_string(&item_trace, "research", "research_status");
                let item_status = item_trace.status.clone();
                let item_workflow_ok = matches!(
                    item_status,
                    WorkflowStatus::Success | WorkflowStatus::TerminalSuccess(_)
                );
                let item_research_confirmed = item_research_status.as_deref() == Some("confirmed");
                let item_ok = item_workflow_ok && item_research_confirmed;

                if item_research_status.as_deref() == Some("invalid") {
                    if let Err(e) = write_partial_invalidation_for_todo(
                        &inputs,
                        &item_trace,
                        &series_state.tracked[idx].todo,
                    ) {
                        async_println(format!(
                            "[fix series] failed to write partial invalidation for {}: {e}",
                            series_state.tracked[idx].todo.id
                        ));
                    }
                }

                if !item_ok && item_research_status.as_deref() == Some("unconfirmed") {
                    match parse_fix_plan_update(&item_trace) {
                        Ok(Some(update)) => {
                            events.extend(item_trace.events);
                            if let Err(e) = apply_fix_plan_update(&mut series_state, idx, update) {
                                status = WorkflowStatus::Failure(format!(
                                    "fix todo '{}' plan update invalid: {e}",
                                    series_state.tracked[idx].todo.id
                                ));
                                break;
                            }
                            revisions = series_state.todo_revisions[idx];
                            if let Err(e) =
                                reconcile_fix_series(&series_state, results_dir, &inputs)
                            {
                                status = WorkflowStatus::Failure(e);
                                break;
                            }
                            continue;
                        }
                        Ok(None) => {}
                        Err(e) => {
                            status = WorkflowStatus::Failure(format!(
                                "fix todo '{}' plan update invalid: {e}",
                                series_state.tracked[idx].todo.id
                            ));
                            break;
                        }
                    }
                }

                if !item_ok && should_revise_fix_todo(&item_trace) {
                    match revised_current_fix_todo(&item_trace, &series_state.tracked, idx) {
                        Ok(Some(revised)) if revisions < MAX_FIX_TODO_REVISIONS => {
                            revisions += 1;
                            async_println(format!(
                                "[fix series] revise {}/{} {} (revision {}/{})",
                                idx + 1,
                                fix_series_plan.as_array().map(Vec::len).unwrap_or_default(),
                                series_state.tracked[idx].todo.id,
                                revisions,
                                MAX_FIX_TODO_REVISIONS
                            ));
                            async_println(format!(
                                "[fix series] revised todo {}",
                                summarize_fix_todo(&revised)
                            ));
                            events.extend(item_trace.events);
                            series_state.tracked[idx].todo = revised;
                            series_state.tracked[idx].status = FixTodoStatus::Pending;
                            series_state.todo_revisions[idx] = revisions;
                            series_state.plan_revision =
                                series_state.plan_revision.saturating_add(1);
                            if let Err(e) =
                                reconcile_fix_series(&series_state, results_dir, &inputs)
                            {
                                status = WorkflowStatus::Failure(e);
                                break;
                            }
                            continue;
                        }
                        Ok(Some(_)) => {
                            series_state.tracked[idx].status = FixTodoStatus::Failed;
                            events.extend(item_trace.events);
                            final_state = item_trace.final_state;
                            status = WorkflowStatus::Failure(format!(
                                "fix todo '{}' exceeded revision budget ({})",
                                series_state.tracked[idx].todo.id, MAX_FIX_TODO_REVISIONS
                            ));
                            async_println(format!(
                                "[fix series] failed {}/{} {}",
                                idx + 1,
                                fix_series_plan.as_array().map(Vec::len).unwrap_or_default(),
                                series_state.tracked[idx].todo.id
                            ));
                            break;
                        }
                        Ok(None) => {}
                        Err(e) => {
                            series_state.tracked[idx].status = FixTodoStatus::Failed;
                            events.extend(item_trace.events);
                            final_state = item_trace.final_state;
                            status = WorkflowStatus::Failure(format!(
                                "fix todo '{}' revision invalid: {e}",
                                series_state.tracked[idx].todo.id
                            ));
                            async_println(format!(
                                "[fix series] failed {}/{} {}",
                                idx + 1,
                                fix_series_plan.as_array().map(Vec::len).unwrap_or_default(),
                                series_state.tracked[idx].todo.id
                            ));
                            break;
                        }
                    }
                }

                let commit_result = if item_ok {
                    Some(git_rev_parse(driver.workspace(), "HEAD").await)
                } else {
                    None
                };
                let commit_ok = commit_result.as_ref().map(Result::is_ok).unwrap_or(true);
                series_state.tracked[idx].status = if item_ok && commit_ok {
                    FixTodoStatus::Done
                } else {
                    FixTodoStatus::Failed
                };
                if item_ok {
                    if let Some(Ok(head)) = &commit_result {
                        series_state.tracked[idx].commit_sha = Some(head.clone());
                    }
                    series_state.tracked[idx].outcomes = item_trace
                        .final_state
                        .get("review")
                        .and_then(|step| step.outputs.get("outcomes"))
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    series_state.tracked[idx].is_latent = item_trace
                        .final_state
                        .get("research")
                        .and_then(|step| step.outputs.get("is_latent"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                }
                let item_label = if item_ok && commit_ok {
                    "done"
                } else {
                    "failed"
                };
                async_println(format!(
                    "[fix series] {item_label} {}/{} {}",
                    idx + 1,
                    fix_series_plan.as_array().map(Vec::len).unwrap_or_default(),
                    series_state.tracked[idx].todo.id
                ));
                events.extend(item_trace.events);
                final_state = item_trace.final_state;
                status = if let Some(Err(e)) = commit_result {
                    WorkflowStatus::Failure(e)
                } else if item_workflow_ok && !item_research_confirmed {
                    WorkflowStatus::Failure(format!(
                        "fix todo '{}' research_status was {}, expected confirmed",
                        series_state.tracked[idx].todo.id,
                        item_research_status
                            .as_deref()
                            .unwrap_or("<missing research_status>")
                    ))
                } else {
                    item_status
                };
                break;
            }
            series_state.todo_revisions[idx] = revisions;
            if let Err(e) = reconcile_fix_series(&series_state, results_dir, &inputs) {
                status = WorkflowStatus::Failure(e);
                break;
            }
            if !matches!(series_state.tracked[idx].status, FixTodoStatus::Done) {
                break;
            }
            idx += 1;
        }

        if !matches!(
            status,
            WorkflowStatus::Success | WorkflowStatus::TerminalSuccess(_)
        ) || !series_state
            .tracked
            .iter()
            .all(|item| item.status == FixTodoStatus::Done)
        {
            break;
        }

        let mut assessment_workflow = workflow.clone();
        assessment_workflow.steps.retain(|step| {
            matches!(
                step.id.as_str(),
                "series-assessment" | "final-record-results" | "final-publish"
            )
        });
        assessment_workflow.completion = None;
        let series_commits =
            match validate_series_commit_chain(driver.workspace(), &series_state).await {
                Ok(commits) => commits,
                Err(e) => {
                    status = WorkflowStatus::Failure(e);
                    break;
                }
            };
        let mut assessment_inputs = inputs.clone();
        assessment_inputs.insert("fix_run_mode".into(), Value::String("final".to_string()));
        assessment_inputs.insert(
            "fix_series_state".into(),
            serde_json::to_value(&series_state).unwrap_or(Value::Null),
        );
        assessment_inputs.insert(
            "fix_series_commits".into(),
            Value::Array(series_commits.into_iter().map(Value::String).collect()),
        );
        assessment_inputs.insert(
            "fix_series_original_artifacts".into(),
            Value::String(series_state.original_artifacts.clone()),
        );
        let assessment_trace = run_with_optional_resume(
            &assessment_workflow,
            driver,
            assessment_inputs,
            iteration_cap,
            observer.clone(),
            results_dir.map(|dir| {
                dir.join(format!(
                    "workflow-state/final-assessment-revision-{}",
                    series_state.plan_revision
                ))
            }),
            resume,
        )
        .await;
        let assessment_decision =
            step_output_string(&assessment_trace, "series-assessment", "decision");
        let assessment_ok = matches!(
            assessment_trace.status,
            WorkflowStatus::Success | WorkflowStatus::TerminalSuccess(_)
        );
        if assessment_ok && assessment_decision.as_deref() == Some("revise_pending_plan") {
            match parse_step_plan_update(&assessment_trace, "series-assessment")
                .and_then(|update| {
                    update.ok_or_else(|| {
                        "series assessment requested revision without plan_update".to_string()
                    })
                })
                .and_then(|update| apply_final_fix_plan_update(&mut series_state, update))
            {
                Ok(()) => {
                    events.extend(assessment_trace.events);
                    final_state = assessment_trace.final_state;
                    if let Err(e) = reconcile_fix_series(&series_state, results_dir, &inputs) {
                        status = WorkflowStatus::Failure(e);
                        break 'series;
                    }
                    continue 'series;
                }
                Err(e) => {
                    events.extend(assessment_trace.events);
                    final_state = assessment_trace.final_state;
                    status = WorkflowStatus::Failure(format!(
                        "series assessment plan update invalid: {e}"
                    ));
                    break 'series;
                }
            }
        }
        if series_assessment_is_terminal_failure(
            &assessment_trace.status,
            assessment_decision.as_deref(),
        ) {
            events.extend(assessment_trace.events);
            final_state = assessment_trace.final_state;
            status = WorkflowStatus::Failure(
                "series assessment determined that the produced series cannot safely advance"
                    .to_string(),
            );
            break 'series;
        }
        events.extend(assessment_trace.events);
        final_state = assessment_trace.final_state;
        status = assessment_trace.status;
        break;
    }

    Trace {
        events,
        status,
        final_state,
    }
}

fn series_assessment_is_terminal_failure(status: &WorkflowStatus, decision: Option<&str>) -> bool {
    matches!(
        status,
        WorkflowStatus::Success | WorkflowStatus::TerminalSuccess(_)
    ) && decision == Some("failure")
}

async fn run_with_optional_observer(
    workflow: &Workflow,
    driver: &mut LlmDriver,
    inputs: Map<String, Value>,
    iteration_cap: usize,
    observer: Option<EventObserver>,
    snapshot_dir: Option<PathBuf>,
) -> Trace {
    match (observer, snapshot_dir) {
        (Some(observer), Some(dir)) => {
            run_with_persistence_and_observer(
                workflow,
                driver,
                inputs,
                iteration_cap,
                dir,
                observer,
            )
            .await
        }
        (None, Some(dir)) => {
            run_with_persistence(workflow, driver, inputs, iteration_cap, dir).await
        }
        (Some(observer), None) => {
            run_with_observer(workflow, driver, inputs, iteration_cap, observer).await
        }
        (None, None) => run_with_cap(workflow, driver, inputs, iteration_cap).await,
    }
}

async fn run_with_optional_resume(
    workflow: &Workflow,
    driver: &mut LlmDriver,
    inputs: Map<String, Value>,
    iteration_cap: usize,
    observer: Option<EventObserver>,
    snapshot_dir: Option<PathBuf>,
    resume: bool,
) -> Trace {
    if resume {
        if let Some(dir) = snapshot_dir.as_ref() {
            if let Some(mut snapshot) = WorkflowSnapshot::load(dir, &workflow.id) {
                refresh_snapshot_inputs(&mut snapshot, &inputs);
                return run_resume(workflow, driver, snapshot, Some(dir.clone()), iteration_cap)
                    .await;
            }
        }
    }
    run_with_optional_observer(
        workflow,
        driver,
        inputs,
        iteration_cap,
        observer,
        snapshot_dir,
    )
    .await
}

fn refresh_snapshot_inputs(snapshot: &mut WorkflowSnapshot, inputs: &Map<String, Value>) {
    snapshot.inputs.extend(inputs.clone());
}

fn fix_series_plan_value(tracked: &[TrackedFixTodo]) -> Value {
    serde_json::to_value(
        tracked
            .iter()
            .map(|tracked| &tracked.todo)
            .collect::<Vec<_>>(),
    )
    .unwrap_or(Value::Array(Vec::new()))
}

fn should_revise_fix_todo(trace: &Trace) -> bool {
    if step_output_string(trace, "research", "research_status").as_deref() != Some("unconfirmed") {
        return false;
    }
    let Some(decision) = trace
        .final_state
        .get("research")
        .and_then(|st| st.outputs.get("research_decision"))
        .and_then(Value::as_object)
    else {
        return false;
    };
    decision.get("bug_proven").and_then(Value::as_bool) == Some(true)
        && decision.get("fix_contract_proven").and_then(Value::as_bool) == Some(false)
        && decision.get("invalidity_proven").and_then(Value::as_bool) == Some(false)
}

fn write_partial_invalidation_for_todo(
    inputs: &Map<String, Value>,
    trace: &Trace,
    todo: &FixSeriesTodo,
) -> Result<(), String> {
    let Some(dir) = inputs.get("target_artifact_dir").and_then(Value::as_str) else {
        return Ok(());
    };
    if dir.trim().is_empty() {
        return Ok(());
    }
    let invalid_evidence_kind =
        step_output_string(trace, "research", "invalid_evidence_kind").unwrap_or_default();
    let invalid_evidence =
        step_output_string(trace, "research", "invalid_evidence").unwrap_or_default();
    if invalid_evidence_kind != "source_or_commit_evidence" || invalid_evidence.trim().is_empty() {
        return Ok(());
    }
    let analysis = step_output_string(trace, "research", "analysis").unwrap_or_default();
    kres_core::write_partial_invalidation_artifact(
        Path::new(dir),
        &todo.id,
        &todo.title,
        &analysis,
        &invalid_evidence,
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}

fn revised_current_fix_todo(
    trace: &Trace,
    tracked: &[TrackedFixTodo],
    idx: usize,
) -> Result<Option<FixSeriesTodo>, String> {
    let Some(plan) = parse_fix_plan_raw(trace)? else {
        return Ok(None);
    };
    let current_id = &tracked[idx].todo.id;
    let mut matching = plan.into_iter().filter(|todo| &todo.id == current_id);
    let Some(revised) = matching.next() else {
        return Ok(None);
    };
    if matching.next().is_some() {
        return Err(format!(
            "revised fix_plan contains duplicate todo id '{}'",
            current_id
        ));
    }
    validate_fix_todo_shape(&revised, idx)?;
    for dep in &revised.depends_on {
        if !tracked[..idx].iter().any(|tracked| &tracked.todo.id == dep) {
            return Err(format!(
                "revised todo '{}' depends_on '{}' which is not an earlier todo",
                revised.id, dep
            ));
        }
    }
    if revised == tracked[idx].todo {
        return Ok(None);
    }
    Ok(Some(revised))
}

fn parse_fix_plan_raw(trace: &Trace) -> Result<Option<Vec<FixSeriesTodo>>, String> {
    let Some(raw) = trace
        .final_state
        .get("research")
        .and_then(|st| st.outputs.get("fix_plan"))
    else {
        return Ok(None);
    };
    let todos: Vec<FixSeriesTodo> = serde_json::from_value(raw.clone())
        .map_err(|e| format!("expected array of fix todo objects: {e}"))?;
    Ok(Some(todos))
}

fn parse_fix_plan_update(trace: &Trace) -> Result<Option<FixPlanUpdate>, String> {
    parse_step_plan_update(trace, "research")
}

fn parse_step_plan_update(trace: &Trace, step: &str) -> Result<Option<FixPlanUpdate>, String> {
    let Some(raw) = trace
        .final_state
        .get(step)
        .and_then(|state| state.outputs.get("plan_update"))
    else {
        return Ok(None);
    };
    serde_json::from_value(raw.clone())
        .map(Some)
        .map_err(|e| format!("expected typed plan_update: {e}"))
}

fn parse_fix_plan(trace: &Trace) -> Result<Vec<FixSeriesTodo>, String> {
    let Some(todos) = parse_fix_plan_raw(trace)? else {
        return Ok(Vec::new());
    };
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
        validate_fix_todo_shape(todo, idx)?;
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

fn validate_fix_todo_shape(todo: &FixSeriesTodo, idx: usize) -> Result<(), String> {
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
    use std::collections::{HashMap, VecDeque};
    use std::sync::Arc;

    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;

    use super::*;
    use kres_agents::workflow_exec::{StepState, StepStatus};
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
    fn final_assessment_preloads_original_finding_artifacts() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("metadata.yaml"), "id: finding-one\n").unwrap();
        std::fs::write(tmp.path().join("FINDING.md"), "# Exact original claim\n").unwrap();
        std::fs::write(tmp.path().join("summary.md"), "Original summary evidence\n").unwrap();
        let inputs = Map::from_iter([
            (
                "target_artifact_dir".to_string(),
                json!(tmp.path().display().to_string()),
            ),
            ("target_kind".to_string(), json!("finding_dir")),
        ]);

        let artifacts = load_fix_series_original_artifacts(&inputs).unwrap();

        assert!(artifacts.contains("--- ORIGINAL ARTIFACT: metadata.yaml ---"));
        assert!(artifacts.contains("id: finding-one"));
        assert!(artifacts.contains("--- ORIGINAL ARTIFACT: FINDING.md ---"));
        assert!(artifacts.contains("# Exact original claim"));
        assert!(artifacts.contains("--- ORIGINAL ARTIFACT: summary.md ---"));
        assert!(artifacts.contains("Original summary evidence"));
    }

    #[test]
    fn final_assessment_does_not_treat_prose_results_as_original_artifacts() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("summary.md"), "stale generated output\n").unwrap();
        let inputs = Map::from_iter([
            (
                "target_artifact_dir".to_string(),
                json!(tmp.path().display().to_string()),
            ),
            ("target_kind".to_string(), json!("prose")),
        ]);

        assert!(load_fix_series_original_artifacts(&inputs)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn final_assessment_requires_mandatory_finding_artifacts() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("metadata.yaml"), "id: finding-one\n").unwrap();
        let inputs = Map::from_iter([
            (
                "target_artifact_dir".to_string(),
                json!(tmp.path().display().to_string()),
            ),
            ("target_kind".to_string(), json!("finding_dir")),
        ]);

        let err = load_fix_series_original_artifacts(&inputs).unwrap_err();
        assert!(err.contains("FINDING.md"), "got: {err}");
    }

    #[cfg(unix)]
    #[test]
    fn final_assessment_rejects_artifact_symlink_escape() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), "unconsented secret\n").unwrap();
        std::fs::write(tmp.path().join("metadata.yaml"), "id: finding-one\n").unwrap();
        symlink(outside.path(), tmp.path().join("FINDING.md")).unwrap();
        let inputs = Map::from_iter([
            (
                "target_artifact_dir".to_string(),
                json!(tmp.path().display().to_string()),
            ),
            ("target_kind".to_string(), json!("finding_dir")),
        ]);

        let err = load_fix_series_original_artifacts(&inputs).unwrap_err();
        assert!(
            err.contains("resolves outside finding directory"),
            "got: {err}"
        );
        assert!(!err.contains("unconsented secret"));
    }

    #[test]
    fn resume_refreshes_machine_supplied_workflow_inputs() {
        let mut snapshot = WorkflowSnapshot {
            schema_version: WorkflowSnapshot::SCHEMA_VERSION,
            workflow_id: "fix".to_string(),
            inputs: Map::from_iter([
                ("target".to_string(), json!("same target")),
                ("fix_series_original_artifacts".to_string(), json!("stale")),
            ]),
            steps: Vec::new(),
            events_count: 0,
        };
        let fresh = Map::from_iter([
            ("target".to_string(), json!("same target")),
            (
                "fix_series_original_artifacts".to_string(),
                json!("fresh artifacts"),
            ),
        ]);

        refresh_snapshot_inputs(&mut snapshot, &fresh);

        assert_eq!(
            snapshot.inputs.get("fix_series_original_artifacts"),
            Some(&json!("fresh artifacts"))
        );
    }

    #[test]
    fn typed_series_failure_is_terminal_after_successful_validation() {
        assert!(series_assessment_is_terminal_failure(
            &WorkflowStatus::Success,
            Some("failure")
        ));
        assert!(!series_assessment_is_terminal_failure(
            &WorkflowStatus::Success,
            Some("unconfirmed")
        ));
        assert!(!series_assessment_is_terminal_failure(
            &WorkflowStatus::Failure("invalid outputs".to_string()),
            Some("failure")
        ));
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
        assert_eq!(cfg.prompt_file.lenses.len(), 5);
        assert_eq!(cfg.prompt_file.lenses[0].id, "memory-lifetime");
        assert!(cfg.prompt_file.lenses.iter().any(|l| l.id == "assertions"));
        assert!(cfg.prompt_file.prompt.contains("TARGET: HEAD"));
        assert!(cfg
            .prompt_file
            .prompt
            .contains("TARGET KIND: git commit or range"));
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
        assert_eq!(cfg.prompt_file.lenses.len(), 4);
        assert!(!cfg.prompt_file.lenses.iter().any(|l| l.id == "assertions"));
        assert!(cfg
            .prompt_file
            .prompt
            .contains("TARGET KIND: current-workspace source scope"));
        assert!(cfg
            .prompt_file
            .prompt
            .contains("There is no implied commit"));
        assert!(cfg
            .prompt_file
            .prompt
            .contains("one low-effort change survey"));
        assert_eq!(
            cfg.file_scan_target.as_deref(),
            Some("drivers/example/example.c")
        );
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

    #[test]
    fn fix_series_state_roundtrip_resets_interrupted_todo() {
        let dir =
            std::env::temp_dir().join(format!("kres-fix-series-state-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let state = FixSeriesState {
            schema_version: FixSeriesState::SCHEMA_VERSION,
            target: "finding".to_string(),
            plan_revision: 2,
            plan_update_revisions: 0,
            final_revisions: 0,
            base_head: "base".to_string(),
            original_bugs: vec![FixSeriesBug {
                id: "bug".to_string(),
                description: "bug".to_string(),
            }],
            original_artifacts: "immutable original finding\n".to_string(),
            tracked: vec![TrackedFixTodo {
                todo: series_todo("first", Vec::new()),
                status: FixTodoStatus::InProgress,
                commit_sha: None,
                outcomes: Vec::new(),
                is_latent: false,
            }],
            todo_revisions: vec![1],
        };
        state.save(&dir).unwrap();

        let loaded = FixSeriesState::load(&dir, "finding").unwrap();
        assert_eq!(loaded.plan_revision, 2);
        assert_eq!(loaded.base_head, "base");
        assert_eq!(loaded.original_artifacts, "immutable original finding\n");
        assert_eq!(loaded.todo_revisions, vec![1]);
        assert_eq!(loaded.tracked[0].status, FixTodoStatus::Pending);
        assert!(FixSeriesState::load(&dir, "other finding")
            .unwrap_err()
            .contains("target"));

        let path = FixSeriesState::path(&dir);
        let mut legacy: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        legacy.as_object_mut().unwrap().remove("original_artifacts");
        std::fs::write(&path, serde_json::to_string_pretty(&legacy).unwrap()).unwrap();
        let legacy = FixSeriesState::load(&dir, "finding").unwrap();
        assert!(legacy.original_artifacts.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fix_plan_update_is_revision_checked_and_preserves_completed_prefix() {
        let mut state = FixSeriesState {
            schema_version: FixSeriesState::SCHEMA_VERSION,
            target: "finding".to_string(),
            plan_revision: 3,
            plan_update_revisions: 0,
            final_revisions: 0,
            base_head: "base".to_string(),
            original_bugs: vec![FixSeriesBug {
                id: "bug".to_string(),
                description: "bug".to_string(),
            }],
            original_artifacts: String::new(),
            tracked: vec![
                TrackedFixTodo {
                    todo: series_todo("done", Vec::new()),
                    status: FixTodoStatus::Done,
                    commit_sha: Some("done-sha".to_string()),
                    outcomes: Vec::new(),
                    is_latent: false,
                },
                TrackedFixTodo {
                    todo: series_todo("current", vec!["done".to_string()]),
                    status: FixTodoStatus::InProgress,
                    commit_sha: None,
                    outcomes: Vec::new(),
                    is_latent: false,
                },
                TrackedFixTodo {
                    todo: series_todo("later", vec!["current".to_string()]),
                    status: FixTodoStatus::Pending,
                    commit_sha: None,
                    outcomes: Vec::new(),
                    is_latent: false,
                },
            ],
            todo_revisions: vec![0, 0, 0],
        };
        let split_a = series_todo("current-a", vec!["done".to_string()]);
        let split_b = series_todo("current-b", vec!["current-a".to_string()]);
        let mut later = series_todo("later", vec!["current-b".to_string()]);
        later.scope = "revised later scope".to_string();
        apply_fix_plan_update(
            &mut state,
            1,
            FixPlanUpdate {
                expected_revision: 3,
                operations: vec![
                    FixPlanOperation::SplitCurrent {
                        todos: vec![split_a, split_b],
                    },
                    FixPlanOperation::AppendAfterCurrent {
                        todos: vec![series_todo("appended", vec!["current-b".to_string()])],
                    },
                    FixPlanOperation::RevisePending { todo: later },
                ],
            },
        )
        .unwrap();
        assert_eq!(state.plan_revision, 4);
        assert_eq!(state.tracked[0].status, FixTodoStatus::Done);
        assert_eq!(state.tracked[1].todo.id, "current-a");
        assert_eq!(state.tracked[2].todo.id, "current-b");
        assert_eq!(state.tracked[3].todo.id, "appended");
        assert_eq!(state.tracked[4].todo.scope, "revised later scope");
        assert_eq!(state.todo_revisions.len(), state.tracked.len());

        let err = apply_fix_plan_update(
            &mut state,
            1,
            FixPlanUpdate {
                expected_revision: 3,
                operations: vec![FixPlanOperation::ReplaceCurrent {
                    todo: series_todo("current-a", vec!["done".to_string()]),
                }],
            },
        )
        .unwrap_err();
        assert!(err.contains("stale"));

        state.plan_update_revisions = MAX_PLAN_UPDATE_REVISIONS;
        let current_revision = state.plan_revision;
        let err = apply_fix_plan_update(
            &mut state,
            1,
            FixPlanUpdate {
                expected_revision: current_revision,
                operations: vec![FixPlanOperation::ReplaceCurrent {
                    todo: series_todo("current-a", vec!["done".to_string()]),
                }],
            },
        )
        .unwrap_err();
        assert!(err.contains("structural revision budget"));
    }

    #[test]
    fn reconciliation_preserves_original_bugs_when_todos_change() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("metadata.yaml"), "id: finding\n").unwrap();
        std::fs::write(dir.path().join("FINDING.md"), "# finding\n").unwrap();
        let state_dir = tempfile::tempdir().unwrap();
        let state = FixSeriesState {
            schema_version: FixSeriesState::SCHEMA_VERSION,
            target: "finding".to_string(),
            plan_revision: 2,
            plan_update_revisions: 1,
            final_revisions: 0,
            base_head: "base".to_string(),
            original_bugs: vec![FixSeriesBug {
                id: "original".to_string(),
                description: "original bug".to_string(),
            }],
            original_artifacts: String::new(),
            tracked: vec![TrackedFixTodo {
                todo: series_todo("split-commit", Vec::new()),
                status: FixTodoStatus::Pending,
                commit_sha: None,
                outcomes: Vec::new(),
                is_latent: false,
            }],
            todo_revisions: vec![0],
        };
        let mut inputs = Map::new();
        inputs.insert(
            "target_artifact_dir".into(),
            Value::String(dir.path().display().to_string()),
        );

        reconcile_fix_series(&state, Some(state_dir.path()), &inputs).unwrap();

        assert_eq!(
            kres_core::read_finding_bugs(dir.path()).unwrap(),
            vec![kres_core::FindingBug {
                id: "original".to_string(),
                description: "original bug".to_string(),
            }]
        );
    }

    #[tokio::test]
    async fn completed_prefix_rejects_unowned_head_and_accepts_snapshot_commit() {
        let repo = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(repo.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q"]);
        git(&["config", "user.name", "kres test"]);
        git(&["config", "user.email", "kres@example.com"]);
        std::fs::write(repo.path().join("a"), "base\n").unwrap();
        git(&["add", "a"]);
        git(&["commit", "-q", "-m", "base"]);
        let base = git_rev_parse(repo.path(), "HEAD").await.unwrap();
        std::fs::write(repo.path().join("a"), "first\n").unwrap();
        git(&["commit", "-q", "-am", "first"]);
        let first = git_rev_parse(repo.path(), "HEAD").await.unwrap();
        std::fs::write(repo.path().join("a"), "second\n").unwrap();
        git(&["commit", "-q", "-am", "second"]);
        let second = git_rev_parse(repo.path(), "HEAD").await.unwrap();
        let state = FixSeriesState {
            schema_version: FixSeriesState::SCHEMA_VERSION,
            target: "finding".to_string(),
            plan_revision: 1,
            plan_update_revisions: 0,
            final_revisions: 0,
            base_head: base,
            original_bugs: vec![FixSeriesBug {
                id: "bug".to_string(),
                description: "bug".to_string(),
            }],
            original_artifacts: String::new(),
            tracked: vec![
                TrackedFixTodo {
                    todo: series_todo("first", Vec::new()),
                    status: FixTodoStatus::Done,
                    commit_sha: Some(first),
                    outcomes: Vec::new(),
                    is_latent: false,
                },
                TrackedFixTodo {
                    todo: series_todo("second", vec!["first".to_string()]),
                    status: FixTodoStatus::Pending,
                    commit_sha: None,
                    outcomes: Vec::new(),
                    is_latent: false,
                },
            ],
            todo_revisions: vec![0, 0],
        };
        assert!(validate_completed_prefix(repo.path(), &state, None)
            .await
            .unwrap_err()
            .contains("does not match completed fix prefix"));

        let snapshot = WorkflowSnapshot {
            schema_version: WorkflowSnapshot::SCHEMA_VERSION,
            workflow_id: "fix".to_string(),
            inputs: Map::new(),
            steps: vec![kres_agents::workflow_exec::StepState {
                id: "commit".to_string(),
                status: StepStatus::Done,
                outputs: Map::from_iter([("commit_sha".to_string(), Value::String(second))]),
                ..Default::default()
            }],
            events_count: 0,
        };
        validate_completed_prefix(repo.path(), &state, Some(&snapshot))
            .await
            .unwrap();
    }

    #[test]
    fn todo_snapshot_key_distinguishes_sanitized_id_collisions() {
        assert_ne!(todo_snapshot_key("fix/a"), todo_snapshot_key("fix-a"));
        assert!(todo_snapshot_key("fix/a")
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_')));
    }

    #[test]
    fn final_plan_update_can_only_append_valid_todos() {
        let mut state = FixSeriesState {
            schema_version: FixSeriesState::SCHEMA_VERSION,
            target: "finding".to_string(),
            plan_revision: 1,
            plan_update_revisions: 0,
            final_revisions: 0,
            base_head: "base".to_string(),
            original_bugs: vec![FixSeriesBug {
                id: "bug".to_string(),
                description: "bug".to_string(),
            }],
            original_artifacts: String::new(),
            tracked: vec![TrackedFixTodo {
                todo: series_todo("done", Vec::new()),
                status: FixTodoStatus::Done,
                commit_sha: Some("done-sha".to_string()),
                outcomes: Vec::new(),
                is_latent: false,
            }],
            todo_revisions: vec![0],
        };
        apply_final_fix_plan_update(
            &mut state,
            FixPlanUpdate {
                expected_revision: 1,
                operations: vec![FixPlanOperation::AppendAfterCurrent {
                    todos: vec![series_todo("followup", vec!["done".to_string()])],
                }],
            },
        )
        .unwrap();
        assert_eq!(state.plan_revision, 2);
        assert_eq!(state.tracked[1].status, FixTodoStatus::Pending);

        let err = apply_final_fix_plan_update(
            &mut state,
            FixPlanUpdate {
                expected_revision: 2,
                operations: vec![FixPlanOperation::RemovePending {
                    id: "followup".to_string(),
                }],
            },
        )
        .unwrap_err();
        assert!(err.contains("append_after_current"));
    }

    fn research_trace(outputs: Map<String, Value>) -> Trace {
        let mut final_state = HashMap::new();
        final_state.insert(
            "research".to_string(),
            StepState {
                id: "research".to_string(),
                status: StepStatus::Done,
                attempt: 1,
                outputs,
                ..StepState::default()
            },
        );
        Trace {
            events: Vec::new(),
            status: WorkflowStatus::Failure("per-todo research not confirmed".to_string()),
            final_state,
        }
    }

    #[test]
    fn fix_series_revises_current_todo_from_unconfirmed_research() {
        let mut original = series_todo("fix-one", Vec::new());
        original.fix_contract = "bad contract".to_string();
        let mut revised = original.clone();
        revised.fix_contract = "corrected contract".to_string();
        revised.rationale = "corrected rationale".to_string();

        let mut outputs = Map::new();
        outputs.insert("research_status".to_string(), json!("unconfirmed"));
        outputs.insert(
            "research_decision".to_string(),
            json!({
                "bug_proven": true,
                "fix_contract_proven": false,
                "invalidity_proven": false,
                "needs_more_audit": true
            }),
        );
        outputs.insert("fix_plan".to_string(), json!([revised]));

        let trace = research_trace(outputs);
        let tracked = vec![TrackedFixTodo {
            todo: original,
            status: FixTodoStatus::InProgress,
            commit_sha: None,
            outcomes: Vec::new(),
            is_latent: false,
        }];

        assert!(should_revise_fix_todo(&trace));
        let revised = revised_current_fix_todo(&trace, &tracked, 0)
            .unwrap()
            .expect("expected revised todo");
        assert_eq!(revised.fix_contract, "corrected contract");
    }

    #[test]
    fn invalid_todo_research_writes_partial_invalidation() {
        let tmp = std::env::temp_dir().join(format!(
            "kres-partial-invalidation-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("metadata.yaml"), "id: F1\nstatus: active\n").unwrap();
        std::fs::write(tmp.join("FINDING.md"), "# Finding\n").unwrap();

        let mut inputs = Map::new();
        inputs.insert(
            "target_artifact_dir".to_string(),
            json!(tmp.display().to_string()),
        );
        let mut outputs = Map::new();
        outputs.insert("research_status".to_string(), json!("invalid"));
        outputs.insert("analysis".to_string(), json!("The sibling claim is false."));
        outputs.insert(
            "invalid_evidence".to_string(),
            json!("net/example.c:12 already rejects it."),
        );
        outputs.insert(
            "invalid_evidence_kind".to_string(),
            json!("source_or_commit_evidence"),
        );
        let trace = research_trace(outputs);
        let todo = series_todo("fix-two", Vec::new());

        write_partial_invalidation_for_todo(&inputs, &trace, &todo).unwrap();

        let body = std::fs::read_to_string(tmp.join("partial-invalidation.md")).unwrap();
        assert!(body.contains("Invalidated Todo: todo fix-two"));
        assert!(body.contains("The sibling claim is false."));
        assert!(body.contains("net/example.c:12 already rejects it."));
        assert_eq!(
            std::fs::read_to_string(tmp.join("metadata.yaml")).unwrap(),
            "id: F1\nstatus: active\n"
        );
        let _ = std::fs::remove_dir_all(&tmp);
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
                    "findings": {"type": "array<Finding>"},
                    "analysis": {"type": "string"}
                }
            }]
        });
        let workflow = kres_agents::workflow::parse_workflow(&wf_json.to_string()).unwrap();
        let responses = VecDeque::from(vec![fake_messages_response(
            "{\"findings\": [{\"id\":\"leak\",\"title\":\"leak\",\"severity\":\"high\",\"summary\":\"kernel/a.c:7 leaks a reference\"}], \
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
