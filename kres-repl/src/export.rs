//! `kres --export <dir>` — emit a per-finding folder tree from
//! `findings.json`.
//!
//! For each finding in the store, the export writes:
//!
//!   <dir>/<tag>/metadata.yaml  structured metadata (id, severity,
//!                              git HEAD sha/subject, cross-refs,
//!                              symbol and file-section locations)
//!   <dir>/<tag>/FINDING.md     human-readable full body: summary,
//!                              mechanism, reproducer, impact, fix
//!                              sketch, open questions, per-task
//!                              analysis details
//!
//! `<tag>` is the finding's `id`, sanitized so it's safe as a
//! directory name. Collisions after sanitizing get a numeric suffix.
//!
//! The metadata.yaml body comes from a tiny mustache-like template
//! embedded at build time (`configs/prompts/export-metadata.yaml`);
//! operators can shadow the embedded copy by dropping a file at
//! `~/.kres/prompts/export-metadata.yaml`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;
use kres_core::findings::{Finding, FindingDetail, FindingsStore, Severity, Status};

/// Embedded default for the metadata template. Operator overrides
/// live at `~/.kres/prompts/export-metadata.yaml`.
const METADATA_TEMPLATE: &str = include_str!("../../configs/prompts/export-metadata.yaml");

/// Inputs to a single export run.
pub struct ExportInputs {
    /// Path to `findings.json`. Required.
    pub findings_path: PathBuf,
    /// Target directory. Created if missing.
    pub output_dir: PathBuf,
    /// Workspace the findings refer to. Used to probe `git HEAD` so
    /// each exported finding carries the commit state the analysis
    /// was performed against.
    pub workspace: PathBuf,
}

/// Per-run workspace-git snapshot. Empty strings if the workspace
/// isn't a git repo or `git` isn't on `$PATH`.
struct GitHead {
    sha: String,
    subject: String,
}

pub async fn run_export(inputs: ExportInputs) -> Result<()> {
    let ExportInputs {
        findings_path,
        output_dir,
        workspace,
    } = inputs;

    if !findings_path.exists() {
        return Err(anyhow::anyhow!(
            "--export: findings file {} does not exist",
            findings_path.display()
        ));
    }

    std::fs::create_dir_all(&output_dir)
        .with_context(|| format!("creating export dir {}", output_dir.display()))?;

    let store = FindingsStore::new(&findings_path)
        .await
        .with_context(|| format!("loading findings {}", findings_path.display()))?;
    let findings = store.snapshot().await;
    let git = probe_git_head(&workspace);
    let template = load_metadata_template();

    // First pass: assign a stable per-finding tag so FINDING.md's
    // Related section can resolve every id to the directory we're
    // about to create. Collisions after sanitize_tag get a numeric
    // suffix via unique_tag, so this is the authoritative id→tag
    // table for the whole export.
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut id_to_tag: std::collections::HashMap<String, String> =
        std::collections::HashMap::with_capacity(findings.len());
    for f in &findings {
        let tag = unique_tag(&f.id, &mut used);
        id_to_tag.insert(f.id.clone(), tag);
    }

    let mut written = 0usize;
    for f in &findings {
        let tag = &id_to_tag[&f.id];
        let finding_dir = output_dir.join(tag);
        std::fs::create_dir_all(&finding_dir)
            .with_context(|| format!("creating {}", finding_dir.display()))?;
        write_metadata_yaml(&finding_dir.join("metadata.yaml"), f, &git, &template)?;
        write_finding_md(&finding_dir.join("FINDING.md"), f, &id_to_tag)?;
        written += 1;
    }

    eprintln!(
        "--export: wrote {} finding(s) to {}",
        written,
        output_dir.display()
    );
    // Regenerate INDEX.md so every --export run leaves a ready-to-read
    // top-level overview alongside the per-finding folders. Parses the
    // metadata.yaml files we just wrote rather than reusing the
    // in-memory findings list — keeps the code path identical to
    // `--export-index` so the two outputs can't drift.
    let index = run_export_index(&output_dir)?;
    eprintln!("--export: index   = {}", index.display());
    Ok(())
}

/// Walk `<dir>/*/metadata.yaml` and write `<dir>/INDEX.md` — one
/// row per finding, grouped by severity (High → Medium → Low), and
/// inside each group ordered by `date` ascending so the
/// longest-standing bug sits at the top. Findings with no date sink
/// to the bottom of their group but remain present.
pub fn run_export_index(dir: &Path) -> Result<PathBuf> {
    if !dir.is_dir() {
        return Err(anyhow::anyhow!(
            "--export-index: {} is not a directory",
            dir.display()
        ));
    }
    let mut rows: Vec<IndexRow> = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let meta = entry.path().join("metadata.yaml");
        if !meta.exists() {
            continue;
        }
        let yaml = std::fs::read_to_string(&meta)
            .with_context(|| format!("reading {}", meta.display()))?;
        rows.push(IndexRow {
            tag: entry.file_name().to_string_lossy().into_owned(),
            id: top_level_scalar(&yaml, "id").unwrap_or_default(),
            title: top_level_scalar(&yaml, "title").unwrap_or_default(),
            severity: parse_severity(top_level_scalar(&yaml, "severity").as_deref().unwrap_or("")),
            status: top_level_scalar(&yaml, "status").unwrap_or_else(|| "active".to_string()),
            date: top_level_scalar(&yaml, "date"),
        });
    }
    rows.sort_by(|a, b| {
        // Severity desc (High first); within a tier, oldest date
        // first; None dates go to the end of their tier; finally fall
        // back to id for determinism.
        let sev = severity_sort_key(b.severity).cmp(&severity_sort_key(a.severity));
        if sev != std::cmp::Ordering::Equal {
            return sev;
        }
        match (a.date.as_deref(), b.date.as_deref()) {
            (Some(x), Some(y)) => x.cmp(y).then_with(|| a.id.cmp(&b.id)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.id.cmp(&b.id),
        }
    });

    let out_path = dir.join("INDEX.md");
    std::fs::write(&out_path, render_index(&rows))
        .with_context(|| format!("writing {}", out_path.display()))?;
    Ok(out_path)
}

#[derive(Debug)]
struct IndexRow {
    tag: String,
    id: String,
    title: String,
    severity: Option<Severity>,
    status: String,
    date: Option<String>,
}

fn severity_sort_key(s: Option<Severity>) -> u8 {
    match s {
        Some(Severity::High) => 3,
        Some(Severity::Medium) => 2,
        Some(Severity::Low) => 1,
        None => 0,
    }
}

fn parse_severity(s: &str) -> Option<Severity> {
    match s {
        "low" => Some(Severity::Low),
        "medium" => Some(Severity::Medium),
        "high" => Some(Severity::High),
        _ => None,
    }
}

/// Parse a top-level scalar field from our generated metadata.yaml.
/// Recognises two shapes:
///   key: "quoted value"
///   key: unquoted-value
/// Ignores indented continuations and nested mappings. Returns the
/// raw string value (quotes and backslash escapes unwrapped).
fn top_level_scalar(yaml: &str, key: &str) -> Option<String> {
    let needle = format!("{key}: ");
    for line in yaml.lines() {
        // Indented lines belong to nested mappings / list items.
        if line.starts_with(' ') || line.starts_with('\t') {
            continue;
        }
        let Some(rest) = line.strip_prefix(&needle) else {
            continue;
        };
        let rest = rest.trim();
        if let Some(inner) = rest.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
            return Some(unquote_yaml(inner));
        }
        return Some(rest.to_string());
    }
    None
}

/// Reverse of yaml_scalar: unwrap backslash-escapes we know about.
/// Unknown escapes pass through as the literal char.
fn unquote_yaml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

fn render_index(rows: &[IndexRow]) -> String {
    let mut out = String::new();
    out.push_str("# kres findings index\n\n");
    let ts = chrono::Utc::now().to_rfc3339();
    out.push_str(&format!("_generated: {ts}_\n\n"));
    if rows.is_empty() {
        out.push_str("(no findings)\n");
        return out;
    }
    let (h, m, l, u) = rows
        .iter()
        .fold((0, 0, 0, 0), |(h, m, l, u), r| match r.severity {
            Some(Severity::High) => (h + 1, m, l, u),
            Some(Severity::Medium) => (h, m + 1, l, u),
            Some(Severity::Low) => (h, m, l + 1, u),
            None => (h, m, l, u + 1),
        });
    out.push_str(&format!(
        "{} finding(s): {} high, {} medium, {} low",
        rows.len(),
        h,
        m,
        l
    ));
    if u > 0 {
        out.push_str(&format!(", {u} unknown-severity"));
    }
    out.push_str("\n\n");
    out.push_str("| Severity | Date | Status | ID | Title |\n");
    out.push_str("|---|---|---|---|---|\n");
    for r in rows {
        let sev = r
            .severity
            .map(|s| match s {
                Severity::High => "high",
                Severity::Medium => "medium",
                Severity::Low => "low",
            })
            .unwrap_or("?");
        let date = r.date.as_deref().unwrap_or("—");
        let title = escape_md_table_cell(&r.title);
        out.push_str(&format!(
            "| {sev} | {date} | {status} | [`{id}`]({tag}/FINDING.md) | {title} |\n",
            status = r.status,
            id = r.id,
            tag = r.tag,
            title = title,
        ));
    }
    out
}

fn escape_md_table_cell(s: &str) -> String {
    // Pipes break GFM table cells; newlines break the row. Replace
    // both with something that keeps the row intact.
    s.replace('|', "\\|").replace('\n', " ")
}

/// Disk override wins when it exists and is non-empty; else the
/// compiled-in copy. Mirrors the `~/.kres/commands/<name>.md`
/// convention used by `user_commands`, but under
/// `~/.kres/prompts/` so we don't crowd the slash-commands namespace.
fn load_metadata_template() -> String {
    if let Some(home) = dirs::home_dir() {
        let p = home
            .join(".kres")
            .join("prompts")
            .join("export-metadata.yaml");
        if let Ok(s) = std::fs::read_to_string(&p) {
            if !s.trim().is_empty() {
                return s;
            }
        }
    }
    METADATA_TEMPLATE.to_string()
}

/// Turn an arbitrary finding id into a directory-safe tag.
fn sanitize_tag(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    let mut prev_underscore = false;
    for c in id.chars() {
        let keep = c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.';
        if keep {
            out.push(c);
            prev_underscore = false;
        } else if !prev_underscore {
            out.push('_');
            prev_underscore = true;
        }
    }
    let trimmed = out.trim_matches('_').to_string();
    if trimmed.is_empty() {
        "finding".to_string()
    } else {
        trimmed
    }
}

fn unique_tag(id: &str, used: &mut std::collections::HashSet<String>) -> String {
    let base = sanitize_tag(id);
    if used.insert(base.clone()) {
        return base;
    }
    for n in 2u32.. {
        let candidate = format!("{base}-{n}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
    unreachable!("u32 exhausted sanitizing tag")
}

fn probe_git_head(workspace: &Path) -> GitHead {
    GitHead {
        sha: run_git(workspace, &["rev-parse", "HEAD"]).unwrap_or_default(),
        subject: run_git(workspace, &["log", "-1", "--format=%s"]).unwrap_or_default(),
    }
}

fn run_git(workspace: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn write_metadata_yaml(path: &Path, f: &Finding, git: &GitHead, template: &str) -> Result<()> {
    let ctx = build_context(f, git);
    let body = render(template, &ctx);
    std::fs::write(path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn severity_str(s: Severity) -> &'static str {
    match s {
        Severity::Low => "low",
        Severity::Medium => "medium",
        Severity::High => "high",
    }
}

fn status_str(s: Status) -> &'static str {
    match s {
        Status::Active => "active",
        Status::Invalidated => "invalidated",
    }
}

// ---------------------------------------------------------------
// Tiny mustache-like template engine.
//
// Supported syntax:
//   {{key}}          scalar, auto-quoted as YAML double-quoted string
//   {{!key}}         scalar, emitted raw (for enums / ints already
//                    safe to inline)
//   {{#key}}...{{/key}}
//                    section. When the value is a list, the inner is
//                    rendered once per item with the item's fields in
//                    scope. When the value is a non-empty scalar the
//                    inner is rendered once. Missing / empty → skip.
//
// Scope: a single parent `Ctx` plus a per-iteration item map during
// list sections. Nested sections use the same lookup rule: item
// fields shadow parent.
// ---------------------------------------------------------------

#[derive(Debug, Clone)]
enum Value {
    Scalar(String),
    Items(Vec<BTreeMap<String, Value>>),
}

type Ctx = BTreeMap<String, Value>;

fn build_context(f: &Finding, git: &GitHead) -> Ctx {
    let mut c: Ctx = BTreeMap::new();
    c.insert("id".into(), Value::Scalar(f.id.clone()));
    c.insert("title".into(), Value::Scalar(f.title.clone()));
    c.insert(
        "severity".into(),
        Value::Scalar(severity_str(f.severity).into()),
    );
    c.insert("status".into(), Value::Scalar(status_str(f.status).into()));
    c.insert("git_sha".into(), Value::Scalar(git.sha.clone()));
    c.insert("git_subject".into(), Value::Scalar(git.subject.clone()));

    // Use first_seen_at when the finding carries one; fall back to
    // wall-clock now for legacy records (pre-first_seen_at findings.json
    // files have None on every entry). The fallback means a re-export
    // of a legacy store keeps a stamped `date:` line, at the cost of
    // the date drifting on each export — acceptable because new
    // findings going forward carry their real discovery date. Format
    // is calendar-date only (YYYY-MM-DD); second-precision was noise
    // for the reader and drifted on every re-export anyway.
    let date_ts = f.first_seen_at.unwrap_or_else(Utc::now);
    c.insert("has_date".into(), Value::Scalar("1".into()));
    c.insert(
        "date".into(),
        Value::Scalar(date_ts.format("%Y-%m-%d").to_string()),
    );
    if let Some(ref ib) = f.introduced_by {
        if !ib.sha.is_empty() {
            c.insert("has_introduced_by".into(), Value::Scalar("1".into()));
            c.insert("introduced_by_sha".into(), Value::Scalar(ib.sha.clone()));
            if !ib.subject.is_empty() {
                c.insert(
                    "has_introduced_by_subject".into(),
                    Value::Scalar("1".into()),
                );
                c.insert(
                    "introduced_by_subject".into(),
                    Value::Scalar(ib.subject.clone()),
                );
            }
        }
    }
    if let Some(ref t) = f.first_seen_task {
        c.insert("has_first_seen_task".into(), Value::Scalar("1".into()));
        c.insert("first_seen_task".into(), Value::Scalar(t.clone()));
    }
    if let Some(ref t) = f.last_updated_task {
        c.insert("has_last_updated_task".into(), Value::Scalar("1".into()));
        c.insert("last_updated_task".into(), Value::Scalar(t.clone()));
    }

    if !f.related_finding_ids.is_empty() {
        c.insert("has_related_finding_ids".into(), Value::Scalar("1".into()));
        let items = f
            .related_finding_ids
            .iter()
            .map(|id| {
                let mut m = BTreeMap::new();
                m.insert("item".into(), Value::Scalar(id.clone()));
                m
            })
            .collect();
        c.insert("related_finding_ids".into(), Value::Items(items));
    }
    if !f.relevant_symbols.is_empty() {
        c.insert("has_relevant_symbols".into(), Value::Scalar("1".into()));
        let items = f
            .relevant_symbols
            .iter()
            .map(|s| {
                let mut m = BTreeMap::new();
                m.insert("name".into(), Value::Scalar(s.name.clone()));
                m.insert("filename".into(), Value::Scalar(s.filename.clone()));
                m.insert("line".into(), Value::Scalar(s.line.to_string()));
                m
            })
            .collect();
        c.insert("relevant_symbols".into(), Value::Items(items));
    }
    if !f.relevant_file_sections.is_empty() {
        c.insert(
            "has_relevant_file_sections".into(),
            Value::Scalar("1".into()),
        );
        let items = f
            .relevant_file_sections
            .iter()
            .map(|s| {
                let mut m = BTreeMap::new();
                m.insert("filename".into(), Value::Scalar(s.filename.clone()));
                m.insert("line_start".into(), Value::Scalar(s.line_start.to_string()));
                m.insert("line_end".into(), Value::Scalar(s.line_end.to_string()));
                m
            })
            .collect();
        c.insert("relevant_file_sections".into(), Value::Items(items));
    }
    if !f.open_questions.is_empty() {
        c.insert("has_open_questions".into(), Value::Scalar("1".into()));
        let items = f
            .open_questions
            .iter()
            .map(|q| {
                let mut m = BTreeMap::new();
                m.insert("item".into(), Value::Scalar(q.clone()));
                m
            })
            .collect();
        c.insert("open_questions".into(), Value::Items(items));
    }

    c
}

/// Render `template` against `ctx`. Lookup for a key inside a list
/// iteration first checks the item's own fields, then the parent
/// context.
fn render(template: &str, ctx: &Ctx) -> String {
    render_scoped(template, ctx, None)
}

fn render_scoped(template: &str, parent: &Ctx, item: Option<&Ctx>) -> String {
    let mut out = String::new();
    let bytes = template.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        let Some(open) = find_subseq(bytes, b"{{", i) else {
            out.push_str(&template[i..]);
            break;
        };
        out.push_str(&template[i..open]);
        let Some(close) = find_subseq(bytes, b"}}", open + 2) else {
            // Unterminated tag — emit literally and stop.
            out.push_str(&template[open..]);
            break;
        };
        let tag = template[open + 2..close].trim();
        let after = close + 2;

        if let Some(name) = tag.strip_prefix('#') {
            let name = name.trim().to_string();
            let Some((inner_end, end_tag_end)) = find_section_end(bytes, after, &name) else {
                // Unmatched section open — emit literally and stop.
                out.push_str(&template[open..]);
                break;
            };
            let inner = &template[after..inner_end];
            match lookup(parent, item, &name) {
                Some(Value::Items(items)) => {
                    for it in items {
                        out.push_str(&render_scoped(inner, parent, Some(it)));
                    }
                }
                Some(Value::Scalar(s)) if !s.is_empty() => {
                    out.push_str(&render_scoped(inner, parent, item));
                }
                _ => {}
            }
            i = end_tag_end;
            continue;
        }
        if tag.starts_with('/') {
            // Stray close tag outside a section — emit literally.
            out.push_str(&template[open..after]);
            i = after;
            continue;
        }
        let (raw, name) = if let Some(rest) = tag.strip_prefix('!') {
            (true, rest.trim())
        } else {
            (false, tag)
        };
        if let Some(Value::Scalar(s)) = lookup(parent, item, name) {
            if raw {
                out.push_str(s);
            } else {
                out.push_str(&yaml_scalar(s));
            }
        }
        i = after;
    }
    out
}

fn lookup<'a>(parent: &'a Ctx, item: Option<&'a Ctx>, name: &str) -> Option<&'a Value> {
    if let Some(it) = item {
        if let Some(v) = it.get(name) {
            return Some(v);
        }
    }
    parent.get(name)
}

/// Find `{{/name}}` balanced with any nested `{{#name}}` openings.
/// Returns (inner_end, end_tag_end) where inner_end is the byte
/// index of the `{{` of the closing tag and end_tag_end is one past
/// the closing `}}`.
fn find_section_end(bytes: &[u8], from: usize, name: &str) -> Option<(usize, usize)> {
    let mut depth = 1usize;
    let mut i = from;
    while i < bytes.len() {
        let open = find_subseq(bytes, b"{{", i)?;
        let close = find_subseq(bytes, b"}}", open + 2)?;
        let tag = std::str::from_utf8(&bytes[open + 2..close]).ok()?.trim();
        if let Some(n) = tag.strip_prefix('#') {
            if n.trim() == name {
                depth += 1;
            }
        } else if let Some(n) = tag.strip_prefix('/') {
            if n.trim() == name {
                depth -= 1;
                if depth == 0 {
                    return Some((open, close + 2));
                }
            }
        }
        i = close + 2;
    }
    None
}

fn find_subseq(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from >= hay.len() || hay.len() < needle.len() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|off| off + from)
}

/// Quote `s` as a YAML double-quoted scalar. Always quotes so we
/// don't have to reason about special unquoted forms (numbers,
/// booleans, null, leading `-`, embedded `:`, etc.).
fn yaml_scalar(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn write_finding_md(
    path: &Path,
    f: &Finding,
    id_to_tag: &std::collections::HashMap<String, String>,
) -> Result<()> {
    let mut m = String::new();
    m.push_str(&format!("# `{}` — {}\n\n", f.id, f.title));
    m.push_str(&format!(
        "**Severity:** {}  \n**Status:** {}\n\n",
        severity_str(f.severity),
        status_str(f.status)
    ));
    if let Some(ref ib) = f.introduced_by {
        if !ib.sha.is_empty() {
            if ib.subject.is_empty() {
                m.push_str(&format!("**Introduced by:** `{}`  \n", ib.sha));
            } else {
                m.push_str(&format!(
                    "**Introduced by:** `{}` — {}  \n",
                    ib.sha, ib.subject
                ));
            }
        }
    }
    if let Some(ref t) = f.first_seen_task {
        m.push_str(&format!("**First seen:** `{}`  \n", t));
    }
    if let Some(ref t) = f.last_updated_task {
        m.push_str(&format!("**Last updated:** `{}`  \n", t));
    }
    if !f.related_finding_ids.is_empty() {
        m.push_str("**Related:** ");
        m.push_str(
            &f.related_finding_ids
                .iter()
                .map(|id| match id_to_tag.get(id) {
                    Some(tag) => format!("[`{id}`](../{tag}/FINDING.md)"),
                    None => format!("`{id}`"),
                })
                .collect::<Vec<_>>()
                .join(", "),
        );
        m.push('\n');
    }
    m.push('\n');

    m.push_str("## Summary\n\n");
    m.push_str(&f.summary);
    m.push_str("\n\n");

    if let Some(ref md) = f.mechanism_detail {
        if !md.is_empty() {
            m.push_str("## Mechanism\n\n");
            m.push_str(md);
            m.push_str("\n\n");
        }
    }

    m.push_str("## Reproducer\n\n");
    m.push_str(&f.reproducer_sketch);
    m.push_str("\n\n## Impact\n\n");
    m.push_str(&f.impact);
    m.push_str("\n\n");

    if let Some(ref fx) = f.fix_sketch {
        if !fx.is_empty() {
            m.push_str("## Fix sketch\n\n");
            m.push_str(fx);
            m.push_str("\n\n");
        }
    }

    if !f.open_questions.is_empty() {
        m.push_str("## Open questions\n\n");
        for q in &f.open_questions {
            m.push_str(&format!("- {q}\n"));
        }
        m.push('\n');
    }

    if !f.relevant_symbols.is_empty() {
        m.push_str("## Relevant symbols\n\n");
        for s in &f.relevant_symbols {
            m.push_str(&format!("- `{}` at `{}:{}`\n", s.name, s.filename, s.line));
            if !s.definition.is_empty() {
                m.push_str("  ```\n");
                for line in s.definition.lines() {
                    m.push_str("  ");
                    m.push_str(line);
                    m.push('\n');
                }
                m.push_str("  ```\n");
            }
        }
        m.push('\n');
    }

    if !f.relevant_file_sections.is_empty() {
        m.push_str("## Relevant file sections\n\n");
        for sec in &f.relevant_file_sections {
            m.push_str(&format!(
                "### `{}` lines {}–{}\n\n",
                sec.filename, sec.line_start, sec.line_end
            ));
            if !sec.content.is_empty() {
                m.push_str("```\n");
                m.push_str(&sec.content);
                if !sec.content.ends_with('\n') {
                    m.push('\n');
                }
                m.push_str("```\n\n");
            }
        }
    }

    if !f.details.is_empty() {
        m.push_str("## Task details\n\n");
        for d in &f.details {
            render_detail(&mut m, d);
        }
    }

    std::fs::write(path, m).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn render_detail(out: &mut String, d: &FindingDetail) {
    out.push_str(&format!("### `{}`\n\n", d.task));
    out.push_str(&d.analysis);
    if !d.analysis.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;
    use kres_core::findings::{IntroducedBy, RelevantFileSection, RelevantSymbol};

    fn finding_sample() -> Finding {
        Finding {
            id: "race_in_cq_ack".into(),
            title: "Race in CQ ack".into(),
            severity: Severity::High,
            status: Status::Active,
            relevant_symbols: vec![RelevantSymbol {
                name: "cq_ack".into(),
                filename: "drivers/net/x.c".into(),
                line: 42,
                definition: "void cq_ack(void) {}".into(),
            }],
            relevant_file_sections: vec![RelevantFileSection {
                filename: "drivers/net/x.c".into(),
                line_start: 40,
                line_end: 50,
                content: "void cq_ack(void) {\n}\n".into(),
            }],
            summary: "s".into(),
            reproducer_sketch: "r".into(),
            impact: "i".into(),
            mechanism_detail: None,
            fix_sketch: None,
            open_questions: vec!["What about A?".into()],
            first_seen_task: Some("t1".into()),
            last_updated_task: Some("t2".into()),
            related_finding_ids: vec!["rel_a".into()],
            reactivate: false,
            details: vec![],
            introduced_by: None,
            first_seen_at: None,
        }
    }

    fn git_sample() -> GitHead {
        GitHead {
            sha: "abc123".into(),
            subject: "a \"quoted\" subject".into(),
        }
    }

    #[test]
    fn sanitize_keeps_safe_chars_and_collapses_the_rest() {
        assert_eq!(sanitize_tag("race-in-cq_ack"), "race-in-cq_ack");
        assert_eq!(sanitize_tag("foo/bar baz"), "foo_bar_baz");
        assert_eq!(sanitize_tag("///leading"), "leading");
        assert_eq!(sanitize_tag(""), "finding");
    }

    #[test]
    fn unique_tag_appends_suffix_on_collision() {
        let mut used = std::collections::HashSet::new();
        assert_eq!(unique_tag("a/b", &mut used), "a_b");
        assert_eq!(unique_tag("a b", &mut used), "a_b-2");
        assert_eq!(unique_tag("a!b", &mut used), "a_b-3");
    }

    #[test]
    fn yaml_scalar_quotes_and_escapes() {
        assert_eq!(yaml_scalar("plain"), "\"plain\"");
        assert_eq!(yaml_scalar("a \"quoted\""), "\"a \\\"quoted\\\"\"");
        assert_eq!(yaml_scalar("line1\nline2"), "\"line1\\nline2\"");
        assert_eq!(yaml_scalar("back\\slash"), "\"back\\\\slash\"");
    }

    #[test]
    fn render_scalars_raw_and_quoted() {
        let mut ctx: Ctx = BTreeMap::new();
        ctx.insert("id".into(), Value::Scalar("race_x".into()));
        ctx.insert("severity".into(), Value::Scalar("high".into()));
        let t = "id: {{id}}\nseverity: {{!severity}}\n";
        assert_eq!(render(t, &ctx), "id: \"race_x\"\nseverity: high\n");
    }

    #[test]
    fn render_section_skipped_when_missing_or_empty() {
        let ctx: Ctx = BTreeMap::new();
        let t = "a\n{{#has_x}}inside\n{{/has_x}}b\n";
        assert_eq!(render(t, &ctx), "a\nb\n");
    }

    #[test]
    fn render_list_section_iterates_items() {
        let mut ctx: Ctx = BTreeMap::new();
        ctx.insert("has_list".into(), Value::Scalar("1".into()));
        let items = vec![
            {
                let mut m = BTreeMap::new();
                m.insert("item".into(), Value::Scalar("a".into()));
                m
            },
            {
                let mut m = BTreeMap::new();
                m.insert("item".into(), Value::Scalar("b".into()));
                m
            },
        ];
        ctx.insert("list".into(), Value::Items(items));
        let t = "{{#has_list}}list:\n{{#list}}  - {{item}}\n{{/list}}{{/has_list}}";
        assert_eq!(render(t, &ctx), "list:\n  - \"a\"\n  - \"b\"\n");
    }

    #[test]
    fn embedded_template_renders_against_real_finding() {
        let out = render(
            METADATA_TEMPLATE,
            &build_context(&finding_sample(), &git_sample()),
        );
        assert!(out.contains("id: \"race_in_cq_ack\""));
        assert!(out.contains("severity: high\n"));
        assert!(out.contains("status: active\n"));
        assert!(out.contains("sha: \"abc123\""));
        // double-quotes inside the commit subject must be escaped.
        assert!(out.contains("subject: \"a \\\"quoted\\\" subject\""));
        assert!(out.contains("first_seen_task: \"t1\""));
        assert!(out.contains("related_finding_ids:\n  - \"rel_a\"\n"));
        assert!(out.contains("relevant_symbols:\n"));
        assert!(out.contains("    line: 42\n"));
        assert!(out.contains("relevant_file_sections:\n"));
        assert!(out.contains("    line_start: 40\n"));
        assert!(out.contains("open_questions:\n  - \"What about A?\"\n"));
    }

    fn tmp_dir(nonce: &str) -> std::path::PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "kres-export-test-{}-{}-{:x}",
            nonce,
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn top_level_scalar_parses_quoted_and_raw() {
        let y = "id: \"race_x\"\nseverity: high\n  nested: ignore\nstatus: active\n";
        assert_eq!(top_level_scalar(y, "id").as_deref(), Some("race_x"));
        assert_eq!(top_level_scalar(y, "severity").as_deref(), Some("high"));
        assert_eq!(top_level_scalar(y, "status").as_deref(), Some("active"));
        assert_eq!(top_level_scalar(y, "nested"), None, "indented line ignored");
        assert_eq!(top_level_scalar(y, "missing"), None);
    }

    #[test]
    fn top_level_scalar_unquotes_escapes() {
        let y = "title: \"a \\\"quoted\\\" title\"\n";
        assert_eq!(
            top_level_scalar(y, "title").as_deref(),
            Some("a \"quoted\" title")
        );
    }

    #[test]
    fn export_index_sorts_by_severity_then_date_then_id() {
        let dir = tmp_dir("export-index");
        let write = |tag: &str, body: &str| {
            let d = dir.join(tag);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("metadata.yaml"), body).unwrap();
        };
        write(
            "b_newer_high",
            "id: \"b\"\ntitle: \"newer high\"\nseverity: high\nstatus: active\ndate: \"2026-04-24T10:00:00Z\"\n",
        );
        write(
            "a_older_high",
            "id: \"a\"\ntitle: \"older high\"\nseverity: high\nstatus: active\ndate: \"2026-04-20T10:00:00Z\"\n",
        );
        write(
            "c_no_date_high",
            "id: \"c\"\ntitle: \"undated high\"\nseverity: high\nstatus: active\n",
        );
        write(
            "d_medium",
            "id: \"d\"\ntitle: \"medium\"\nseverity: medium\nstatus: active\ndate: \"2026-01-01T00:00:00Z\"\n",
        );
        write(
            "e_low",
            "id: \"e\"\ntitle: \"low\"\nseverity: low\nstatus: invalidated\ndate: \"2026-02-01T00:00:00Z\"\n",
        );
        let out = run_export_index(&dir).unwrap();
        let body = std::fs::read_to_string(&out).unwrap();
        // Severity desc, oldest-first within a tier, undated at the
        // bottom of its tier.
        let order = [
            "[`a`](a_older_high/FINDING.md)",
            "[`b`](b_newer_high/FINDING.md)",
            "[`c`](c_no_date_high/FINDING.md)",
            "[`d`](d_medium/FINDING.md)",
            "[`e`](e_low/FINDING.md)",
        ];
        let mut cursor = 0usize;
        for want in order {
            let hit = body[cursor..].find(want).unwrap_or_else(|| {
                panic!("ordering wrong; missing {want} after byte {cursor}\n{body}")
            });
            cursor += hit + want.len();
        }
        // Histogram line is present.
        assert!(body.contains("3 high, 1 medium, 1 low"), "{body}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn finding_md_related_emits_markdown_link_for_known_ids() {
        let mut f = finding_sample();
        f.related_finding_ids = vec!["present/id".into(), "absent_id".into()];
        let mut id_to_tag = std::collections::HashMap::new();
        // "present/id" sanitises to "present_id"; "absent_id" isn't
        // part of the export so no entry in the map → plain code.
        id_to_tag.insert("present/id".to_string(), "present_id".to_string());
        let dir = tmp_dir("related-links");
        let path = dir.join("FINDING.md");
        write_finding_md(&path, &f, &id_to_tag).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(
            body.contains("[`present/id`](../present_id/FINDING.md)"),
            "missing link: {body}"
        );
        // Absent id falls through to plain code formatting.
        assert!(body.contains(", `absent_id`"), "missing fallback: {body}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn embedded_template_renders_introduced_by_when_set() {
        let mut f = finding_sample();
        f.introduced_by = Some(IntroducedBy {
            sha: "deadbeef".into(),
            subject: "subsys: a regression".into(),
        });
        let out = render(METADATA_TEMPLATE, &build_context(&f, &git_sample()));
        assert!(out.contains("introduced_by:\n"));
        assert!(out.contains("  sha: \"deadbeef\"\n"));
        assert!(out.contains("  subject: \"subsys: a regression\"\n"));
    }

    #[test]
    fn embedded_template_introduced_by_sha_only() {
        let mut f = finding_sample();
        f.introduced_by = Some(IntroducedBy {
            sha: "cafebabe".into(),
            subject: "".into(),
        });
        let out = render(METADATA_TEMPLATE, &build_context(&f, &git_sample()));
        assert!(out.contains("  sha: \"cafebabe\"\n"));
        // Exactly one `subject:` line from the git block — none from
        // introduced_by when its subject is empty.
        let subject_lines = out.matches("  subject:").count();
        assert_eq!(subject_lines, 1, "only the git subject line should appear");
    }

    #[test]
    fn embedded_template_omits_introduced_by_when_unset() {
        let out = render(
            METADATA_TEMPLATE,
            &build_context(&finding_sample(), &git_sample()),
        );
        assert!(!out.contains("introduced_by"));
    }

    #[test]
    fn embedded_template_omits_empty_sections() {
        let mut f = finding_sample();
        f.relevant_symbols.clear();
        f.relevant_file_sections.clear();
        f.open_questions.clear();
        f.related_finding_ids.clear();
        f.first_seen_task = None;
        f.last_updated_task = None;
        let out = render(METADATA_TEMPLATE, &build_context(&f, &git_sample()));
        assert!(!out.contains("relevant_symbols:"));
        assert!(!out.contains("relevant_file_sections:"));
        assert!(!out.contains("open_questions:"));
        assert!(!out.contains("related_finding_ids:"));
        assert!(!out.contains("first_seen_task:"));
        assert!(!out.contains("last_updated_task:"));
        // Required scalars still render.
        assert!(out.contains("id: \"race_in_cq_ack\""));
        assert!(out.contains("severity: high"));
    }
}
