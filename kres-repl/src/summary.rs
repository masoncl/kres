//! /summary and `kres --summary` — render a plain-text summary from
//! a research run's report.md + findings.json.
//!
//! The summariser is backed by the `/summary` (or
//! `/summary-markdown`) slash-command template. The binary carries
//! the embedded default via `kres_agents::user_commands`, and an
//! operator can shadow it by dropping a file under
//! `~/.kres/commands/`. Resolution order inside `run_summary`:
//!   1. `inputs.template_path` (explicit `--template FILE`),
//!   2. `user_commands::lookup("summary")` /
//!      `user_commands::lookup("summary-markdown")` — which
//!      itself prefers `~/.kres/commands/<name>.md` on disk and
//!      falls back to the compiled-in default.
//!
//! Stale files under `~/.kres/prompts/` or
//! `~/.kres/system-prompts/` are never consulted from this
//! module — `~/.kres/commands/` is the canonical override path
//! for slash-command templates.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use kres_core::findings::Finding;
use serde_json::json;

use kres_agents::AgentConfig;
use kres_core::findings::FindingsFile;
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
    pub report_path: PathBuf,
    pub findings_path: Option<PathBuf>,
    pub output_path: PathBuf,
    /// Explicit override for the system prompt template. When Some,
    /// run_summary reads the file and errors if it cannot. When None,
    /// `~/.kres/commands/summary.md` wins if it exists; else the
    /// compiled-in `summary` body from `kres_agents::user_commands`
    /// is used. When `markdown` is true the `summary-markdown`
    /// variant is selected at each hop instead.
    pub template_path: Option<PathBuf>,
    /// Select the markdown variant of the template + the `.md` output
    /// filename default. Ignored when `template_path` is set (the
    /// caller has already chosen a template).
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
/// file. `kres --summary` uses this so it can issue the one-shot
/// summary call without spinning up the full orchestrator. The
/// summariser is cheap and short — the fast agent is plenty strong
/// for it, and we avoid burning slow-agent budget on formatting work.
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

/// Resolve the summariser's system-prompt template to a
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

/// Run the summary pipeline. Reads report.md (required) and
/// findings.json (optional — missing is a warning, not an error),
/// sends them to the fast agent with the embedded template as the
/// system prompt, and writes the response to `inputs.output_path`.
///
/// When the assembled prompt exceeds `max_input_tokens` (or the
/// conservative [`DEFAULT_INPUT_BUDGET`] fallback), the run switches
/// to a map-reduce shape: findings are split into chunks that each
/// fit, the template is applied to each chunk to produce a partial
/// summary, and a final combine call merges the partials into one
/// output. The single-call path stays the default when the payload
/// fits.
pub async fn run_summary(inputs: SummaryInputs) -> Result<()> {
    let report_md = std::fs::read_to_string(&inputs.report_path)
        .with_context(|| format!("reading report {}", inputs.report_path.display()))?;
    if report_md.trim().is_empty() {
        return Err(anyhow!(
            "report {} is empty — nothing to summarise",
            inputs.report_path.display()
        ));
    }

    let (findings, findings_note) = match &inputs.findings_path {
        Some(p) if p.exists() => {
            let raw = std::fs::read_to_string(p)
                .with_context(|| format!("reading findings {}", p.display()))?;
            let file: FindingsFile = serde_json::from_str(&raw)
                .with_context(|| format!("parsing findings {}", p.display()))?;
            (file.findings, String::new())
        }
        Some(p) => {
            let msg = format!(
                "warning: findings file {} does not exist; producing report from report.md only",
                p.display()
            );
            eprintln!("{msg}");
            (Vec::new(), msg)
        }
        None => {
            let msg = "warning: no findings file supplied; producing report from report.md only"
                .to_string();
            eprintln!("{msg}");
            (Vec::new(), msg)
        }
    };

    // Resolve the system prompt template: explicit --template wins,
    // else the on-disk operator override under ~/.kres/commands/,
    // else the compiled-in default. `inputs.markdown` (from the
    // `/summary-markdown` command or the `--summary-markdown` CLI
    // flag) picks the markdown variant at each hop. We read each
    // file at most once — the per-hop log line names the source so
    // operators can tell which template actually shaped the output.
    let (template_src, template_text) = resolve_template(&inputs)?;
    eprintln!("summary: template = {}", template_src);

    let mut cfg = CallConfig::defaults_for(inputs.model.clone())
        .with_max_tokens(inputs.max_tokens)
        .with_stream_label("summary");
    cfg = cfg.with_system(template_text.clone());
    if let Some(n) = inputs.max_input_tokens {
        cfg = cfg.with_max_input_tokens(n);
    }

    let budget = inputs.max_input_tokens.unwrap_or(DEFAULT_INPUT_BUDGET);
    let original_prompt = inputs.original_prompt.as_deref().unwrap_or("");
    let findings_note_opt = if findings_note.is_empty() {
        None
    } else {
        Some(findings_note.as_str())
    };

    // One-shot attempt first: build the full prompt and see if it
    // fits the budget. `count_tokens_exact` returns None on API
    // failure — fall back to a chars/4 heuristic rather than
    // assuming either direction.
    let full_prompt = build_prompt_json(original_prompt, &report_md, &findings, findings_note_opt)?;
    let full_messages = vec![user_message(&full_prompt)];
    let size = size_call(&inputs.client, &cfg, &full_messages, budget).await;
    eprintln!(
        "summary: input sizing findings={} report_chars={} tokens={:?} budget={}",
        findings.len(),
        report_md.len(),
        size,
        budget
    );

    let needs_staging = size.map(|t| t > budget as u64).unwrap_or(false);
    let text = if !needs_staging {
        eprintln!(
            "summary: single-shot to {} ({} finding(s), original_prompt={})",
            inputs.model.id,
            findings.len(),
            if original_prompt.is_empty() {
                "no"
            } else {
                "yes"
            },
        );
        call_and_extract(&inputs.client, &cfg, &full_messages, "summary").await?
    } else {
        stage_summary(
            &inputs,
            &cfg,
            original_prompt,
            &report_md,
            &findings,
            findings_note_opt,
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
    eprintln!(
        "summary: wrote {} chars to {}",
        text.len(),
        inputs.output_path.display(),
    );
    Ok(())
}

/// Map-reduce path: chunk findings into groups that each fit the
/// input budget (with the same report.md + template attached), call
/// the fast agent on each, then combine the partials into one final
/// output. Triggered by `run_summary` when the single-shot prompt
/// oversizes.
#[allow(clippy::too_many_arguments)]
async fn stage_summary(
    inputs: &SummaryInputs,
    cfg: &CallConfig,
    original_prompt: &str,
    report_md: &str,
    findings: &[Finding],
    findings_note_opt: Option<&str>,
    budget: u32,
) -> Result<String> {
    if findings.is_empty() {
        return Err(anyhow!(
            "summary prompt exceeds {} input tokens but there are no findings to chunk \
             (report.md alone overflows budget). Trim the report or raise max_input_tokens.",
            budget
        ));
    }
    let chunks = chunk_findings_to_fit(
        &inputs.client,
        cfg,
        original_prompt,
        report_md,
        findings,
        budget,
    )
    .await?;
    eprintln!(
        "summary: staging: {} chunk(s) over {} finding(s); will render partials then combine",
        chunks.len(),
        findings.len(),
    );

    let mut partials = Vec::with_capacity(chunks.len());
    for (idx, chunk) in chunks.iter().enumerate() {
        let note = partial_note(idx + 1, chunks.len(), findings_note_opt);
        let prompt_json =
            build_partial_prompt_json(original_prompt, report_md, chunk, Some(note.as_str()))?;
        let messages = vec![user_message(&prompt_json)];
        let label = format!("summary partial {}/{}", idx + 1, chunks.len());
        eprintln!(
            "summary: partial {}/{} — {} finding(s)",
            idx + 1,
            chunks.len(),
            chunk.len(),
        );
        let text = call_and_extract(&inputs.client, cfg, &messages, &label).await?;
        partials.push(text);
    }

    // Combine pass: synthesise a dedicated system prompt that tells
    // the fast agent to merge the partials without re-deriving
    // structured facts. Falls back to the same model/budget config as
    // the partials.
    let combine_system = combine_system_prompt(inputs.markdown);
    let combine_cfg = CallConfig::defaults_for(inputs.model.clone())
        .with_max_tokens(inputs.max_tokens)
        .with_stream_label("summary combine")
        .with_system(combine_system);
    let combine_cfg = match inputs.max_input_tokens {
        Some(n) => combine_cfg.with_max_input_tokens(n),
        None => combine_cfg,
    };
    let combine_json = serde_json::to_string(&json!({
        "task": "combine_summaries",
        "original_prompt": original_prompt,
        "partials": partials,
    }))?;
    let combine_messages = vec![user_message(&combine_json)];
    // Pre-size the combine call. Partials are typically smaller than
    // the source they cover, but a lens that expands prose can leave
    // the concatenation over budget. Surface that as a clear error
    // rather than letting the LLM call fail mid-stream, so the
    // operator knows to raise max_input_tokens (or trim report.md).
    let combine_size = size_call(&inputs.client, &combine_cfg, &combine_messages, budget).await;
    eprintln!(
        "summary: combine sizing partials={} tokens={:?} budget={}",
        partials.len(),
        combine_size,
        budget,
    );
    if let Some(n) = combine_size {
        if n > budget as u64 {
            return Err(anyhow!(
                "combined partials ({n} tokens) exceed the {budget}-token input budget — \
                 raise max_input_tokens or shrink report.md"
            ));
        }
    }
    eprintln!(
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
/// report.md + template) fits `budget` input tokens. Starts at 2
/// parts (the caller only invokes this after the full 1-chunk
/// payload already oversized) and doubles until every partition
/// fits or each chunk is a single finding. Returns the chunks as
/// borrowed slices.
async fn chunk_findings_to_fit<'a>(
    client: &Client,
    cfg: &CallConfig,
    original_prompt: &str,
    report_md: &str,
    findings: &'a [Finding],
    budget: u32,
) -> Result<Vec<&'a [Finding]>> {
    if findings.len() < 2 {
        return Err(anyhow!(
            "cannot chunk {} finding(s) to fit the {} input-token budget; \
             report.md alone is the overflow source",
            findings.len(),
            budget
        ));
    }
    // Size each chunk with a representative partial_note applied so
    // the probe matches the real partial call within a few bytes.
    // Picking idx/total from the current `parts` keeps the format!
    // string aligned with what the partial call will emit.
    let mut parts: usize = 2;
    loop {
        let chunks = split_evenly(findings, parts);
        let mut all_fit = true;
        for (idx, chunk) in chunks.iter().enumerate() {
            let probe_note = partial_note(idx + 1, chunks.len(), None);
            let prompt = build_partial_prompt_json(
                original_prompt,
                report_md,
                chunk,
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
                 report.md is likely the overflow source",
                budget
            ));
        }
        parts = (parts * 2).min(findings.len());
    }
}

/// Split `items` into `parts` contiguous slices, biggest-first when
/// the length doesn't divide evenly (so earlier chunks absorb the
/// remainder).
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

fn build_prompt_json(
    original_prompt: &str,
    report_md: &str,
    findings: &[Finding],
    findings_note: Option<&str>,
) -> Result<String> {
    let findings_missing = findings.is_empty();
    let note = if findings_missing {
        "findings.json absent or empty; derive the summary from report.md alone. Do not invent structured facts."
    } else {
        findings_note.unwrap_or("")
    };
    Ok(serde_json::to_string(&json!({
        "task": "summary",
        "original_prompt": original_prompt,
        "report_md": report_md,
        "findings": findings,
        "findings_missing": findings_missing,
        "note": note,
    }))?)
}

fn build_partial_prompt_json(
    original_prompt: &str,
    report_md: &str,
    findings: &[Finding],
    extra_note: Option<&str>,
) -> Result<String> {
    let note = extra_note.unwrap_or("");
    Ok(serde_json::to_string(&json!({
        "task": "summary",
        "original_prompt": original_prompt,
        "report_md": report_md,
        "findings": findings,
        "findings_missing": false,
        "note": note,
    }))?)
}

fn partial_note(idx: usize, total: usize, carry_over: Option<&str>) -> String {
    let mut n = format!(
        "You are rendering partial summary {idx} of {total} for the same research run. \
         Cover only the findings provided in this chunk. A later stage will merge the \
         partials into a single final summary, so emit the sections in the template's \
         normal shape and skip any closing or global framing that would duplicate \
         across partials."
    );
    if let Some(extra) = carry_over {
        if !extra.is_empty() {
            n.push(' ');
            n.push_str(extra);
        }
    }
    n
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
         at the top. End the output with a blank line."
    )
}

fn user_message(content: &str) -> Message {
    Message {
        role: "user".into(),
        content: content.to_string(),
        cache: false,
        cached_prefix: None,
    }
}

/// Safety factor on the chars/4 heuristic. When the cheap estimate
/// comes in at <= budget * SAFE_FRAC, we trust it and skip the
/// count_tokens_exact round-trip; the trip costs one API hit per
/// summary attempt and is pure overhead for payloads well below
/// budget. 0.75 leaves slack for the chars/4 estimate's own
/// inaccuracy (it undercounts long identifiers and multi-byte
/// code points).
const SAFE_FRAC: f64 = 0.75;

async fn count_or_estimate(client: &Client, cfg: &CallConfig, messages: &[Message]) -> Option<u64> {
    if let Some(n) = client.count_tokens_exact(cfg, messages).await {
        return Some(n);
    }
    Some(cheap_estimate(cfg, messages))
}

/// chars/4 estimate over user content + system prompt. Mirrors the
/// rate-limit path's fallback heuristic. Used both as a gate before
/// the exact count call and as the last-resort answer when the exact
/// endpoint itself fails.
fn cheap_estimate(cfg: &CallConfig, messages: &[Message]) -> u64 {
    let user_chars: usize = messages.iter().map(|m| m.content.len()).sum();
    let system_chars = cfg.system.as_ref().map(|s| s.len()).unwrap_or(0);
    ((user_chars + system_chars) as u64) / 4
}

/// Sizing gate used before every LLM call in the summary pipeline.
/// Skip the count_tokens_exact round-trip when the chars/4 estimate
/// is comfortably under budget — a ~2× cost saving on small runs.
/// When the estimate is close to (or over) budget, fall through to
/// the exact count so the staging decision reflects the real token
/// count rather than a lossy heuristic.
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
    eprintln!(
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
