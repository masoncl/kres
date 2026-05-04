//! `kres --export <dir>` — emit a per-finding folder tree from
//! `findings.json`.
//!
//! For each finding in the store, the export writes:
//!
//!   <dir>/findings/<tag>/metadata.yaml  structured metadata (id,
//!                                       severity, git HEAD sha/
//!                                       subject, cross-refs, symbol
//!                                       and file-section locations)
//!   <dir>/findings/<tag>/FINDING.md     human-readable full body:
//!                                       summary, mechanism,
//!                                       reproducer, impact, fix
//!                                       sketch, open questions,
//!                                       per-task analysis details
//!
//! `--export` writes the per-finding folders under
//! `<dir>/findings/` and installs the bundled README.md and
//! findings-index.py at the top level so the search/regen
//! tooling is ready to hand. It does *not* run the script:
//! INDEX.md and index.html are produced by the separate
//! `--export-index <dir>` mode, which shells out to python3.
//! Splitting the two keeps `--export` free of any python3
//! dependency at run time.
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
    // Per-finding folders live under <output_dir>/findings/ so the
    // top-level dir holds only entry-point files (INDEX.md,
    // index.html, findings-index.py, …). Created up front so the
    // per-tag mkdir below sees an existing parent.
    let findings_root = output_dir.join("findings");
    std::fs::create_dir_all(&findings_root)
        .with_context(|| format!("creating findings dir {}", findings_root.display()))?;

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
        let finding_dir = findings_root.join(tag);
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
    // Drop the bundled README + findings-index.py alongside the
    // per-finding folders so operators have the search/regen tooling
    // ready to hand. We install only — running `--generate` is left
    // to `--export-index DIR`, which keeps `--export` free of any
    // python3 dependency.
    install_export_readme(&output_dir);
    install_index_script(&output_dir);
    Ok(())
}

/// Bundled findings index + search tool. Compiled into the kres
/// binary so it travels with the release; copied into the export
/// directory by [`run_index_script`] on first use, then invoked with
/// `--generate` to refresh INDEX.md and index.html.
///
/// The script lives at `scripts/findings-index.py` in the source
/// tree. Operators can edit the per-export-dir copy freely — kres
/// won't overwrite it on re-runs of `--export` or `--export-index`.
/// Beyond the regen path kres drives, the same script also exposes
/// `--search QUERY` for ad-hoc filtering of the dir from the shell.
const INDEX_SCRIPT_BODY: &str = include_str!("../../scripts/findings-index.py");
const INDEX_SCRIPT_NAME: &str = "findings-index.py";

/// Bundled README explaining the export layout and how to use
/// findings-index.py. Same install discipline as the script: copied
/// into the export dir on first use; never overwritten on re-run, so
/// operator edits survive a kres rebuild.
const EXPORT_README_BODY: &str = include_str!("../../scripts/export-README.md");
const EXPORT_README_NAME: &str = "README.md";

/// Install `findings-index.py` into `dir` (if absent) and run it
/// with cwd = `dir`. The script walks `<dir>/*/metadata.yaml`, sorts
/// the rows by severity then date then id, and writes both
/// `<dir>/INDEX.md` and `<dir>/index.html`. kres no longer renders
/// either file directly so operators can iterate on layout, columns,
/// or filters without rebuilding the binary — they edit the per-export
/// copy of the script in place.
///
/// Returns the expected `INDEX.md` path. Whether the file actually
/// exists on return depends on the script: a missing python3 or a
/// hand-edit that errors out leaves the markdown un-refreshed and
/// `run_index_script` logs a diagnostic to stderr.
pub fn run_export_index(dir: &Path) -> Result<PathBuf> {
    if !dir.is_dir() {
        return Err(anyhow::anyhow!(
            "--export-index: {} is not a directory",
            dir.display()
        ));
    }
    install_export_readme(dir);
    run_index_script(dir);
    Ok(dir.join("INDEX.md"))
}

/// Install the bundled `README.md` into `dir` if it isn't already
/// there. Same don't-overwrite rule as `run_index_script`: an
/// operator-edited README survives kres re-runs. Failures log to
/// stderr but do not propagate — the README is documentation, not
/// load-bearing for the export.
fn install_export_readme(dir: &Path) {
    let readme_path = dir.join(EXPORT_README_NAME);
    if readme_path.exists() {
        return;
    }
    if let Err(e) = std::fs::write(&readme_path, EXPORT_README_BODY) {
        eprintln!("--export: couldn't install {} ({e})", readme_path.display());
        return;
    }
    eprintln!("--export: installed {}", readme_path.display());
}

/// Install the bundled findings-index.py into `dir` if it isn't
/// already there. Returns the script path on success, `None` on a
/// write/chmod error (already logged to stderr). Don't-overwrite
/// rule matches the README install: an operator-edited script
/// survives kres re-runs.
fn install_index_script(dir: &Path) -> Option<PathBuf> {
    let script_path = dir.join(INDEX_SCRIPT_NAME);
    if script_path.exists() {
        return Some(script_path);
    }
    if let Err(e) = std::fs::write(&script_path, INDEX_SCRIPT_BODY) {
        eprintln!("--export: couldn't install {} ({e})", script_path.display());
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        if let Err(e) = std::fs::set_permissions(&script_path, perms) {
            eprintln!("--export: couldn't chmod {} ({e})", script_path.display());
            return None;
        }
    }
    eprintln!("--export: installed {}", script_path.display());
    Some(script_path)
}

/// Install findings-index.py (if needed) and invoke it with
/// `--generate` to regenerate INDEX.md and index.html. Failures
/// along either step print a diagnostic to stderr but do not
/// propagate — the script itself is editable by the operator, so
/// a missing python interpreter or a hand-edited script that
/// errors out shouldn't abort an otherwise-successful export.
fn run_index_script(dir: &Path) {
    let Some(script_path) = install_index_script(dir) else {
        return;
    };
    let status = std::process::Command::new(&script_path)
        .arg("--generate")
        .current_dir(dir)
        .status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => eprintln!("--export: {} exited with {}", script_path.display(), s),
        Err(e) => eprintln!("--export: failed to run {} ({e})", script_path.display()),
    }
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

/// Pick the canonical top-level filename for a finding.
/// Order: first relevant symbol → first relevant file section → "".
fn primary_filename(f: &Finding) -> String {
    if let Some(sym) = f.relevant_symbols.first() {
        if !sym.filename.is_empty() {
            return sym.filename.clone();
        }
    }
    if let Some(sec) = f.relevant_file_sections.first() {
        if !sec.filename.is_empty() {
            return sec.filename.clone();
        }
    }
    String::new()
}

fn status_str(s: Status) -> &'static str {
    match s {
        Status::Active => "active",
        Status::Unconfirmed => "unconfirmed",
        Status::Fixed => "fixed",
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
    // Canonical top-level filename: prefer the first relevant symbol's
    // file (named code site), fall back to the first relevant file
    // section, then empty. The template emits the field unconditionally
    // so an empty value renders as `filename: ""` — readers can grep
    // for unattributed findings without writing a tri-state check.
    let primary_filename = primary_filename(f);
    c.insert("filename".into(), Value::Scalar(primary_filename));
    // Subsystem is not currently in the Finding schema; leave the slot
    // present so readers and downstream tools see a consistent shape.
    // A later todo will derive this from `filename` via a path-prefix
    // rule.
    c.insert("subsystem".into(), Value::Scalar(String::new()));

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
    fn export_index_sorts_by_severity_then_date_then_id() {
        let dir = tmp_dir("export-index");
        let write = |tag: &str, body: &str| {
            let d = dir.join("findings").join(tag);
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
            "[`a`](findings/a_older_high/FINDING.md)",
            "[`b`](findings/b_newer_high/FINDING.md)",
            "[`c`](findings/c_no_date_high/FINDING.md)",
            "[`d`](findings/d_medium/FINDING.md)",
            "[`e`](findings/e_low/FINDING.md)",
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
    fn export_index_renders_subsystem_column() {
        // Two findings with subsystem set, one without — the column
        // should appear in the header, populated rows show the value,
        // and a missing/blank value renders as `—`.
        let dir = tmp_dir("export-index-subsystem");
        let write = |tag: &str, body: &str| {
            let d = dir.join("findings").join(tag);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("metadata.yaml"), body).unwrap();
        };
        write(
            "with_subsystem",
            "id: \"a\"\ntitle: \"named\"\nseverity: high\nstatus: active\nsubsystem: \"mm/folio\"\n",
        );
        write(
            "blank_subsystem",
            "id: \"b\"\ntitle: \"blank\"\nseverity: medium\nstatus: active\nsubsystem: \"\"\n",
        );
        write(
            "no_subsystem",
            "id: \"c\"\ntitle: \"missing\"\nseverity: low\nstatus: active\n",
        );
        let out = run_export_index(&dir).unwrap();
        let body = std::fs::read_to_string(&out).unwrap();
        assert!(
            body.contains("| Severity | Subsystem | Date | Status | ID | Title |"),
            "header missing Subsystem column: {body}"
        );
        assert!(
            body.contains("| high | mm/folio |"),
            "populated subsystem cell missing: {body}"
        );
        // Both blank and absent subsystem render as the em-dash
        // placeholder. Two rows × one em-dash each = 2 occurrences in
        // the subsystem column.  Each row also has a `—` for date,
        // so any cell-level grep is brittle; check the row prefix.
        assert!(
            body.contains("| medium | — |"),
            "blank subsystem should render as em-dash: {body}"
        );
        assert!(
            body.contains("| low | — |"),
            "missing subsystem should render as em-dash: {body}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn export_index_installs_index_script_and_preserves_local_edits() {
        // run_export_index installs findings-index.py into the export
        // dir on first call, then runs it with --generate. We don't
        // assert on the resulting INDEX.md/index.html (that depends
        // on python3 being on PATH), but we do assert:
        //   1. The script lands at <dir>/findings-index.py.
        //   2. Its body matches the bundled copy verbatim.
        //   3. A subsequent run does NOT overwrite an operator's
        //      hand-edited copy — the "if not already present" rule.
        let dir = tmp_dir("export-index-script-install");
        std::fs::create_dir_all(dir.join("findings/a_high")).unwrap();
        std::fs::write(
            dir.join("findings/a_high/metadata.yaml"),
            "id: \"a\"\ntitle: \"some bug\"\nseverity: high\nstatus: active\n",
        )
        .unwrap();
        run_export_index(&dir).unwrap();
        let script = dir.join("findings-index.py");
        assert!(
            script.exists(),
            "findings-index.py should be installed in {}",
            dir.display()
        );
        let body = std::fs::read_to_string(&script).unwrap();
        assert_eq!(
            body, INDEX_SCRIPT_BODY,
            "first install should match the bundled copy verbatim"
        );
        // Operator-edited script body must survive a second run.
        let edited = "#!/usr/bin/env python3\nprint('operator edit')\n";
        std::fs::write(&script, edited).unwrap();
        run_export_index(&dir).unwrap();
        let body_after = std::fs::read_to_string(&script).unwrap();
        assert_eq!(
            body_after, edited,
            "second run must not overwrite an operator-edited script"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn export_index_installs_readme_and_preserves_local_edits() {
        // run_export_index installs README.md alongside the script.
        // Same don't-overwrite contract: the bundled copy lands on
        // first run, an operator edit survives a re-run.
        let dir = tmp_dir("export-index-readme-install");
        std::fs::create_dir_all(dir.join("findings/a_high")).unwrap();
        std::fs::write(
            dir.join("findings/a_high/metadata.yaml"),
            "id: \"a\"\ntitle: \"some bug\"\nseverity: high\nstatus: active\n",
        )
        .unwrap();
        run_export_index(&dir).unwrap();
        let readme = dir.join("README.md");
        assert!(
            readme.exists(),
            "README.md should be installed in {}",
            dir.display()
        );
        let body = std::fs::read_to_string(&readme).unwrap();
        assert_eq!(
            body, EXPORT_README_BODY,
            "first install should match the bundled README verbatim"
        );
        // Operator-edited README must survive a second run.
        let edited = "# my custom export README\n";
        std::fs::write(&readme, edited).unwrap();
        run_export_index(&dir).unwrap();
        let body_after = std::fs::read_to_string(&readme).unwrap();
        assert_eq!(
            body_after, edited,
            "second run must not overwrite an operator-edited README"
        );
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
    fn embedded_template_emits_top_level_filename_and_blank_subsystem() {
        // Sample finding has a relevant_symbol at drivers/net/x.c —
        // the canonical top-level `filename:` should reflect that.
        // `subsystem:` is intentionally blank for now (later todo).
        let out = render(
            METADATA_TEMPLATE,
            &build_context(&finding_sample(), &git_sample()),
        );
        assert!(
            out.contains("filename: \"drivers/net/x.c\""),
            "missing filename: {out}"
        );
        assert!(
            out.contains("subsystem: \"\"\n"),
            "missing subsystem: {out}"
        );
    }

    #[test]
    fn primary_filename_falls_back_to_file_sections_then_empty() {
        // No symbols, but a file section: filename comes from there.
        let mut f = finding_sample();
        f.relevant_symbols.clear();
        assert_eq!(primary_filename(&f), "drivers/net/x.c");
        // Neither symbols nor sections: empty string.
        f.relevant_file_sections.clear();
        assert_eq!(primary_filename(&f), "");
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
