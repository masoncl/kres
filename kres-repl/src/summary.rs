//! /summary and `kres --summary` — render a plain-text (or markdown)
//! bug report from `findings.json` ALONE. report.md is no longer
//! consulted by this module; every fact the summary emits comes from
//! the findings store.
//!
//! Flow:
//!   1. Open the findings file via `kres_core::FindingsStore` (jsondb)
//!      and take a `FindingsFile` snapshot.
//!   2. Filter out `Status::Invalidated` and sort the remaining
//!      findings by severity, most severe first; within one severity
//!      keep the store's insertion order.
//!   3. Bucket the per-task material: for every task id that appears
//!      in `finding.details[].task` ∪ `task_prose[].task`, collect
//!      the finding-by-finding analysis snippets and the file-level
//!      task_prose body. This is the set of "per-task summaries and
//!      details" the user asked for.
//!   4. Condense pass: greedy-pack tasks into batches that each fit
//!      the fast-agent input budget, then issue ONE call per batch
//!      using the embedded `condense-task.system.md` system prompt.
//!      The output is plain prose — since the final document is one
//!      aggregate report, we don't need per-task keying in the
//!      condense result. Batch outputs are concatenated into a
//!      single `task_observations` string the render pass quotes
//!      from. A single task that alone exceeds the budget falls
//!      back to `condense_single_task`, which recursively splits
//!      per_finding halves and drops task_prose with a breadcrumb.
//!   5. Render pass: send the sorted findings (with `details`
//!      stripped via `redact_findings_for_agent`) plus the
//!      `task_observations` string to the `summary` (or
//!      `summary-markdown`) slash-command template. Single-shot
//!      when the prompt fits `max_input_tokens`; otherwise split
//!      findings into batches that each fit (every batch carries
//!      the full observations string), render one partial per
//!      batch, then combine the partials.
//!
//! The `/summary` / `/summary-markdown` commands and the CLI flags
//! `--summary` / `--summary-markdown` all land here — only the
//! template choice and output filename differ between them.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use kres_core::findings::{
    redact_findings_for_agent, Finding, FindingsFile, FindingsStore, Severity, Status,
};
use serde_json::json;

use kres_agents::AgentConfig;
use kres_llm::{client::Client, config::CallConfig, request::Message, Model};

/// Conservative fallback when the caller didn't set max_input_tokens
/// and we need a budget to decide staging. Claude's default 200K
/// context, minus headroom for the output and protocol overhead.
const DEFAULT_INPUT_BUDGET: u32 = 180_000;

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

/// Build a minimal fast-agent LLM client from a fast-code-agent config
/// file. `kres --summary` uses this so it can issue the summary calls
/// without spinning up the full orchestrator. The summariser runs on
/// the fast agent — per-task condensation and per-batch rendering are
/// both formatting work that the slow agent would be overkill for.
pub fn load_fast_for_summary(
    fast_cfg_path: &Path,
    settings: &crate::settings::Settings,
) -> Result<(Arc<Client>, Model, u32, Option<u32>)> {
    let fast_cfg = AgentConfig::load(fast_cfg_path)
        .with_context(|| format!("loading fast agent config {}", fast_cfg_path.display()))?;
    let fast_model = crate::settings::pick_model(
        fast_cfg.model.as_deref(),
        crate::settings::ModelRole::Fast,
        settings,
    );
    let client = Arc::new(Client::new(fast_cfg.key.clone())?);
    let max_tokens = fast_cfg.max_tokens.unwrap_or(fast_model.max_output_tokens);
    Ok((client, fast_model, max_tokens, fast_cfg.max_input_tokens))
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
        return Ok((p.display().to_string(), text));
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
        return Ok((p.display().to_string(), text));
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

/// Render the summary. Reads findings.json, runs the condense + render
/// passes, writes the output file.
pub async fn run_summary(inputs: SummaryInputs) -> Result<()> {
    if !inputs.findings_path.exists() {
        return Err(anyhow!(
            "findings file {} does not exist — nothing to summarise",
            inputs.findings_path.display()
        ));
    }

    // 1. Load via jsondb. `FindingsStore::new` opens the same
    // canonical on-disk schema the pipeline writes; `file_snapshot`
    // returns a Clone<FindingsFile> that detaches us from the live
    // lock so the summariser doesn't hold a read guard across the
    // LLM round-trips.
    let store = FindingsStore::new(inputs.findings_path.clone())
        .await
        .with_context(|| format!("opening findings {}", inputs.findings_path.display()))?;
    let file: FindingsFile = store.file_snapshot().await;

    // 2. Filter invalidated + sort by severity (descending), preserve
    // insertion order within a severity band. `Vec::sort_by` is
    // stable in the std lib, so equal-severity findings keep the
    // relative order they appear in on disk.
    let mut active: Vec<Finding> = file
        .findings
        .iter()
        .filter(|f| f.status != Status::Invalidated)
        .cloned()
        .collect();
    active.sort_by(|a, b| severity_rank(b.severity).cmp(&severity_rank(a.severity)));

    kres_core::async_eprintln!(
        "summary: {} active finding(s) (filtered {} invalidated), {} task_prose entry(s)",
        active.len(),
        file.findings.len() - active.len(),
        file.task_prose.len(),
    );

    if active.is_empty() && file.task_prose.is_empty() {
        return Err(anyhow!(
            "no active findings and no task_prose in {} — nothing to summarise",
            inputs.findings_path.display()
        ));
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
    // split+drop fallback in `condense_single_task`.
    let condense_system = kres_agents::embedded_prompts::lookup("condense-task.system.md")
        .ok_or_else(|| anyhow!("condense-task.system.md missing from embedded table — build bug"))?
        .to_string();
    let mut condense_cfg = CallConfig::defaults_for(inputs.model.clone())
        .with_max_tokens(inputs.max_tokens)
        .with_stream_label("summary condense")
        .with_system(condense_system);
    if let Some(n) = inputs.max_input_tokens {
        condense_cfg = condense_cfg.with_max_input_tokens(n);
    }

    let budget = inputs.max_input_tokens.unwrap_or(DEFAULT_INPUT_BUDGET);

    let task_observations: String = condense_tasks_batched(
        &inputs.client,
        &condense_cfg,
        &task_order,
        &mut tasks,
        budget,
    )
    .await?;

    // 5. Render pass. Resolve the template once; reuse it for the
    // single-shot attempt and any partial renders below.
    let (template_src, template_text) = resolve_template(&inputs)?;
    kres_core::async_eprintln!("summary: template = {}", template_src);

    let mut render_cfg = CallConfig::defaults_for(inputs.model.clone())
        .with_max_tokens(inputs.max_tokens)
        .with_stream_label("summary render")
        .with_system(template_text.clone());
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
    // The observations block is typically small relative to
    // findings bodies — we always send it whole alongside every
    // render call (single-shot and each partial).
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
        call_and_extract(
            &inputs.client,
            &render_cfg,
            &full_messages,
            "summary render",
        )
        .await?
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
    client: &Client,
    cfg: &CallConfig,
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
        let size = size_call(client, cfg, &messages, budget).await;
        let fits = size.map(|t| t <= budget as u64).unwrap_or(true);
        if fits {
            continue;
        }

        // Oversize. Pop the offender, flush what was there before.
        let (offender_id, offender_material) = pending.pop().expect("just pushed");
        if !pending.is_empty() {
            batch_n += 1;
            let block = flush_batch(client, cfg, &pending, batch_n).await?;
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
        let probe_size = size_call(client, cfg, &probe_msgs, budget).await;
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
            client,
            cfg,
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
        let block = flush_batch(client, cfg, &pending, batch_n).await?;
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
    client: &Client,
    cfg: &CallConfig,
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
    call_and_extract(client, cfg, &messages, &label).await
}

/// Single-task fallback: used only when a task on its own is too
/// big to fit in a batch. Recursively splits `per_finding` and
/// drops `task_prose` with a breadcrumb, reusing the batch prompt
/// shape with a one-item list.
async fn condense_single_task(
    client: &Client,
    cfg: &CallConfig,
    task_id: &str,
    material: &TaskMaterial,
    label: &str,
    budget: u32,
) -> Result<String> {
    let pending: Vec<(String, TaskMaterial)> = vec![(task_id.to_string(), material.clone())];
    let prompt = build_batch_condense_prompt(&pending)?;
    let messages = vec![user_message(&prompt)];
    let size = size_call(client, cfg, &messages, budget).await;
    let fits = size.map(|t| t <= budget as u64).unwrap_or(true);
    if fits {
        return call_and_extract(client, cfg, &messages, label).await;
    }
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
        let a = Box::pin(condense_single_task(
            client, cfg, task_id, &first, &l1, budget,
        ))
        .await?;
        let b = Box::pin(condense_single_task(
            client, cfg, task_id, &second, &l2, budget,
        ))
        .await?;
        let mut joined = a;
        if !joined.ends_with('\n') {
            joined.push('\n');
        }
        joined.push('\n');
        joined.push_str(&b);
        return Ok(joined);
    }

    // One (or zero) per_finding entry left. Prose is the overflow.
    if !material.prose.is_empty() {
        let stripped = TaskMaterial {
            per_finding: material.per_finding.clone(),
            prose: String::new(),
        };
        let stripped_pending: Vec<(String, TaskMaterial)> = vec![(task_id.to_string(), stripped)];
        let stripped_prompt = build_batch_condense_prompt(&stripped_pending)?;
        let stripped_messages = vec![user_message(&stripped_prompt)];
        let stripped_size = size_call(client, cfg, &stripped_messages, budget).await;
        if stripped_size.map(|t| t <= budget as u64).unwrap_or(true) {
            kres_core::async_eprintln!(
                "summary: condense dropping task_prose ({} chars) for {} to fit budget",
                material.prose.len(),
                truncate(task_id, 40),
            );
            let body = call_and_extract(client, cfg, &stripped_messages, label).await?;
            let mut out = body;
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(
                "\n[DROPPED task_prose] The file-level narrative for this task \
                 exceeded the condense input budget and was elided.\n",
            );
            return Ok(out);
        }
    }

    Err(anyhow!(
        "condense call for task {} exceeds the {} input-token budget even with \
         a single finding and no task_prose — raise max_input_tokens or \
         shrink the finding.details[] analysis body",
        truncate(task_id, 60),
        budget,
    ))
}

/// Map-reduce render path: split findings into batches that fit the
/// input budget, call the render template on each with the full
/// `task_observations` string, then combine the partials. The
/// observations string is small relative to findings bodies and
/// already condensed, so it always rides along in full — batching
/// happens only on the findings axis.
async fn stage_render(
    inputs: &SummaryInputs,
    cfg: &CallConfig,
    original_prompt: &str,
    findings: &[Finding],
    task_observations: &str,
    budget: u32,
) -> Result<String> {
    if findings.is_empty() {
        return Err(anyhow!(
            "render prompt exceeds {} input tokens but there are no findings to chunk \
             (observations alone overflow budget). Trim task_prose entries or raise \
             max_input_tokens.",
            budget
        ));
    }
    let batches = chunk_findings_to_fit(
        &inputs.client,
        cfg,
        original_prompt,
        findings,
        task_observations,
        budget,
    )
    .await?;
    kres_core::async_eprintln!(
        "summary: staging: {} batch(es) over {} finding(s); rendering partials then combining",
        batches.len(),
        findings.len(),
    );

    let mut partials = Vec::with_capacity(batches.len());
    for (idx, batch) in batches.iter().enumerate() {
        let note = partial_note(idx + 1, batches.len());
        let prompt_json = build_render_prompt(
            original_prompt,
            batch,
            task_observations,
            Some(note.as_str()),
        )?;
        let messages = vec![user_message(&prompt_json)];
        let label = format!("summary render partial {}/{}", idx + 1, batches.len());
        kres_core::async_eprintln!(
            "summary: partial {}/{} — {} finding(s), observations_chars={}",
            idx + 1,
            batches.len(),
            batch.len(),
            task_observations.len(),
        );
        let text = call_and_extract(&inputs.client, cfg, &messages, &label).await?;
        partials.push(text);
    }

    let combine_system = combine_system_prompt(inputs.markdown);
    let mut combine_cfg = CallConfig::defaults_for(inputs.model.clone())
        .with_max_tokens(inputs.max_tokens)
        .with_stream_label("summary combine")
        .with_system(combine_system);
    if let Some(n) = inputs.max_input_tokens {
        combine_cfg = combine_cfg.with_max_input_tokens(n);
    }
    let combine_json = serde_json::to_string(&json!({
        "task": "combine_summaries",
        "original_prompt": original_prompt,
        "partials": partials,
    }))?;
    let combine_messages = vec![user_message(&combine_json)];
    let combine_size = size_call(&inputs.client, &combine_cfg, &combine_messages, budget).await;
    kres_core::async_eprintln!(
        "summary: combine sizing partials={} tokens={:?} budget={}",
        partials.len(),
        combine_size,
        budget,
    );
    if let Some(n) = combine_size {
        if n > budget as u64 {
            return Err(anyhow!(
                "combined partials ({n} tokens) exceed the {budget}-token input budget — \
                 raise max_input_tokens or shrink task_prose entries"
            ));
        }
    }
    kres_core::async_eprintln!(
        "summary: combining {} partial(s) into final output",
        partials.len()
    );
    call_and_extract(
        &inputs.client,
        &combine_cfg,
        &combine_messages,
        "summary combine",
    )
    .await
}

/// Split findings into consecutive chunks such that each (chunk +
/// full task_observations + template) fits `budget` input tokens.
/// Starts at 2 parts and doubles until every partition fits or
/// each chunk is a single finding.
async fn chunk_findings_to_fit<'a>(
    client: &Client,
    cfg: &CallConfig,
    original_prompt: &str,
    findings: &'a [Finding],
    task_observations: &str,
    budget: u32,
) -> Result<Vec<&'a [Finding]>> {
    if findings.len() < 2 {
        return Err(anyhow!(
            "cannot chunk {} finding(s) to fit the {} input-token budget; \
             observations or a single finding is the overflow source",
            findings.len(),
            budget
        ));
    }
    let mut parts: usize = 2;
    loop {
        let chunks = split_evenly(findings, parts);
        let mut all_fit = true;
        for (idx, chunk) in chunks.iter().enumerate() {
            let probe_note = partial_note(idx + 1, chunks.len());
            let prompt = build_render_prompt(
                original_prompt,
                chunk,
                task_observations,
                Some(probe_note.as_str()),
            )?;
            let messages = vec![user_message(&prompt)];
            let size = size_call(client, cfg, &messages, budget).await;
            let over = size.map(|t| t > budget as u64).unwrap_or(false);
            if over {
                all_fit = false;
                break;
            }
        }
        if all_fit {
            return Ok(chunks);
        }
        if parts >= findings.len() {
            return Err(anyhow!(
                "even one finding per chunk exceeds the {} input-token budget; \
                 observations are likely the overflow source",
                budget
            ));
        }
        parts = (parts * 2).min(findings.len());
    }
}

fn split_evenly<T>(items: &[T], parts: usize) -> Vec<&[T]> {
    if parts == 0 || items.is_empty() {
        return vec![items];
    }
    let base = items.len() / parts;
    let rem = items.len() % parts;
    let mut out = Vec::with_capacity(parts);
    let mut start = 0;
    for i in 0..parts {
        let len = base + if i < rem { 1 } else { 0 };
        if len == 0 {
            continue;
        }
        out.push(&items[start..start + len]);
        start += len;
    }
    out
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

fn build_render_prompt(
    original_prompt: &str,
    findings: &[Finding],
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

fn partial_note(idx: usize, total: usize) -> String {
    format!(
        "You are rendering partial summary {idx} of {total} for the same research run. \
         Cover only the findings provided in this chunk. A later stage will merge the \
         partials into a single final summary, so emit the sections in the template's \
         normal shape and skip any closing or global framing that would duplicate \
         across partials."
    )
}

fn combine_system_prompt(markdown: bool) -> String {
    let flavour = if markdown { "markdown" } else { "plain text" };
    format!(
        "You are merging partial summaries produced from the same research run into a \
         single {flavour} summary. Every section in the partials must appear in the \
         final output — merge duplicates (the same underlying topic or finding) rather \
         than listing them twice. Preserve the style, tone, structure, and line \
         wrapping the partials already use; do not invent new section headings or \
         framing. If the partials open with a shared contextual lead-in, keep one copy \
         at the top. Severity ordering is already baked into the partials — preserve \
         it; do not re-sort across the merged output. End the output with a blank line."
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

async fn call_and_extract(
    client: &Client,
    cfg: &CallConfig,
    messages: &[Message],
    stage: &str,
) -> Result<String> {
    let resp = client
        .messages_streaming(cfg, messages)
        .await
        .map_err(|e| anyhow!("{stage}: call failed: {e}"))?;
    let text = extract_text(&resp);
    if text.trim().is_empty() {
        return Err(anyhow!(
            "{stage}: empty body (stop_reason={:?})",
            resp.stop_reason
        ));
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
    use super::*;
    use kres_core::findings::{FindingDetail, RelevantFileSection, RelevantSymbol, TaskProse};

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
    fn severity_sort_desc_with_stable_within_band() {
        let findings = [
            f("a", Severity::Low, Status::Active, vec![]),
            f("b", Severity::High, Status::Active, vec![]),
            f("c", Severity::Medium, Status::Active, vec![]),
            f("d", Severity::High, Status::Active, vec![]),
            f("e", Severity::High, Status::Active, vec![]),
        ];
        let mut got: Vec<Finding> = findings.to_vec();
        got.sort_by(|a, b| severity_rank(b.severity).cmp(&severity_rank(a.severity)));
        let ids: Vec<&str> = got.iter().map(|x| x.id.as_str()).collect();
        // High (b, d, e) first in input order, then Medium (c), Low (a).
        assert_eq!(ids, vec!["b", "d", "e", "c", "a"]);
    }

    #[test]
    fn invalidated_findings_filtered_out() {
        let findings = [
            f("live", Severity::Medium, Status::Active, vec![]),
            f("dead", Severity::High, Status::Invalidated, vec![]),
        ];
        let kept: Vec<&Finding> = findings
            .iter()
            .filter(|f| f.status != Status::Invalidated)
            .collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].id, "live");
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
