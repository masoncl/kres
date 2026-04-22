//! /summary and `kres --summary` — render a plain-text bug report from
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
use serde_json::json;

use kres_agents::AgentConfig;
use kres_core::findings::FindingsFile;
use kres_llm::{client::Client, config::CallConfig, request::Message, Model};

/// Resolve the plain-text bug-report template via the slash-command
/// lookup. Goes through `~/.kres/commands/summary.md` first (if the
/// operator wrote one) and falls back to the embedded default.
/// Panics only if the embedded table is missing the key — which
/// the module's own unit test
/// (`all_expected_commands_are_present`) prevents.
pub fn bug_summary_template() -> String {
    kres_agents::user_commands::lookup("summary")
        .expect("`summary` missing from user_commands table")
}

/// Resolve the markdown variant of the bug-report template. Same
/// two-step lookup (`~/.kres/commands/summary-markdown.md` →
/// embedded).
pub fn bug_summary_markdown_template() -> String {
    kres_agents::user_commands::lookup("summary-markdown")
        .expect("`summary-markdown` missing from user_commands table")
}

/// Default on-disk override location for the plain-text template.
/// Empty by default; an operator who wants to shadow the embedded
/// prompt drops a file at `~/.kres/commands/summary.md`. Returns
/// None when $HOME is unset.
pub fn default_template_path() -> Option<PathBuf> {
    dirs::home_dir()
        .map(|h| h.join(".kres").join("commands").join("summary.md"))
}

/// Default on-disk override location for the markdown variant.
/// `--markdown` selects this instead of the plain-text one.
pub fn default_markdown_template_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| {
        h.join(".kres")
            .join("commands")
            .join("summary-markdown.md")
    })
}

/// All the inputs to one summary run. Constructed once by either the
/// REPL command handler or the `kres --summary` main-entry path.
pub struct SummaryInputs {
    pub report_path: PathBuf,
    pub findings_path: Option<PathBuf>,
    pub output_path: PathBuf,
    /// Explicit override for the system prompt template. When Some,
    /// run_summary reads the file and errors if it cannot. When None,
    /// `~/.kres/system-prompts/bug-summary.md` wins if it exists;
    /// else the compiled-in fallback is used. When `markdown` is
    /// true the markdown variant of each resolution step is tried
    /// instead.
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

/// Build the default output path for a bug report given an optional
/// `--results` directory and an optional caller-supplied filename.
/// Filename defaults to `bug-report.txt`; when results_dir is None the
/// file lands in the current working directory.
pub fn default_output_path(results_dir: Option<&Path>, filename: Option<&str>) -> PathBuf {
    let name = filename.unwrap_or("bug-report.txt");
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

/// Run the summary pipeline. Reads report.md (required) and
/// findings.json (optional — missing is a warning, not an error),
/// sends them to the fast agent with the embedded template as the
/// system prompt, and writes the plain-text response to
/// `inputs.output_path`.
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

    let findings_missing = findings.is_empty();
    let note = if findings_missing {
        "findings.json absent or empty; derive bugs from report.md alone. Do not invent structured facts."
    } else if !findings_note.is_empty() {
        findings_note.as_str()
    } else {
        ""
    };
    let prompt_json = serde_json::to_string(&json!({
        "task": "bug_report",
        "original_prompt": inputs.original_prompt.as_deref().unwrap_or(""),
        "report_md": report_md,
        "findings": findings,
        "findings_missing": findings_missing,
        "note": note,
    }))?;

    // Resolve the system prompt template: explicit --template wins,
    // then the on-disk operator override under ~/.kres/commands/
    // (handled inside bug_summary_template / bug_summary_markdown_template
    // via user_commands::lookup), else the compiled-in default.
    // `--markdown` picks the markdown variant at each hop. Each hop
    // logs its source so operators can tell which template shaped
    // the report.
    let (disk_default, fallback_text, fallback_label): (
        Option<PathBuf>,
        String,
        &'static str,
    ) = if inputs.markdown {
        (
            default_markdown_template_path(),
            bug_summary_markdown_template(),
            "<compiled-in markdown fallback>",
        )
    } else {
        (
            default_template_path(),
            bug_summary_template(),
            "<compiled-in fallback>",
        )
    };
    let (template_src, template_text): (String, String) = if let Some(ref p) = inputs.template_path
    {
        let text = std::fs::read_to_string(p)
            .with_context(|| format!("reading template {}", p.display()))?;
        (p.display().to_string(), text)
    } else if let Some(p) = disk_default.filter(|p| p.exists()) {
        let text = std::fs::read_to_string(&p)
            .with_context(|| format!("reading template {}", p.display()))?;
        (p.display().to_string(), text)
    } else {
        (fallback_label.to_string(), fallback_text)
    };
    eprintln!("summary: template = {}", template_src);

    let mut cfg = CallConfig::defaults_for(inputs.model.clone())
        .with_max_tokens(inputs.max_tokens)
        .with_stream_label("summary");
    cfg = cfg.with_system(template_text);
    if let Some(n) = inputs.max_input_tokens {
        cfg = cfg.with_max_input_tokens(n);
    }

    let messages = vec![Message {
        role: "user".into(),
        content: prompt_json,
        cache: false,
        cached_prefix: None,
    }];

    eprintln!(
        "summary: sending to {} ({} finding(s), {} chars of report, original_prompt={})",
        inputs.model.id,
        findings.len(),
        report_md.len(),
        if inputs.original_prompt.is_some() {
            "yes"
        } else {
            "no"
        }
    );
    let resp = inputs
        .client
        .messages_streaming(&cfg, &messages)
        .await
        .map_err(|e| anyhow!("summary call failed: {e}"))?;
    let text = extract_text(&resp);
    if text.trim().is_empty() {
        return Err(anyhow!(
            "summary call returned empty body (stop_reason={:?})",
            resp.stop_reason
        ));
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
        "summary: wrote {} chars to {} (usage in={} out={})",
        text.len(),
        inputs.output_path.display(),
        resp.usage.input_tokens,
        resp.usage.output_tokens
    );
    Ok(())
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
