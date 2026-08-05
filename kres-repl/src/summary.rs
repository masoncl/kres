//! /summary and `kres --summary` — validate every canonical finding with
//! the existing `validate` workflow, then render a plain-text (or markdown)
//! bug report from the validation-produced summaries. report.md is not
//! consulted.
//!
//! Flow:
//!   1. Export findings and run the `validate` JSON workflow for each one.
//!   2. Project each structured verdict and generated summary.md into a
//!      transient Finding, dropping stale pre-validation narrative.
//!   3. Filter out `Status::Invalidated` and sort the remaining
//!      findings by severity, most severe first; within one severity
//!      keep the store's insertion order.
//!   4. Bucket the per-task material: for every task id that appears
//!      in `finding.details[].task` ∪ `task_prose[].task`, collect
//!      the finding-by-finding analysis snippets and the file-level
//!      task_prose body. This is the set of "per-task summaries and
//!      details" the user asked for.
//!   5. Condense pass: greedy-pack tasks into batches that each fit
//!      the fast-agent input budget, then issue ONE call per batch
//!      using the embedded `condense-task.system.md` system prompt.
//!      The output is plain prose — since the final document is one
//!      aggregate report, we don't need per-task keying in the
//!      condense result. Batch outputs are concatenated into a
//!      single `task_observations` string the render pass quotes
//!      from. A single task that alone exceeds the budget falls
//!      back to `condense_single_task`, which recursively partitions
//!      every per-finding analysis and task-prose body without dropping data.
//!   6. Render pass: send the sorted findings (with `details`
//!      stripped via `redact_findings_for_agent`) plus the
//!      `task_observations` string to the `summary` (or
//!      `summary-markdown`) slash-command template. Single-shot
//!      when the prompt fits `max_input_tokens`; otherwise partition
//!      complete findings while retaining the task observations on every
//!      render call, then combine the partials.
//!
//! The `/summary` / `/summary-markdown` commands and the CLI flags
//! `--summary` / `--summary-markdown` all land here — only the
//! template choice and output filename differ between them.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use kres_core::findings::{
    redact_findings_for_agent, Finding, FindingsFile, FindingsStore, Severity, Status,
};
use kres_core::{LoggedUsage, Shutdown, TurnLogger};
use serde_json::json;
use tokio::task::JoinSet;

use kres_agents::pipeline::AgentRunner;
use kres_agents::workflow_exec::WorkflowStatus;
use kres_agents::workflow_runner::{derive_inputs, LlmDriver};
use kres_llm::{
    client::Client, config::CallConfig, model::ThinkingBudget, request::Message, Model,
};

const SUMMARY_VALIDATION_CONCURRENCY: usize = 20;

/// Default on-disk override location for the plain-text template.
/// Empty by default; an operator who wants to shadow the embedded
/// prompt drops a file at `~/.kres/commands/summary.md`. Returns
/// None when $HOME is unset.
pub fn default_template_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".kres").join("commands").join("summary.md"))
}

/// Default on-disk override location for the markdown variant.
/// `/summary-markdown` (and `--summary-markdown` on the CLI) selects
/// this instead of the plain-text one.
pub fn default_markdown_template_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".kres").join("commands").join("summary-markdown.md"))
}

/// All the inputs to one summary run. Constructed once by either the
/// REPL command handler or the `kres --summary` main-entry path.
pub struct SummaryInputs {
    /// Path to `findings.json`. Required — the summary is derived
    /// from this file alone.
    pub findings_path: PathBuf,
    pub output_path: PathBuf,
    /// Explicit override for the render-pass system prompt template.
    /// When Some, run_summary reads the file and errors if it cannot.
    /// When None, `~/.kres/commands/summary.md` wins if it exists;
    /// else the compiled-in `summary` body from
    /// `kres_agents::user_commands` is used. When `markdown` is true
    /// the `summary-markdown` variant is selected at each hop
    /// instead.
    pub template_path: Option<PathBuf>,
    /// Select the markdown variant of the template + the `.md`
    /// output filename default. Ignored when `template_path` is set
    /// (the caller has already chosen a template).
    pub markdown: bool,
    /// The top-level question that drove this research run. Loaded
    /// from in-REPL memory or `<results>/prompt.md`. When absent we
    /// still produce a report, just without the extra framing.
    pub original_prompt: Option<String>,
    pub client: Arc<Client>,
    pub model: Model,
    pub max_tokens: u32,
    pub max_input_tokens: Option<u32>,
    pub thinking: Option<ThinkingBudget>,
    /// Validation-produced findings. The renderer never reads stale narrative
    /// or task prose directly from the canonical findings store.
    pub validated_findings: Vec<Finding>,
    /// Session logger. Summary render/condense calls are ordinary inference
    /// spend and belong in `code.jsonl` alongside every other call, or the
    /// context accounting under-reports the session.
    pub logger: Option<Arc<TurnLogger>>,
}

/// Client, call config and logger for one summary inference call. Bundled
/// because every helper below needs all three and threading them separately
/// grew the signatures without making anything clearer.
#[derive(Clone, Copy)]
struct SummaryCall<'a> {
    client: &'a Client,
    cfg: &'a CallConfig,
    logger: Option<&'a TurnLogger>,
}

pub struct SummaryValidationInputs {
    pub findings_path: PathBuf,
    pub validation_dir: PathBuf,
    pub workspace: PathBuf,
    pub agent_runner: Arc<AgentRunner>,
    pub skills_dir: Option<PathBuf>,
    pub shutdown: Shutdown,
}

struct SummaryValidationJob {
    finding: Finding,
    exported: crate::export::ExportedFinding,
    workspace: PathBuf,
    validation_dir: PathBuf,
    workflow: kres_agents::workflow::Workflow,
    agent_runner: Arc<AgentRunner>,
    skills_dir: Option<PathBuf>,
    shutdown: Shutdown,
}

pub async fn validate_findings_for_summary(
    inputs: SummaryValidationInputs,
) -> Result<Vec<Finding>> {
    let store = FindingsStore::new(inputs.findings_path.clone())
        .await
        .with_context(|| format!("opening findings {}", inputs.findings_path.display()))?;
    let original = store.snapshot().await;
    let by_id: BTreeMap<String, Finding> = original
        .into_iter()
        .map(|finding| (finding.id.clone(), finding))
        .collect();

    if inputs.validation_dir.exists() {
        std::fs::remove_dir_all(&inputs.validation_dir).with_context(|| {
            format!(
                "clearing summary validation directory {}",
                inputs.validation_dir.display()
            )
        })?;
    }
    let exported = crate::export::run_export(crate::export::ExportInputs {
        findings_path: inputs.findings_path,
        output_dir: inputs.validation_dir.clone(),
        workspace: inputs.workspace.clone(),
        redact_details: true,
    })
    .await?;
    let override_dir = dirs::home_dir().map(|home| home.join(".kres/workflows"));
    let workflow = kres_agents::workflow::lookup_workflow(override_dir.as_deref(), "validate")?;
    let batch_shutdown = inputs.shutdown.child();
    let mut jobs = Vec::with_capacity(exported.len());
    for exported_finding in exported {
        let Some(finding) = by_id.get(&exported_finding.id).cloned() else {
            return Err(anyhow!(
                "exported finding {} is missing from canonical findings",
                exported_finding.id
            ));
        };
        jobs.push(SummaryValidationJob {
            finding,
            exported: exported_finding,
            workspace: inputs.workspace.clone(),
            validation_dir: inputs.validation_dir.clone(),
            workflow: workflow.clone(),
            agent_runner: inputs.agent_runner.clone(),
            skills_dir: inputs.skills_dir.clone(),
            shutdown: batch_shutdown.child(),
        });
    }

    let result = run_bounded_ordered(jobs, SUMMARY_VALIDATION_CONCURRENCY, |job| async move {
        validate_summary_finding(job).await
    })
    .await;
    if result.is_err() {
        batch_shutdown.cancel();
    }
    result
}

async fn validate_summary_finding(job: SummaryValidationJob) -> Result<Finding> {
    kres_core::consent::get_or_install().grant_from_mention(&job.exported.dir);
    let mut driver = LlmDriver::new(job.workspace.clone(), job.workflow.clone())
        .with_agent_runner(job.agent_runner)
        .with_shutdown(job.shutdown);
    if let Some(skills_dir) = job.skills_dir.as_ref() {
        let (with_skills, warnings) = driver.with_skills_dir(skills_dir)?;
        for warning in warnings {
            kres_core::async_eprintln!("summary validation skill: {warning}");
        }
        driver = with_skills;
    }
    let mut workflow_inputs = serde_json::Map::new();
    workflow_inputs.insert(
        "target".into(),
        serde_json::Value::String(job.exported.dir.display().to_string()),
    );
    workflow_inputs.insert(
        "source_workspace".into(),
        serde_json::Value::String(job.workspace.display().to_string()),
    );
    let state_dir = job.validation_dir.join("workflow-state").join(
        job.exported
            .dir
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("finding")),
    );
    kres_core::async_eprintln!("summary validation: start {}", job.exported.id);
    let run = crate::workflow::run_workflow_driver(
        &job.workflow,
        &mut driver,
        derive_inputs(&job.workflow, workflow_inputs),
        crate::workflow::WorkflowRunOptions {
            iteration_cap: 200,
            state_dir: Some(state_dir),
            ..Default::default()
        },
    )
    .await?;
    if !matches!(
        run.trace.status,
        WorkflowStatus::Success | WorkflowStatus::TerminalSuccess(_)
    ) {
        return Err(anyhow!(
            "validation failed for {}: {}",
            job.exported.id,
            crate::workflow::workflow_status_label(&run.trace.status)
        ));
    }
    let outputs = &run
        .trace
        .final_state
        .get("validate-reachability")
        .ok_or_else(|| anyhow!("validation produced no reachability step"))?
        .outputs;
    let validated_summary = std::fs::read_to_string(job.exported.dir.join("summary.md"))
        .with_context(|| format!("reading validated summary for {}", job.exported.id))?;
    let validated = project_validated_finding(job.finding, outputs, validated_summary)?;
    kres_core::async_eprintln!("summary validation: done {}", job.exported.id);
    Ok(validated)
}

async fn run_bounded_ordered<T, U, F, Fut>(
    items: Vec<T>,
    limit: usize,
    operation: F,
) -> Result<Vec<U>>
where
    T: Send + 'static,
    U: Send + 'static,
    F: Fn(T) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = Result<U>> + Send + 'static,
{
    if limit == 0 {
        return Err(anyhow!("bounded operation concurrency must be positive"));
    }

    let item_count = items.len();
    let mut pending = items.into_iter().enumerate();
    let mut running = JoinSet::new();
    let mut outputs: Vec<Option<U>> = std::iter::repeat_with(|| None).take(item_count).collect();

    for _ in 0..limit {
        let Some((index, item)) = pending.next() else {
            break;
        };
        let operation = operation.clone();
        running.spawn(async move { operation(item).await.map(|output| (index, output)) });
    }

    while let Some(joined) = running.join_next().await {
        let completed = match joined {
            Ok(result) => result,
            Err(error) => Err(anyhow!("bounded operation task failed: {error}")),
        };
        let (index, output) = match completed {
            Ok(value) => value,
            Err(error) => {
                running.abort_all();
                return Err(error);
            }
        };
        outputs[index] = Some(output);

        if let Some((next_index, item)) = pending.next() {
            let operation = operation.clone();
            running.spawn(async move { operation(item).await.map(|output| (next_index, output)) });
        }
    }

    outputs
        .into_iter()
        .map(|output| output.ok_or_else(|| anyhow!("bounded operation produced no output")))
        .collect()
}

fn apply_validation_outputs(
    finding: &mut Finding,
    outputs: &serde_json::Map<String, serde_json::Value>,
) -> Result<()> {
    finding.severity = match outputs.get("severity").and_then(|value| value.as_str()) {
        Some("high") => Severity::High,
        Some("medium") => Severity::Medium,
        Some("low") => Severity::Low,
        other => return Err(anyhow!("validation returned invalid severity {other:?}")),
    };
    finding.status = match outputs.get("verdict").and_then(|value| value.as_str()) {
        Some("Invalid") => Status::Invalidated,
        Some("Fixed") => Status::Fixed,
        Some("Unconfirmed" | "Unknown") => Status::Unconfirmed,
        Some("Plausible" | "ConfirmedLatent") => Status::Active,
        other => return Err(anyhow!("validation returned invalid verdict {other:?}")),
    };
    if let Some(subject) = outputs
        .get("triage_coding")
        .and_then(|value| value.get("summary_subject"))
        .and_then(|value| value.as_str())
        .filter(|subject| !subject.trim().is_empty())
    {
        finding.title = subject.to_string();
    }
    Ok(())
}

fn project_validated_finding(
    mut finding: Finding,
    outputs: &serde_json::Map<String, serde_json::Value>,
    validated_summary: String,
) -> Result<Finding> {
    apply_validation_outputs(&mut finding, outputs)?;
    finding.summary = validated_summary;
    finding.relevant_symbols.clear();
    finding.relevant_file_sections.clear();
    finding.reproducer_sketch.clear();
    finding.impact.clear();
    finding.mechanism_detail = None;
    finding.fix_sketch = None;
    finding.open_questions.clear();
    finding.related_finding_ids.clear();
    finding.details.clear();
    finding.introduced_by = None;
    Ok(finding)
}

fn is_summary_candidate(finding: &Finding) -> bool {
    !matches!(finding.status, Status::Invalidated | Status::Fixed)
}

/// Build the default output path for a summary given an optional
/// `--results` directory and an optional caller-supplied filename.
/// Filename defaults to `summary.txt`; callers wanting the markdown
/// variant pass `Some("summary.md")`. When results_dir is None the
/// file lands in the current working directory.
pub fn default_output_path(results_dir: Option<&Path>, filename: Option<&str>) -> PathBuf {
    let name = filename.unwrap_or("summary.txt");
    match results_dir {
        Some(d) => d.join(name),
        None => PathBuf::from(name),
    }
}

fn apply_thinking_override(cfg: CallConfig, thinking: Option<ThinkingBudget>) -> CallConfig {
    match thinking {
        Some(thinking) => cfg.with_thinking(thinking),
        None => cfg,
    }
}

/// Resolve the render-pass system prompt template to a
/// (source-label, body) pair. Each disk path is read at most once;
/// the embedded fallback skips disk entirely. Precedence:
///   1. `inputs.template_path` (explicit `--template FILE`).
///   2. `~/.kres/commands/<name>.md` when the file exists (the
///      operator override path; `<name>` is `summary` or
///      `summary-markdown` depending on `inputs.markdown`).
///   3. The compiled-in body from `kres_agents::user_commands`.
fn resolve_template(inputs: &SummaryInputs) -> Result<(String, String)> {
    if let Some(ref p) = inputs.template_path {
        let text = std::fs::read_to_string(p)
            .with_context(|| format!("reading template {}", p.display()))?;
        return Ok((
            p.display().to_string(),
            kres_agents::user_commands::kernel_problem_prompt(&text),
        ));
    }
    let (disk_default, fallback_label, fallback_name) = if inputs.markdown {
        (
            default_markdown_template_path(),
            "<compiled-in markdown fallback>",
            "summary-markdown",
        )
    } else {
        (default_template_path(), "<compiled-in fallback>", "summary")
    };
    if let Some(p) = disk_default.filter(|p| p.exists()) {
        let text = std::fs::read_to_string(&p)
            .with_context(|| format!("reading template {}", p.display()))?;
        if !text.trim().is_empty() {
            return Ok((
                p.display().to_string(),
                kres_agents::user_commands::kernel_problem_prompt(&text),
            ));
        }
    }
    let body = kres_agents::user_commands::lookup(fallback_name).ok_or_else(|| {
        anyhow!("embedded `{fallback_name}` template missing from user_commands — build bug")
    })?;
    Ok((fallback_label.to_string(), body))
}

/// Material attributed to one task id: the per-finding analysis
/// snippets that task contributed, plus any file-level
/// [`TaskProse`](kres_core::findings::TaskProse) body it emitted.
/// Assembled from the `FindingsFile` before the condense pass.
#[derive(Debug, Default, Clone)]
struct TaskMaterial {
    /// `(finding_id, finding_title, per-task analysis body)`, in
    /// findings-array order.
    per_finding: Vec<(String, String, String)>,
    /// The `task_prose[].prose` body for this task, or empty when
    /// the task never emitted file-level narrative.
    prose: String,
}

/// Render already-validated findings and write the output file.
pub async fn run_summary(inputs: SummaryInputs) -> Result<()> {
    if !inputs.findings_path.exists() {
        return Err(anyhow!(
            "findings file {} does not exist — nothing to summarise",
            inputs.findings_path.display()
        ));
    }

    let file = FindingsFile {
        findings: inputs.validated_findings.clone(),
        ..Default::default()
    };

    // 2. Filter invalidated + sort by severity (descending), preserve
    // insertion order within a severity band. `Vec::sort_by` is
    // stable in the std lib, so equal-severity findings keep the
    // relative order they appear in on disk.
    let mut active: Vec<Finding> = file
        .findings
        .iter()
        .filter(|finding| is_summary_candidate(finding))
        .cloned()
        .collect();
    active.sort_by_key(|f| std::cmp::Reverse(severity_rank(f.severity)));

    kres_core::async_eprintln!(
        "summary: {} active finding(s) (filtered {} invalidated), {} task_prose entry(s)",
        active.len(),
        file.findings.len() - active.len(),
        file.task_prose.len(),
    );

    if active.is_empty() {
        if let Some(parent) = inputs.output_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
        }
        std::fs::write(
            &inputs.output_path,
            "No findings remained after validation.\n",
        )
        .with_context(|| format!("writing summary to {}", inputs.output_path.display()))?;
        return Ok(());
    }

    // 3. Bucket per-task material. Order tasks by first appearance
    // so the condense calls and logs stay stable across runs on the
    // same input.
    let (task_order, mut tasks) = bucket_task_material(&active, &file);
    kres_core::async_eprintln!(
        "summary: {} distinct task id(s) contributing material",
        task_order.len()
    );

    // 4. Condense pass. Tasks are packed into batches that each fit
    // the fast agent's input budget — one API call per batch, not
    // one per task. A run with 37 tasks collapses to ~2-3 calls
    // instead of 37. Per-task overflow (a single task too big to
    // batch with anything else) falls through to the single-task
    // lossless recursive partition fallback in `condense_single_task`.
    let condense_system = kres_agents::embedded_prompts::lookup("condense-task.system.md")
        .ok_or_else(|| anyhow!("condense-task.system.md missing from embedded table — build bug"))?
        .to_string();
    let mut condense_cfg = CallConfig::defaults_for(inputs.model.clone())
        .with_max_tokens(inputs.max_tokens)
        .with_stream_label("summary condense")
        .with_system(condense_system);
    condense_cfg = apply_thinking_override(condense_cfg, inputs.thinking);
    if let Some(n) = inputs.max_input_tokens {
        condense_cfg = condense_cfg.with_max_input_tokens(n);
    }

    // A configured value describes provider capability; it is not a content
    // budget. When capability is unknown, try the complete request first and
    // partition only after the provider returns a typed over-limit response.
    let budget = inputs.max_input_tokens.unwrap_or(u32::MAX);

    let condense_call = SummaryCall {
        client: &inputs.client,
        cfg: &condense_cfg,
        logger: inputs.logger.as_deref(),
    };
    let task_observations: String =
        condense_tasks_batched(condense_call, &task_order, &mut tasks, budget).await?;

    // 5. Render pass. Resolve the template once; reuse it for the
    // single-shot attempt and any partial renders below.
    let (template_src, template_text) = resolve_template(&inputs)?;
    kres_core::async_eprintln!("summary: template = {}", template_src);

    let mut render_cfg = CallConfig::defaults_for(inputs.model.clone())
        .with_max_tokens(inputs.max_tokens)
        .with_stream_label("summary render")
        .with_system(template_text.clone());
    render_cfg = apply_thinking_override(render_cfg, inputs.thinking);
    if let Some(n) = inputs.max_input_tokens {
        render_cfg = render_cfg.with_max_input_tokens(n);
    }

    let original_prompt = inputs.original_prompt.as_deref().unwrap_or("");

    // Redact findings for the render (strip `details[]` — that's
    // what the condense pass consumed; the render pass sees the
    // condensed observations via `task_observations`).
    let render_findings = redact_findings_for_agent(&active);

    // One-shot attempt first. `size_call` short-circuits the exact
    // count when the chars/4 estimate is comfortably under budget.
    // The staged path partitions only complete findings. Every partial keeps
    // the task observations so the renderer never has to infer relationships
    // between findings and observations that were sent to different calls.
    let full_prompt =
        build_render_prompt(original_prompt, &render_findings, &task_observations, None)?;
    let full_messages = vec![user_message(&full_prompt)];
    let size = size_call(&inputs.client, &render_cfg, &full_messages, budget).await;
    kres_core::async_eprintln!(
        "summary: render sizing findings={} observations_chars={} tokens={:?} budget={}",
        render_findings.len(),
        task_observations.len(),
        size,
        budget,
    );

    let needs_staging = size.map(|t| t > budget as u64).unwrap_or(false);
    let text = if !needs_staging {
        kres_core::async_eprintln!(
            "summary: single-shot render to {} ({} finding(s), original_prompt={})",
            inputs.model.id,
            render_findings.len(),
            if original_prompt.is_empty() {
                "no"
            } else {
                "yes"
            },
        );
        let render_call = SummaryCall {
            client: &inputs.client,
            cfg: &render_cfg,
            logger: inputs.logger.as_deref(),
        };
        match try_call_and_extract(render_call, &full_messages, "summary render").await {
            Ok(text) => text,
            Err(SummaryCallError::OverInput { limit }) => {
                let provider_budget = smaller_partition_budget(budget, limit)?;
                kres_core::async_eprintln!(
                    "summary: provider rejected complete render; partitioning at reported {}-token capability",
                    provider_budget,
                );
                stage_render(
                    &inputs,
                    &render_cfg,
                    original_prompt,
                    &render_findings,
                    &task_observations,
                    provider_budget,
                )
                .await?
            }
            Err(SummaryCallError::Other(error)) => return Err(error),
        }
    } else {
        stage_render(
            &inputs,
            &render_cfg,
            original_prompt,
            &render_findings,
            &task_observations,
            budget,
        )
        .await?
    };

    if text.trim().is_empty() {
        return Err(anyhow!("summary produced empty body"));
    }

    if let Some(parent) = inputs.output_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
    }
    std::fs::write(&inputs.output_path, &text)
        .with_context(|| format!("writing summary to {}", inputs.output_path.display()))?;
    kres_core::async_eprintln!(
        "summary: wrote {} chars to {}",
        text.len(),
        inputs.output_path.display(),
    );
    Ok(())
}

/// Walk the findings and task_prose array to build:
///   - an ordered list of task ids, first-appearance order across
///     both lists (findings first, then any task_prose-only tasks);
///   - a map of task_id → TaskMaterial.
fn bucket_task_material(
    findings: &[Finding],
    file: &FindingsFile,
) -> (Vec<String>, BTreeMap<String, TaskMaterial>) {
    let mut order: Vec<String> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out: BTreeMap<String, TaskMaterial> = BTreeMap::new();

    for f in findings {
        for d in &f.details {
            if d.task.is_empty() || d.analysis.trim().is_empty() {
                continue;
            }
            if seen.insert(d.task.clone()) {
                order.push(d.task.clone());
            }
            out.entry(d.task.clone()).or_default().per_finding.push((
                f.id.clone(),
                f.title.clone(),
                d.analysis.clone(),
            ));
        }
    }

    for p in &file.task_prose {
        if p.task.is_empty() || p.prose.trim().is_empty() {
            continue;
        }
        if seen.insert(p.task.clone()) {
            order.push(p.task.clone());
        }
        let slot = out.entry(p.task.clone()).or_default();
        // Tolerate the rare case where a single task emitted more
        // than one task_prose entry (append with a blank line between
        // bodies so the condenser sees both).
        if !slot.prose.is_empty() {
            slot.prose.push_str("\n\n");
        }
        slot.prose.push_str(&p.prose);
    }

    (order, out)
}

/// Greedy-pack tasks into batches that each fit the input budget;
/// one API call per batch. Each batch returns a plain-text block of
/// observations — no per-task keying, no JSON envelope. The blocks
/// are concatenated into the single observations string the render
/// pass quotes from.
///
/// Batching:
///   - Walk `task_order` and accumulate items into `pending`.
///   - After each add, `size_call` the pending batch. When the
///     estimate crosses `budget`, flush the batch WITHOUT the item
///     that pushed it over, then seed a new batch with that item.
///   - If a single item alone exceeds `budget`, hand it to
///     `condense_single_task` for the per-task split/drop fallback.
///
/// `tasks` is consumed as we go (`.remove()` on each key).
async fn condense_tasks_batched(
    call: SummaryCall<'_>,
    task_order: &[String],
    tasks: &mut BTreeMap<String, TaskMaterial>,
    budget: u32,
) -> Result<String> {
    let mut blocks: Vec<String> = Vec::new();
    let mut pending: Vec<(String, TaskMaterial)> = Vec::new();
    let mut batch_n: usize = 0;

    for (idx, task_id) in task_order.iter().enumerate() {
        let material = tasks.remove(task_id).unwrap_or_default();
        kres_core::async_eprintln!(
            "summary: packing task {}/{} id={} findings={} prose_chars={}",
            idx + 1,
            task_order.len(),
            truncate(task_id, 40),
            material.per_finding.len(),
            material.prose.len(),
        );

        // Probe with the candidate added.
        pending.push((task_id.clone(), material));
        let prompt = build_batch_condense_prompt(&pending)?;
        let messages = vec![user_message(&prompt)];
        let size = size_call(call.client, call.cfg, &messages, budget).await;
        let fits = size.map(|t| t <= budget as u64).unwrap_or(true);
        if fits {
            continue;
        }

        // Oversize. Pop the offender, flush what was there before.
        let (offender_id, offender_material) = pending.pop().expect("just pushed");
        if !pending.is_empty() {
            batch_n += 1;
            let block = flush_batch(call, &pending, batch_n).await?;
            blocks.push(block);
            pending.clear();
        }

        // At this point pending is empty. Probe the offender alone
        // BEFORE reseeding the batch — if the offender on its own
        // exceeds the budget we must NOT ship it through
        // flush_batch (that would hit the API with an over-budget
        // prompt and bounce). Route to condense_single_task
        // instead, which splits/drops the material until it fits.
        let probe_one = vec![(offender_id.clone(), offender_material.clone())];
        let probe_prompt = build_batch_condense_prompt(&probe_one)?;
        let probe_msgs = vec![user_message(&probe_prompt)];
        let probe_size = size_call(call.client, call.cfg, &probe_msgs, budget).await;
        let probe_fits = probe_size.map(|t| t <= budget as u64).unwrap_or(true);
        if probe_fits {
            pending.push((offender_id, offender_material));
            continue;
        }

        kres_core::async_eprintln!(
            "summary: task {} alone exceeds budget; falling back to single-task split",
            truncate(&offender_id, 40),
        );
        let single_label = format!("summary condense single {}", truncate(&offender_id, 40));
        let block = condense_single_task(
            call,
            &offender_id,
            &offender_material,
            &single_label,
            budget,
        )
        .await?;
        blocks.push(block);
    }

    if !pending.is_empty() {
        batch_n += 1;
        let block = flush_batch(call, &pending, batch_n).await?;
        blocks.push(block);
    }

    kres_core::async_eprintln!(
        "summary: condense produced {} block(s) across {} batch call(s)",
        blocks.len(),
        batch_n,
    );
    Ok(join_blocks(&blocks))
}

/// Concatenate batch condensation blocks into a single
/// observations string, separated by blank lines. Trailing
/// whitespace on each block is normalised so blocks don't
/// compound-stack blank lines.
fn join_blocks(blocks: &[String]) -> String {
    let mut out = String::new();
    for b in blocks {
        let trimmed = b.trim_end_matches(|c: char| c == '\n' || c.is_whitespace());
        if trimmed.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(trimmed);
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// Fire one batch-condense call and return its prose output.
/// `batch_n` is the 1-based batch index used for the stream label.
async fn flush_batch(
    call: SummaryCall<'_>,
    batch: &[(String, TaskMaterial)],
    batch_n: usize,
) -> Result<String> {
    let prompt = build_batch_condense_prompt(batch)?;
    let messages = vec![user_message(&prompt)];
    let label = format!("summary condense batch {batch_n}");
    kres_core::async_eprintln!(
        "summary: condense batch {} — {} task(s)",
        batch_n,
        batch.len()
    );
    match try_call_and_extract(call, &messages, &label).await {
        Ok(text) => Ok(text),
        Err(SummaryCallError::OverInput { .. }) if batch.len() > 1 => {
            let midpoint = batch.len() / 2;
            let left = Box::pin(flush_batch(call, &batch[..midpoint], batch_n));
            let right = Box::pin(flush_batch(call, &batch[midpoint..], batch_n));
            let (left, right) = tokio::try_join!(left, right)?;
            Ok(join_blocks(&[left, right]))
        }
        Err(SummaryCallError::OverInput { limit }) => {
            let provider_budget = u32::try_from(limit).unwrap_or(u32::MAX);
            let (task_id, material) = &batch[0];
            condense_single_task(call, task_id, material, &label, provider_budget).await
        }
        Err(SummaryCallError::Other(error)) => Err(error),
    }
}

/// Single-task fallback: used only when a task on its own is too big to fit in
/// a batch. Recursively partitions every body; no task prose or finding
/// analysis is discarded.
async fn condense_single_task(
    call: SummaryCall<'_>,
    task_id: &str,
    material: &TaskMaterial,
    label: &str,
    budget: u32,
) -> Result<String> {
    let pending: Vec<(String, TaskMaterial)> = vec![(task_id.to_string(), material.clone())];
    let prompt = build_batch_condense_prompt(&pending)?;
    let messages = vec![user_message(&prompt)];
    let size = size_call(call.client, call.cfg, &messages, budget).await;
    let fits = size.map(|t| t <= budget as u64).unwrap_or(true);
    let budget = if fits {
        match try_call_and_extract(call, &messages, label).await {
            Ok(text) => return Ok(text),
            Err(SummaryCallError::OverInput { limit }) => smaller_partition_budget(budget, limit)?,
            Err(SummaryCallError::Other(error)) => return Err(error),
        }
    } else {
        budget
    };
    kres_core::async_eprintln!(
        "summary: single-task condense oversize for {} (budget={}); splitting",
        truncate(task_id, 40),
        budget,
    );

    // Split per_finding in half; first half keeps the prose.
    if material.per_finding.len() >= 2 {
        let mid = material.per_finding.len() / 2;
        let (left, right) = material.per_finding.split_at(mid);
        let first = TaskMaterial {
            per_finding: left.to_vec(),
            prose: material.prose.clone(),
        };
        let second = TaskMaterial {
            per_finding: right.to_vec(),
            prose: String::new(),
        };
        let l1 = format!("{label} 1/2");
        let l2 = format!("{label} 2/2");
        let a = Box::pin(condense_single_task(call, task_id, &first, &l1, budget)).await?;
        let b = Box::pin(condense_single_task(call, task_id, &second, &l2, budget)).await?;
        let mut joined = a;
        if !joined.ends_with('\n') {
            joined.push('\n');
        }
        joined.push('\n');
        joined.push_str(&b);
        return Ok(joined);
    }

    // Partition prose losslessly. The finding material stays with the first
    // fragment and is not duplicated into the second.
    if !material.prose.is_empty() {
        let (left_prose, right_prose) = split_utf8_mid(&material.prose);
        if !right_prose.is_empty() {
            let first = TaskMaterial {
                per_finding: material.per_finding.clone(),
                prose: left_prose.to_string(),
            };
            let second = TaskMaterial {
                per_finding: Vec::new(),
                prose: right_prose.to_string(),
            };
            let a = Box::pin(condense_single_task(
                call,
                task_id,
                &first,
                &format!("{label} prose 1/2"),
                budget,
            ))
            .await?;
            let b = Box::pin(condense_single_task(
                call,
                task_id,
                &second,
                &format!("{label} prose 2/2"),
                budget,
            ))
            .await?;
            return Ok(join_blocks(&[a, b]));
        }
    }

    // A single finding analysis may itself be larger than a provider request.
    // Partition only its analysis; repeat the finding identity so both pieces
    // remain attributable.
    if let Some((finding_id, title, analysis)) = material.per_finding.first() {
        let (left_analysis, right_analysis) = split_utf8_mid(analysis);
        if !right_analysis.is_empty() {
            let first = TaskMaterial {
                per_finding: vec![(finding_id.clone(), title.clone(), left_analysis.to_string())],
                prose: String::new(),
            };
            let second = TaskMaterial {
                per_finding: vec![(
                    finding_id.clone(),
                    title.clone(),
                    right_analysis.to_string(),
                )],
                prose: String::new(),
            };
            let a = Box::pin(condense_single_task(
                call,
                task_id,
                &first,
                &format!("{label} finding 1/2"),
                budget,
            ))
            .await?;
            let b = Box::pin(condense_single_task(
                call,
                task_id,
                &second,
                &format!("{label} finding 2/2"),
                budget,
            ))
            .await?;
            return Ok(join_blocks(&[a, b]));
        }
    }

    Err(anyhow!(
        "condense call for task {} exceeds the provider's {}-token input capability, and its remaining indivisible identity/schema framing cannot be partitioned",
        truncate(task_id, 60),
        budget,
    ))
}

fn split_utf8_mid(text: &str) -> (&str, &str) {
    if text.len() < 2 {
        return (text, "");
    }
    let mut midpoint = text.len() / 2;
    while midpoint > 0 && !text.is_char_boundary(midpoint) {
        midpoint -= 1;
    }
    if midpoint == 0 {
        midpoint = text
            .char_indices()
            .nth(1)
            .map_or(text.len(), |(index, _)| index);
    }
    text.split_at(midpoint)
}

fn smaller_partition_budget(current: u32, reported: u64) -> Result<u32> {
    if current <= 1 {
        return Err(anyhow!(
            "provider rejected an indivisible one-token summary framing"
        ));
    }
    let reported = u32::try_from(reported).unwrap_or(u32::MAX);
    let smaller = reported
        .min(current.saturating_sub(1))
        .min(current.saturating_mul(3) / 4)
        .max(1);
    Ok(smaller)
}

/// Map-reduce render path over complete findings. Task observations accompany
/// every partition because separating the two destroys their semantic links.
async fn stage_render(
    inputs: &SummaryInputs,
    cfg: &CallConfig,
    original_prompt: &str,
    findings: &[Finding],
    task_observations: &str,
    budget: u32,
) -> Result<String> {
    let call = SummaryCall {
        client: &inputs.client,
        cfg,
        logger: inputs.logger.as_deref(),
    };
    let finding_batches =
        partition_findings_to_fit(call, original_prompt, findings, task_observations, budget)
            .await?;
    kres_core::async_eprintln!(
        "summary: staging {} complete-finding partition(s), each with task observations",
        finding_batches.len(),
    );
    let mut partials = Vec::new();
    for (index, batch) in finding_batches.iter().enumerate() {
        let note = format!(
            "This is complete-finding partition {}/{}. Preserve every supplied finding and its relationships to the supplied task observations; a later pass combines all partitions.",
            index + 1,
            finding_batches.len(),
        );
        let prompt_json =
            build_render_prompt(original_prompt, batch, task_observations, Some(&note))?;
        let messages = vec![user_message(&prompt_json)];
        let label = format!(
            "summary render finding partition {}/{}",
            index + 1,
            finding_batches.len(),
        );
        match try_call_and_extract(call, &messages, &label).await {
            Ok(text) => partials.push(text),
            Err(SummaryCallError::OverInput { limit }) => {
                let smaller_budget = smaller_partition_budget(budget, limit)?;
                kres_core::async_eprintln!(
                    "summary: provider rejected render partition; repartitioning all findings at {} tokens",
                    smaller_budget
                );
                return Box::pin(stage_render(
                    inputs,
                    cfg,
                    original_prompt,
                    findings,
                    task_observations,
                    smaller_budget,
                ))
                .await;
            }
            Err(SummaryCallError::Other(error)) => return Err(error),
        }
    }
    let combine_system = combine_system_prompt(inputs.markdown);
    let mut combine_cfg = CallConfig::defaults_for(inputs.model.clone())
        .with_max_tokens(inputs.max_tokens)
        .with_stream_label("summary combine")
        .with_system(combine_system);
    combine_cfg = apply_thinking_override(combine_cfg, inputs.thinking);
    if let Some(n) = inputs.max_input_tokens {
        combine_cfg = combine_cfg.with_max_input_tokens(n);
    }
    let combine_call = SummaryCall {
        client: &inputs.client,
        cfg: &combine_cfg,
        logger: inputs.logger.as_deref(),
    };
    combine_summary_parts(combine_call, original_prompt, partials, budget, 0).await
}

fn combine_summary_parts<'a>(
    call: SummaryCall<'a>,
    original_prompt: &'a str,
    parts: Vec<String>,
    budget: u32,
    depth: usize,
) -> std::pin::Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
    Box::pin(async move {
        if parts.is_empty() {
            return Ok(String::new());
        }
        if parts.len() == 1 {
            return Ok(parts.into_iter().next().expect("one summary part"));
        }
        let combine_json = serde_json::to_string(&json!({
            "task": "combine_summaries",
            "original_prompt": original_prompt,
            "partials": &parts,
        }))?;
        let messages = vec![user_message(&combine_json)];
        let size = size_call(call.client, call.cfg, &messages, budget)
            .await
            .unwrap_or(0);
        if size <= budget as u64 {
            kres_core::async_eprintln!(
                "summary: combining {} partial(s) at tree depth {} ({} tokens)",
                parts.len(),
                depth,
                size,
            );
            match try_call_and_extract(call, &messages, "summary combine").await {
                Ok(text) => return Ok(text),
                Err(SummaryCallError::OverInput { .. }) => {
                    kres_core::async_eprintln!(
                        "summary: provider rejected estimated combine; reducing fan-in"
                    );
                }
                Err(SummaryCallError::Other(error)) => return Err(error),
            }
        }

        if parts.len() == 2 {
            kres_core::async_eprintln!(
                "summary: two complete partials exceed combine capability; preserving both verbatim"
            );
            return Ok(join_blocks(&parts));
        }
        let midpoint = parts.len() / 2;
        let (left, right) = (parts[..midpoint].to_vec(), parts[midpoint..].to_vec());
        let left = combine_summary_parts(call, original_prompt, left, budget, depth + 1);
        let right = combine_summary_parts(call, original_prompt, right, budget, depth + 1);
        let (left, right) = tokio::try_join!(left, right)?;
        combine_summary_parts(call, original_prompt, vec![left, right], budget, depth + 1).await
    })
}

/// Turn findings into complete render units and greedily pack fitting calls.
/// Serialized findings are never split at arbitrary byte offsets: a model that
/// sees half a JSON object cannot preserve that finding's meaning.
async fn partition_findings_to_fit(
    call: SummaryCall<'_>,
    original_prompt: &str,
    findings: &[Finding],
    task_observations: &str,
    budget: u32,
) -> Result<Vec<Vec<serde_json::Value>>> {
    if findings.is_empty() {
        return Ok(vec![Vec::new()]);
    }

    let mut units = Vec::new();
    for finding in findings {
        let value = serde_json::to_value(finding)?;
        if render_units_fit(
            call,
            original_prompt,
            std::slice::from_ref(&value),
            task_observations,
            budget,
        )
        .await?
        {
            units.push(value);
            continue;
        }

        units.extend(
            partition_finding_evidence(call, original_prompt, finding, task_observations, budget)
                .await?,
        );
    }

    let mut batches = Vec::new();
    let mut current = Vec::new();
    for unit in units {
        current.push(unit);
        if render_units_fit(call, original_prompt, &current, task_observations, budget).await? {
            continue;
        }
        let last = current.pop().expect("unit was just pushed");
        if current.is_empty() {
            return Err(anyhow!(
                "summary render unit exceeds the provider's {budget}-token input capability"
            ));
        }
        batches.push(std::mem::take(&mut current));
        current.push(last);
        if !render_units_fit(call, original_prompt, &current, task_observations, budget).await? {
            return Err(anyhow!(
                "summary render fragment exceeds the provider's {budget}-token input capability"
            ));
        }
    }
    if !current.is_empty() {
        batches.push(current);
    }
    Ok(batches)
}

/// Partition only the source-evidence collections of one finding. Every unit
/// repeats the complete semantic finding record, so no call receives a partial
/// title, mechanism, impact, reproducer, or fix. Oversized source bodies are
/// split as explicitly labelled exact-text partitions; their JSON structure is
/// never split.
async fn partition_finding_evidence(
    call: SummaryCall<'_>,
    original_prompt: &str,
    finding: &Finding,
    task_observations: &str,
    budget: u32,
) -> Result<Vec<serde_json::Value>> {
    let mut core = finding.clone();
    let symbols = std::mem::take(&mut core.relevant_symbols);
    let sections = std::mem::take(&mut core.relevant_file_sections);
    let core = serde_json::to_value(core)?;
    let mut evidence = Vec::with_capacity(symbols.len() + sections.len());
    evidence.extend(symbols.into_iter().map(|symbol| {
        json!({
            "kind": "relevant_symbol",
            "name": symbol.name,
            "filename": symbol.filename,
            "line": symbol.line,
            "exact_text": symbol.definition,
        })
    }));
    evidence.extend(sections.into_iter().map(|section| {
        json!({
            "kind": "relevant_file_section",
            "filename": section.filename,
            "line_start": section.line_start,
            "line_end": section.line_end,
            "exact_text": section.content,
        })
    }));

    let wrap = |items: Vec<serde_json::Value>| {
        json!({
            "finding": core,
            "source_evidence_partition": items,
            "source_evidence_partition_note": "The complete semantic finding is repeated in every sibling unit; source-evidence arrays are partitioned without omission.",
        })
    };
    let empty = wrap(Vec::new());
    if !render_units_fit(
        call,
        original_prompt,
        std::slice::from_ref(&empty),
        task_observations,
        budget,
    )
    .await?
    {
        return Err(anyhow!(
            "summary finding {} has an indivisible semantic core larger than the provider's {budget}-token input capability",
            finding.id
        ));
    }

    let mut out = Vec::new();
    let mut current = Vec::new();
    for item in evidence {
        let mut candidate = current.clone();
        candidate.push(item.clone());
        let wrapped = wrap(candidate.clone());
        if render_units_fit(
            call,
            original_prompt,
            std::slice::from_ref(&wrapped),
            task_observations,
            budget,
        )
        .await?
        {
            current = candidate;
            continue;
        }
        if !current.is_empty() {
            out.push(wrap(std::mem::take(&mut current)));
        }
        let single = wrap(vec![item.clone()]);
        if render_units_fit(
            call,
            original_prompt,
            std::slice::from_ref(&single),
            task_observations,
            budget,
        )
        .await?
        {
            current.push(item);
            continue;
        }
        out.extend(
            partition_source_evidence_item(
                call,
                original_prompt,
                task_observations,
                budget,
                &wrap,
                item,
            )
            .await?,
        );
    }
    if !current.is_empty() {
        out.push(wrap(current));
    }
    if out.is_empty() {
        out.push(empty);
    }
    Ok(out)
}

async fn partition_source_evidence_item<F>(
    call: SummaryCall<'_>,
    original_prompt: &str,
    task_observations: &str,
    budget: u32,
    wrap: &F,
    item: serde_json::Value,
) -> Result<Vec<serde_json::Value>>
where
    F: Fn(Vec<serde_json::Value>) -> serde_json::Value,
{
    let text = item
        .get("exact_text")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow!("source evidence is missing exact_text"))?;
    let mut pending = vec![text.to_string()];
    let mut chunks = Vec::new();
    while let Some(candidate_text) = pending.pop() {
        let mut candidate = item.clone();
        candidate["exact_text"] = json!(candidate_text);
        candidate["exact_text_is_partitioned"] = json!(true);
        // Reserve the largest possible partition metadata before deciding the
        // source body fits. Adding the real (smaller) index/count later cannot
        // push an accepted unit back over the provider capability.
        candidate["exact_text_partition"] = json!({"index": usize::MAX, "count": usize::MAX});
        let wrapped = wrap(vec![candidate.clone()]);
        if render_units_fit(
            call,
            original_prompt,
            std::slice::from_ref(&wrapped),
            task_observations,
            budget,
        )
        .await?
        {
            chunks.push(candidate);
            continue;
        }
        let candidate_text = candidate["exact_text"]
            .as_str()
            .expect("source evidence partition retains exact_text");
        let (left, right) = split_utf8_mid(candidate_text);
        if right.is_empty() {
            return Err(anyhow!(
                "summary source-evidence framing exceeds the provider's {budget}-token input capability"
            ));
        }
        pending.push(right.to_string());
        pending.push(left.to_string());
    }
    let count = chunks.len();
    Ok(chunks
        .into_iter()
        .enumerate()
        .map(|(index, mut chunk)| {
            chunk["exact_text_partition"] = json!({"index": index + 1, "count": count});
            wrap(vec![chunk])
        })
        .collect())
}

async fn render_units_fit(
    call: SummaryCall<'_>,
    original_prompt: &str,
    units: &[serde_json::Value],
    task_observations: &str,
    budget: u32,
) -> Result<bool> {
    let prompt = build_render_prompt(
        original_prompt,
        units,
        task_observations,
        Some("Lossless finding partition; preserve the supplied content."),
    )?;
    let messages = vec![user_message(&prompt)];
    Ok(size_call(call.client, call.cfg, &messages, budget)
        .await
        .map(|tokens| tokens <= budget as u64)
        .unwrap_or(true))
}

fn build_batch_condense_prompt(batch: &[(String, TaskMaterial)]) -> Result<String> {
    let items: Vec<_> = batch
        .iter()
        .map(|(task_id, material)| {
            let findings_touched: Vec<_> = material
                .per_finding
                .iter()
                .map(|(id, title, analysis)| {
                    json!({
                        "id": id,
                        "title": title,
                        "analysis": analysis,
                    })
                })
                .collect();
            json!({
                "task_id": task_id,
                "findings_touched": findings_touched,
                "task_prose": material.prose,
            })
        })
        .collect();
    Ok(serde_json::to_string(&json!({
        "task": "condense_tasks",
        "items": items,
    }))?)
}

fn build_render_prompt<T: serde::Serialize>(
    original_prompt: &str,
    findings: &[T],
    task_observations: &str,
    note: Option<&str>,
) -> Result<String> {
    Ok(serde_json::to_string(&json!({
        "task": "summary",
        "original_prompt": original_prompt,
        "findings": findings,
        "task_observations": task_observations,
        "note": note.unwrap_or(""),
    }))?)
}

fn combine_system_prompt(markdown: bool) -> String {
    let flavour = if markdown { "markdown" } else { "plain text" };
    format!(
        "You are merging partial summaries produced from the same research run into a \
         single {flavour} summary. Every section in the partials must appear in the \
         final output — merge duplicates (the same underlying topic or finding) rather \
         than listing them twice. Preserve the style, tone, structure, and line \
         wrapping the partials already use; do not invent new section headings or \
         framing. Preserve the existing candidate commit order; do not re-sort across \
         the merged output. End the output with a newline."
    )
}

/// Rank used by the severity sort. Higher = more severe.
fn severity_rank(s: Severity) -> u8 {
    match s {
        Severity::High => 3,
        Severity::Medium => 2,
        Severity::Low => 1,
    }
}

fn user_message(content: &str) -> Message {
    Message {
        role: "user".into(),
        content: content.to_string(),
        cache: false,
        cached_prefix: None,
    }
}

fn truncate(s: &str, n: usize) -> String {
    // char-boundary safe; task ids are ASCII today but the
    // summariser shouldn't panic if an operator ever stuffs a
    // multi-byte tag into a todo name.
    let mut chars = s.chars();
    let head: String = chars.by_ref().take(n).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// Safety factor on the chars/4 heuristic. When the cheap estimate
/// comes in at <= budget * SAFE_FRAC, we trust it and skip the
/// count_tokens_exact round-trip.
const SAFE_FRAC: f64 = 0.75;

async fn count_or_estimate(client: &Client, cfg: &CallConfig, messages: &[Message]) -> Option<u64> {
    if let Some(n) = client.count_tokens_exact(cfg, messages).await {
        return Some(n);
    }
    Some(cheap_estimate(cfg, messages))
}

fn cheap_estimate(cfg: &CallConfig, messages: &[Message]) -> u64 {
    let user_chars: usize = messages.iter().map(|m| m.content.len()).sum();
    let system_chars = cfg.system.as_ref().map(|s| s.len()).unwrap_or(0);
    ((user_chars + system_chars) as u64) / 4
}

async fn size_call(
    client: &Client,
    cfg: &CallConfig,
    messages: &[Message],
    budget: u32,
) -> Option<u64> {
    let est = cheap_estimate(cfg, messages);
    let safe_ceiling = (budget as f64 * SAFE_FRAC) as u64;
    if est <= safe_ceiling {
        return Some(est);
    }
    count_or_estimate(client, cfg, messages).await
}

enum SummaryCallError {
    OverInput { limit: u64 },
    Other(anyhow::Error),
}

async fn try_call_and_extract(
    call: SummaryCall<'_>,
    messages: &[Message],
    stage: &str,
) -> std::result::Result<String, SummaryCallError> {
    // Every summary inference funnels through here, so this is the one place
    // that has to log for the whole pipeline to be visible in `code.jsonl`.
    let label = format!("phase=summary stage={stage}");
    if let Some(logger) = call.logger {
        let request = call.cfg.request_meta();
        let rendered = messages
            .iter()
            .map(|message| {
                format!(
                    "{}{}",
                    message.cached_prefix.as_deref().unwrap_or(""),
                    message.content
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        logger.log_code_labeled_with_request(
            "user",
            Some(&label),
            &rendered,
            None,
            None,
            Some(&request),
        );
    }
    let resp = call
        .client
        .messages_streaming(call.cfg, messages)
        .await
        .map_err(|error| {
            if let kres_llm::LlmError::OverInputLimit { limit, .. } = error {
                SummaryCallError::OverInput { limit }
            } else {
                SummaryCallError::Other(anyhow!("{stage}: call failed: {error}"))
            }
        })?;
    let text = extract_text(&resp);
    if let Some(logger) = call.logger {
        logger.log_code_labeled_with_model(
            "assistant",
            Some(&label),
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
    if text.trim().is_empty() {
        return Err(SummaryCallError::Other(anyhow!(
            "{stage}: empty body (stop_reason={:?})",
            resp.stop_reason
        )));
    }
    kres_core::async_eprintln!(
        "{stage}: {} chars (usage in={} out={})",
        text.len(),
        resp.usage.input_tokens,
        resp.usage.output_tokens,
    );
    Ok(text)
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
    /// Every summary inference must funnel through `try_call_and_extract`,
    /// because that is the only place the pipeline logs to `code.jsonl`.
    ///
    /// This file previously made its provider calls with no logging at all, so
    /// `/summary` spend was invisible to the context accounting the rest of
    /// kres is measured by. A new call site that talks to a client directly
    /// would silently reintroduce that hole, and no behavioural test would
    /// notice — the summary would still be correct, just unaccounted. Hence a
    /// structural guard.
    #[test]
    fn all_summary_inference_goes_through_the_one_logged_call_site() {
        let source = include_str!("summary.rs");
        // Split so this guard's own needle is not present in the source text
        // it scans.
        let needle = concat!(".messages", "_streaming(");
        let calls = source.matches(needle).count();
        assert_eq!(
            calls, 1,
            "expected exactly one provider call site (inside try_call_and_extract); \
found {calls}. If you added a summary inference path, route it through \
try_call_and_extract so it is logged."
        );
        assert!(
            source.contains(concat!("fn try_call", "_and_extract")),
            "the logged call site was renamed; update this guard"
        );
    }

    use super::*;
    use kres_core::findings::{FindingDetail, RelevantFileSection, RelevantSymbol, TaskProse};
    use kres_llm::model::Effort;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn f(id: &str, sev: Severity, status: Status, details: Vec<(&str, &str)>) -> Finding {
        Finding {
            id: id.to_string(),
            title: format!("title {id}"),
            severity: sev,
            status,
            relevant_symbols: Vec::<RelevantSymbol>::new(),
            relevant_file_sections: Vec::<RelevantFileSection>::new(),
            summary: "s".into(),
            reproducer_sketch: "r".into(),
            impact: "i".into(),
            mechanism_detail: None,
            fix_sketch: None,
            open_questions: Vec::new(),
            first_seen_task: details.first().map(|(t, _)| t.to_string()),
            last_updated_task: details.last().map(|(t, _)| t.to_string()),
            related_finding_ids: Vec::new(),
            details: details
                .into_iter()
                .map(|(t, a)| FindingDetail {
                    task: t.to_string(),
                    analysis: a.to_string(),
                })
                .collect(),
            reactivate: false,
            introduced_by: None,
            first_seen_at: None,
        }
    }

    #[test]
    fn validation_projection_replaces_stale_narrative_and_status() {
        let finding = f(
            "stale",
            Severity::High,
            Status::Active,
            vec![("task", "stale task analysis")],
        );
        let mut finding = finding;
        finding.relevant_symbols.push(RelevantSymbol {
            name: "stale_symbol".into(),
            filename: "stale.c".into(),
            line: 10,
            definition: "stale definition".into(),
        });
        finding.relevant_file_sections.push(RelevantFileSection {
            filename: "stale.c".into(),
            line_start: 1,
            line_end: 20,
            content: "stale source".into(),
        });
        finding.related_finding_ids.push("stale-related".into());
        finding.introduced_by = Some(kres_core::findings::IntroducedBy {
            sha: "deadbeefdead".into(),
            subject: "stale attribution".into(),
        });
        let outputs = serde_json::json!({
            "verdict": "Invalid",
            "severity": "low",
            "triage_coding": {"summary_subject": "validated subject"}
        })
        .as_object()
        .unwrap()
        .clone();

        let projected =
            project_validated_finding(finding, &outputs, "validated body".into()).unwrap();

        assert_eq!(projected.status, Status::Invalidated);
        assert_eq!(projected.severity, Severity::Low);
        assert_eq!(projected.title, "validated subject");
        assert_eq!(projected.summary, "validated body");
        assert!(projected.details.is_empty());
        assert!(projected.relevant_symbols.is_empty());
        assert!(projected.relevant_file_sections.is_empty());
        assert!(projected.related_finding_ids.is_empty());
        assert!(projected.introduced_by.is_none());
        assert!(projected.reproducer_sketch.is_empty());
        assert!(projected.impact.is_empty());
    }

    #[test]
    fn validation_projection_rejects_unknown_control_values() {
        let finding = f("bad", Severity::Low, Status::Active, vec![]);
        let outputs = serde_json::json!({"verdict": "Maybe", "severity": "medium"})
            .as_object()
            .unwrap()
            .clone();

        assert!(project_validated_finding(finding, &outputs, "body".into()).is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn bounded_runner_parallelizes_up_to_limit_and_preserves_order() {
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let items: Vec<usize> = (0..45).collect();
        let outputs = run_bounded_ordered(items.clone(), 20, {
            let active = active.clone();
            let peak = peak.clone();
            move |item| {
                let active = active.clone();
                let peak = peak.clone();
                async move {
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(5 + (item % 5) as u64))
                        .await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    Ok(item * 2)
                }
            }
        })
        .await
        .unwrap();

        assert_eq!(
            outputs,
            items.iter().map(|item| item * 2).collect::<Vec<_>>()
        );
        assert!(peak.load(Ordering::SeqCst) > 1);
        assert!(peak.load(Ordering::SeqCst) <= 20);
    }

    #[test]
    fn severity_sort_desc_with_stable_within_band() {
        let findings = [
            f("a", Severity::Low, Status::Active, vec![]),
            f("b", Severity::High, Status::Active, vec![]),
            f("c", Severity::Medium, Status::Active, vec![]),
            f("d", Severity::High, Status::Active, vec![]),
            f("e", Severity::High, Status::Active, vec![]),
        ];
        let mut got: Vec<Finding> = findings.to_vec();
        got.sort_by_key(|f| std::cmp::Reverse(severity_rank(f.severity)));
        let ids: Vec<&str> = got.iter().map(|x| x.id.as_str()).collect();
        // High (b, d, e) first in input order, then Medium (c), Low (a).
        assert_eq!(ids, vec!["b", "d", "e", "c", "a"]);
    }

    #[test]
    fn invalid_and_fixed_findings_are_filtered_out() {
        let findings = [
            f("live", Severity::Medium, Status::Active, vec![]),
            f("dead", Severity::High, Status::Invalidated, vec![]),
            f("fixed", Severity::High, Status::Fixed, vec![]),
        ];
        let kept: Vec<&Finding> = findings
            .iter()
            .filter(|finding| is_summary_candidate(finding))
            .collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].id, "live");
    }

    #[test]
    fn summary_call_preserves_disabled_thinking_override() {
        let cfg = apply_thinking_override(
            CallConfig::defaults_for(Model::sonnet_4_6()).with_max_tokens(8_000),
            Some(ThinkingBudget::Disabled),
        );
        assert!(cfg.request_meta().thinking.is_none());
    }

    #[test]
    fn summary_call_preserves_adaptive_effort_override() {
        let cfg = apply_thinking_override(
            CallConfig::defaults_for(Model::opus_4_7()).with_max_tokens(8_000),
            Some(ThinkingBudget::Adaptive(Effort::High)),
        );
        let meta = cfg.request_meta();
        assert_eq!(meta.thinking.as_deref(), Some("adaptive"));
        assert_eq!(meta.effort.as_deref(), Some("high"));
        assert!(meta.budget_tokens.is_none());
    }

    #[test]
    fn bucket_task_material_covers_findings_and_prose() {
        let findings = vec![
            f(
                "one",
                Severity::High,
                Status::Active,
                vec![("task-a", "analysis-a1"), ("task-b", "analysis-b")],
            ),
            f(
                "two",
                Severity::Low,
                Status::Active,
                vec![("task-a", "analysis-a2")],
            ),
        ];
        let file = FindingsFile {
            findings: findings.clone(),
            updated_at: None,
            tasks_since_change: 0,
            turn_n: None,
            task_prose: vec![
                TaskProse {
                    task: "task-b".into(),
                    created_at: chrono::Utc::now(),
                    prose: "prose-b-1".into(),
                },
                TaskProse {
                    task: "task-c".into(),
                    created_at: chrono::Utc::now(),
                    prose: "prose-c".into(),
                },
                TaskProse {
                    task: "task-b".into(),
                    created_at: chrono::Utc::now(),
                    prose: "prose-b-2".into(),
                },
            ],
        };
        let (order, map) = bucket_task_material(&findings, &file);
        // Order: task-a first (findings[0].details[0]), then task-b
        // (findings[0].details[1]), then task-c (task_prose-only).
        assert_eq!(order, vec!["task-a", "task-b", "task-c"]);
        let a = map.get("task-a").unwrap();
        assert_eq!(a.per_finding.len(), 2);
        assert_eq!(a.prose, "");
        let b = map.get("task-b").unwrap();
        assert_eq!(b.per_finding.len(), 1);
        // Both prose entries for task-b were concatenated with a
        // blank-line separator.
        assert!(b.prose.contains("prose-b-1"));
        assert!(b.prose.contains("prose-b-2"));
        assert!(b.prose.contains("\n\n"));
        let c = map.get("task-c").unwrap();
        assert_eq!(c.per_finding.len(), 0);
        assert_eq!(c.prose, "prose-c");
    }

    #[test]
    fn task_prose_only_tasks_retain_observations() {
        // Regression: a past narrowing implementation used
        // finding.first_seen_task / last_updated_task to decide
        // which observations survived. Tasks that only emitted
        // TaskProse (never touched a finding) have empty stamps on
        // every finding, so they would silently drop out of both
        // single-shot and partial renders. Current design: bucket
        // collects task_prose-only tasks alongside detail-bearing
        // ones; this test pins that by checking the bucket's
        // output.
        let findings = vec![f(
            "one",
            Severity::High,
            Status::Active,
            vec![("task-a", "a")],
        )];
        let file = FindingsFile {
            findings: findings.clone(),
            updated_at: None,
            tasks_since_change: 0,
            turn_n: None,
            task_prose: vec![TaskProse {
                task: "task-prose-only".into(),
                created_at: chrono::Utc::now(),
                prose: "general narrative".into(),
            }],
        };
        let (order, map) = bucket_task_material(&findings, &file);
        assert!(order.contains(&"task-prose-only".to_string()));
        assert_eq!(
            map.get("task-prose-only").unwrap().prose,
            "general narrative"
        );
    }

    #[test]
    fn join_blocks_drops_empty_and_doesnt_double_blank_lines() {
        let blocks = vec![
            "alpha one\nalpha two\n".to_string(),
            "".to_string(),
            "   \n\n".to_string(),
            "beta".to_string(),
        ];
        let out = join_blocks(&blocks);
        assert_eq!(out, "alpha one\nalpha two\n\nbeta\n");
    }

    #[test]
    fn join_blocks_empty_input_returns_empty() {
        let blocks: Vec<String> = vec![];
        assert!(join_blocks(&blocks).is_empty());
    }

    #[test]
    fn provider_rejection_always_reduces_partition_budget() {
        assert_eq!(smaller_partition_budget(1_000, 900).unwrap(), 750);
        assert_eq!(smaller_partition_budget(1_000, 100).unwrap(), 100);
        assert!(smaller_partition_budget(1, 1).is_err());
    }

    #[test]
    fn utf8_partition_reconstructs_every_byte() {
        let input = format!("{}🦀{}", "a".repeat(127), "β".repeat(129));
        let (left, right) = split_utf8_mid(&input);
        assert!(!left.is_empty());
        assert!(!right.is_empty());
        assert_eq!(format!("{left}{right}"), input);
    }

    #[test]
    fn build_batch_condense_prompt_carries_every_task() {
        let m1 = TaskMaterial {
            per_finding: vec![("f1".into(), "t1".into(), "a1".into())],
            prose: "p1".into(),
        };
        let m2 = TaskMaterial {
            per_finding: vec![],
            prose: "p2".into(),
        };
        let batch = vec![("task-a".to_string(), m1), ("task-b".to_string(), m2)];
        let prompt = build_batch_condense_prompt(&batch).unwrap();
        let v: serde_json::Value = serde_json::from_str(&prompt).unwrap();
        assert_eq!(v["task"], "condense_tasks");
        let items = v["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["task_id"], "task-a");
        assert_eq!(items[0]["task_prose"], "p1");
        assert_eq!(items[1]["task_id"], "task-b");
        assert_eq!(items[1]["findings_touched"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn embedded_condense_prompt_is_registered() {
        // Build-time guarantee that the condense pass has a system
        // prompt to load; otherwise run_summary panics at runtime.
        let body = kres_agents::embedded_prompts::lookup("condense-task.system.md")
            .expect("condense-task.system.md must be embedded");
        assert!(!body.trim().is_empty());
    }
}
