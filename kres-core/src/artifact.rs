use std::path::{Path, PathBuf};

/// A per-bug definition recorded in `metadata.yaml` under `bugs:`.
/// `id` is a stable short identifier joined against
/// `results[].bug` and `outcomes[].bug`; `description` is prose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindingBug {
    pub id: String,
    pub description: String,
}

/// A per-bug result entry recorded in `metadata.yaml` under `results:`.
/// `bug` is the stable id from `metadata.bugs[]` or
/// `research.fix_plan[].id`; for single-bug findings it may be empty.
/// `outcome` is `fixed | invalidated | deferred | unresolved`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindingResult {
    pub bug: String,
    pub outcome: String,
    pub evidence: String,
}

pub const AUTO_GENERATED_FIX_NAME: &str = "auto-generated-fix.diff";
pub const AUTO_GENERATED_FIX_LINK: &str = "[auto-generated-fix.diff](auto-generated-fix.diff)";
pub const INVALIDATION_NAME: &str = "invalidation.md";
pub const PARTIAL_INVALIDATION_NAME: &str = "partial-invalidation.md";
pub const SUMMARY_CROSS_LINK: &str = "[FINDING.md](FINDING.md) | [metadata.yaml](metadata.yaml)";

/// Prefix applied to `auto-generated-fix*.diff` filenames when a
/// previously-published fix is invalidated. The metadata.yaml block
/// name follows the same prefix scheme:
/// `auto_generated_fixes` → `invalidated_auto_generated_fixes`.
pub const INVALIDATED_FIX_PREFIX: &str = "invalidated-";
const ACTIVE_FIX_KEY: &str = "auto_generated_fixes";
const INVALIDATED_FIX_KEY: &str = "invalidated_auto_generated_fixes";

pub fn auto_generated_fix_name(index: u32) -> String {
    if index <= 1 {
        AUTO_GENERATED_FIX_NAME.to_string()
    } else {
        format!("auto-generated-fix-{index}.diff")
    }
}

pub fn auto_generated_fix_link(index: u32) -> String {
    let name = auto_generated_fix_name(index);
    format!("[{name}]({name})")
}

pub fn ensure_artifact_dir_files(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let id = artifact_id_for_dir(dir);
    let metadata = dir.join("metadata.yaml");
    if !metadata.exists() {
        std::fs::write(
            &metadata,
            format!("id: {}\nstatus: active\n", yaml_single_quote(&id)),
        )?;
    }
    let finding = dir.join("FINDING.md");
    if !finding.exists() {
        std::fs::write(&finding, format!("# {id}\n\n**Status:** active\n"))?;
    }
    let summary = dir.join("summary.md");
    if !summary.exists() {
        std::fs::write(&summary, "# Summary\n")?;
    }
    Ok(())
}

pub fn set_finding_status_files(finding_dir: &Path, status: &str) -> std::io::Result<Vec<PathBuf>> {
    if !matches!(status, "active" | "invalidated" | "unconfirmed") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("unsupported finding status: {status}"),
        ));
    }
    ensure_artifact_dir_files(finding_dir)?;
    let metadata = finding_dir.join("metadata.yaml");
    let finding = finding_dir.join("FINDING.md");

    let metadata_body = std::fs::read_to_string(&metadata)?;
    let mut saw_status = false;
    let mut metadata_lines: Vec<String> = metadata_body
        .lines()
        .map(|line| {
            if line.trim_start().starts_with("status:") {
                saw_status = true;
                let indent_len = line.len() - line.trim_start().len();
                format!("{}status: {status}", &line[..indent_len])
            } else {
                line.to_string()
            }
        })
        .collect();
    if !saw_status {
        metadata_lines.push(format!("status: {status}"));
    }
    std::fs::write(
        &metadata,
        finish_lines(metadata_lines, metadata_body.ends_with('\n')),
    )?;

    let finding_body = std::fs::read_to_string(&finding)?;
    let mut saw_status = false;
    let mut finding_lines: Vec<String> = finding_body
        .lines()
        .map(|line| {
            if line.starts_with("**Status:**") {
                saw_status = true;
                format!("**Status:** {status}")
            } else {
                line.to_string()
            }
        })
        .collect();
    if !saw_status {
        finding_lines.push(format!("**Status:** {status}"));
    }
    std::fs::write(
        &finding,
        finish_lines(finding_lines, finding_body.ends_with('\n')),
    )?;

    let mut updated = vec![metadata, finding];
    if let Some(summary) = update_summary_status(finding_dir, status)? {
        updated.push(summary);
    }
    Ok(updated)
}

/// Replace the value block under summary.md's `# Status` heading with
/// the new status (Title Case for prose), preserving the rest of the
/// file. Returns `Ok(None)` when summary.md is absent or has no
/// `# Status` heading — the function does not synthesize a heading
/// it didn't find, since summary.md is human-written prose.
fn update_summary_status(finding_dir: &Path, status: &str) -> std::io::Result<Option<PathBuf>> {
    let summary = finding_dir.join("summary.md");
    let body = match std::fs::read_to_string(&summary) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let pretty = match status {
        "active" => "Active",
        "invalidated" => "Invalidated",
        "unconfirmed" => "Unconfirmed",
        other => other,
    };
    let lines: Vec<&str> = body.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut replaced = false;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        out.push(line.to_string());
        if !replaced && line.trim() == "# Status" {
            i += 1;
            while i < lines.len() && lines[i].trim().is_empty() {
                out.push(lines[i].to_string());
                i += 1;
            }
            let mut consumed = false;
            while i < lines.len() && !lines[i].trim().is_empty() && !lines[i].starts_with('#') {
                if !consumed {
                    out.push(pretty.to_string());
                    consumed = true;
                    replaced = true;
                }
                i += 1;
            }
            if !consumed {
                out.push(pretty.to_string());
                replaced = true;
            }
            continue;
        }
        i += 1;
    }
    if !replaced {
        return Ok(None);
    }
    std::fs::write(&summary, finish_lines(out, body.ends_with('\n')))?;
    Ok(Some(summary))
}

pub fn write_invalidation_artifact(
    finding_dir: &Path,
    analysis: &str,
    invalid_evidence: &str,
) -> std::io::Result<PathBuf> {
    ensure_artifact_dir_files(finding_dir)?;
    let path = finding_dir.join(INVALIDATION_NAME);
    let body = format!(
        "# Invalidation\n\n\
         This finding was invalidated by kres research.\n\n\
         ## Reason\n\n\
         {}\n\n\
         ## Evidence\n\n\
         {}\n",
        markdown_text_or_placeholder(analysis, "No analysis was recorded."),
        markdown_text_or_placeholder(invalid_evidence, "No invalid evidence was recorded."),
    );
    std::fs::write(&path, body)?;
    Ok(path)
}

pub fn write_partial_invalidation_artifact(
    finding_dir: &Path,
    todo_id: &str,
    todo_title: &str,
    analysis: &str,
    invalid_evidence: &str,
) -> std::io::Result<PathBuf> {
    ensure_artifact_dir_files(finding_dir)?;
    let path = finding_dir.join(PARTIAL_INVALIDATION_NAME);
    let marker = format!(
        "<!-- kres-partial-invalidation:{} -->",
        html_comment_key(todo_id)
    );
    let section = format!(
        "{marker}\n\
         ## Invalidated Todo: {}\n\n\
         **Todo ID:** `{}`\n\n\
         **Todo title:** {}\n\n\
         ### Reason\n\n\
         {}\n\n\
         ### Evidence\n\n\
         {}\n",
        markdown_text_or_placeholder(todo_title, "Untitled todo."),
        markdown_text_or_placeholder(todo_id, "unknown"),
        markdown_text_or_placeholder(todo_title, "Untitled todo."),
        markdown_text_or_placeholder(analysis, "No analysis was recorded."),
        markdown_text_or_placeholder(invalid_evidence, "No invalid evidence was recorded."),
    );

    if path.exists() {
        let mut existing = std::fs::read_to_string(&path)?;
        if !existing.contains(&marker) {
            if !existing.ends_with('\n') {
                existing.push('\n');
            }
            existing.push('\n');
            existing.push_str(&section);
            std::fs::write(&path, existing)?;
        }
    } else {
        let body = format!(
            "# Partial Invalidation\n\n\
             One or more fix todos in this broader finding were invalidated. \
             This file records the invalidated part without marking the whole \
             finding invalid.\n\n\
             {section}"
        );
        std::fs::write(&path, body)?;
    }
    Ok(path)
}

pub fn record_auto_generated_fix(dir: &Path) -> std::io::Result<()> {
    record_auto_generated_fix_named(dir, AUTO_GENERATED_FIX_NAME)
}

pub fn record_auto_generated_fix_named(dir: &Path, fix_name: &str) -> std::io::Result<()> {
    validate_auto_generated_fix_name(fix_name)?;
    ensure_artifact_dir_files(dir)?;
    clear_invalidation_artifacts(dir)?;
    let cleared_invalidated = clear_invalidated_fix_state(dir)?;
    let metadata_path = dir.join("metadata.yaml");
    let metadata = std::fs::read_to_string(&metadata_path)?;
    let updated = metadata_with_auto_generated_fix(&metadata, fix_name);
    let updated = if cleared_invalidated {
        metadata_set_status(&updated, "active")
    } else {
        updated
    };
    if updated != metadata {
        std::fs::write(&metadata_path, updated)?;
    }
    if cleared_invalidated {
        let finding_path = dir.join("FINDING.md");
        if finding_path.exists() {
            let finding = std::fs::read_to_string(&finding_path)?;
            let updated = finding_set_status(&finding, "active");
            if updated != finding {
                std::fs::write(&finding_path, updated)?;
            }
        }
    }

    let summary_path = dir.join("summary.md");
    if summary_path.exists() {
        let original = std::fs::read_to_string(&summary_path)?;
        let summary = summary_drop_invalidated_fix_links(&original);
        let link = format!("[{fix_name}]({fix_name})");
        let updated = if !summary.contains(&link) {
            summary_with_auto_generated_fix_link(&summary, &link)
        } else {
            summary
        };
        if updated != original {
            std::fs::write(&summary_path, updated)?;
        }
    }
    Ok(())
}

/// Erase any leftover invalidated-fix state from a prior run:
/// remove `invalidated-auto-generated-fix*.diff` files on disk and
/// strip the `invalidated_auto_generated_fixes:` block from
/// metadata.yaml. Returns `true` when any such state was present
/// (caller uses this to also reset status from invalidated→active).
fn clear_invalidated_fix_state(dir: &Path) -> std::io::Result<bool> {
    let metadata_path = dir.join("metadata.yaml");
    if !metadata_path.exists() {
        return Ok(false);
    }
    let body = std::fs::read_to_string(&metadata_path)?;
    let names = extract_yaml_list_block(&body, INVALIDATED_FIX_KEY);
    let had_state = !names.is_empty();
    for name in &names {
        let path = dir.join(name);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    if had_state {
        let updated = metadata_rename_fix_block(&body, INVALIDATED_FIX_KEY, ACTIVE_FIX_KEY, &[]);
        if updated != body {
            std::fs::write(&metadata_path, updated)?;
        }
    }
    Ok(had_state)
}

/// Replace (or insert) the top-level `bugs:` block in
/// `metadata.yaml`. Each entry names one distinct bug the finding
/// describes; the bug-coverage review lens reads this list when
/// enumerating coverage. Callers pass the complete current list — the
/// existing block (if any) is replaced wholesale.
pub fn set_finding_bugs(finding_dir: &Path, bugs: &[FindingBug]) -> std::io::Result<Vec<PathBuf>> {
    ensure_artifact_dir_files(finding_dir)?;
    let metadata_path = finding_dir.join("metadata.yaml");
    let body = std::fs::read_to_string(&metadata_path)?;
    let updated = metadata_with_bugs(&body, bugs);
    if updated != body {
        std::fs::write(&metadata_path, updated)?;
    }
    Ok(vec![metadata_path])
}

/// Replace (or insert) the top-level `results:` block in
/// `metadata.yaml`. Each entry records what happened to one bug from
/// the finding's `bugs:` list — `outcome` is `fixed | invalidated |
/// deferred | unresolved` and `evidence` is a short prose pointer
/// (file:line, commit ref, linked todo id, etc.). The block is
/// written as a full replacement: callers pass the complete current
/// result set, not a delta.
pub fn set_finding_results(
    finding_dir: &Path,
    results: &[FindingResult],
) -> std::io::Result<Vec<PathBuf>> {
    ensure_artifact_dir_files(finding_dir)?;
    let metadata_path = finding_dir.join("metadata.yaml");
    let body = std::fs::read_to_string(&metadata_path)?;
    let updated = metadata_with_results(&body, results);
    if updated != body {
        std::fs::write(&metadata_path, updated)?;
    }
    Ok(vec![metadata_path])
}

/// Mark previously-published fixes as invalidated. For every entry
/// in metadata.yaml's `auto_generated_fixes:` block, rename the
/// on-disk `.diff` file to its `invalidated-` prefixed name, migrate
/// the metadata block to `invalidated_auto_generated_fixes:` with the
/// renamed values, and rewrite the patch links in `summary.md`. The
/// finding's `**Status:**` line is handled separately by
/// `set_finding_status_files`. No-op when no prior fix exists.
pub fn mark_fixes_invalidated(finding_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let metadata_path = finding_dir.join("metadata.yaml");
    if !metadata_path.exists() {
        return Ok(Vec::new());
    }
    let body = std::fs::read_to_string(&metadata_path)?;
    let names = extract_yaml_list_block(&body, ACTIVE_FIX_KEY);
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let pairs: Vec<(String, String)> = names
        .into_iter()
        .map(|n| {
            let renamed = format!("{INVALIDATED_FIX_PREFIX}{n}");
            (n, renamed)
        })
        .collect();
    let mut touched = Vec::new();
    for (old, new) in &pairs {
        let old_path = finding_dir.join(old);
        let new_path = finding_dir.join(new);
        if old_path.exists() {
            std::fs::rename(&old_path, &new_path)?;
            touched.push(new_path);
        }
    }
    let updated_metadata = metadata_rename_fix_block(
        &body,
        ACTIVE_FIX_KEY,
        INVALIDATED_FIX_KEY,
        &pairs.iter().map(|(_, n)| n.clone()).collect::<Vec<_>>(),
    );
    if updated_metadata != body {
        std::fs::write(&metadata_path, updated_metadata)?;
        touched.push(metadata_path);
    }
    let summary_path = finding_dir.join("summary.md");
    if summary_path.exists() {
        let summary = std::fs::read_to_string(&summary_path)?;
        let updated_summary = summary_rename_fix_links(&summary, &pairs);
        if updated_summary != summary {
            std::fs::write(&summary_path, updated_summary)?;
            touched.push(summary_path);
        }
    }
    Ok(touched)
}

pub fn clear_invalidation_artifacts(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut removed = Vec::new();
    for name in [INVALIDATION_NAME, PARTIAL_INVALIDATION_NAME] {
        let path = dir.join(name);
        match std::fs::remove_file(&path) {
            Ok(()) => removed.push(path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(removed)
}

pub fn patch_file_matches_head(dir: &Path, head_sha: &str) -> std::io::Result<bool> {
    patch_file_matches_head_named(dir, AUTO_GENERATED_FIX_NAME, head_sha)
}

pub fn patch_file_matches_head_named(
    dir: &Path,
    fix_name: &str,
    head_sha: &str,
) -> std::io::Result<bool> {
    validate_auto_generated_fix_name(fix_name)?;
    let patch = dir.join(fix_name);
    let existing = std::fs::read_to_string(patch)?;
    Ok(existing
        .lines()
        .next()
        .is_some_and(|l| l.starts_with(&format!("From {head_sha} "))))
}

fn finish_lines(lines: Vec<String>, trailing_newline: bool) -> String {
    let mut s = lines.join("\n");
    if trailing_newline {
        s.push('\n');
    }
    s
}

fn markdown_text_or_placeholder<'a>(value: &'a str, placeholder: &'static str) -> &'a str {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        placeholder
    } else {
        trimmed
    }
}

fn html_comment_key(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Collect list-item values from the top-level `key:` block in
/// `metadata`. Items are returned in source order. Single- and
/// double-quoted scalars are unquoted so callers receive the bare
/// filename whether the operator hand-authored `- 'foo'` or `- foo`.
fn extract_yaml_list_block(metadata: &str, key: &str) -> Vec<String> {
    let header = format!("{key}:");
    let mut items = Vec::<String>::new();
    let mut lines = metadata.lines().peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        let top_level = line == trimmed;
        if top_level && trimmed == header {
            while let Some(next) = lines.peek() {
                let nt = next.trim_start();
                let next_top_level = *next == nt;
                if let Some(value) = nt.strip_prefix("- ") {
                    items.push(unquote_yaml_scalar(value.trim()));
                    lines.next();
                } else if !next_top_level || nt.is_empty() {
                    lines.next();
                } else {
                    break;
                }
            }
        }
    }
    items
}

/// Strip surrounding `'…'` or `"…"` quoting from a YAML scalar.
/// Single-quoted YAML doubles embedded quotes (`''`); decode those.
/// Returns the input unchanged when no surrounding quote is present.
fn unquote_yaml_scalar(value: &str) -> String {
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        if bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\'' {
            return value[1..value.len() - 1].replace("''", "'");
        }
        if bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"' {
            return value[1..value.len() - 1].to_string();
        }
    }
    value.to_string()
}

/// Strip the top-level block at `from_key` (and any legacy single-line
/// `auto_generated_fix:` form) and append a fresh block at `to_key`
/// whose values are `new_names`. Used by both the invalidate path
/// (active → invalidated) and the publish-after-invalidation path
/// (invalidated → active or just-strip).
fn metadata_rename_fix_block(
    metadata: &str,
    from_key: &str,
    to_key: &str,
    new_names: &[String],
) -> String {
    let mut kept: Vec<&str> = Vec::new();
    let mut lines = metadata.lines().peekable();
    let from_header = format!("{from_key}:");
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        let top_level = line == trimmed;
        if top_level && trimmed.starts_with("auto_generated_fix:") && from_key == ACTIVE_FIX_KEY {
            // Drop legacy single-line form of the active key only;
            // there is no legacy single-line invalidated form.
            continue;
        }
        if top_level && trimmed == from_header {
            while let Some(next) = lines.peek() {
                let nt = next.trim_start();
                let next_top_level = *next == nt;
                if !next_top_level || nt.is_empty() || nt.starts_with('-') {
                    lines.next();
                } else {
                    break;
                }
            }
            continue;
        }
        kept.push(line);
    }
    let mut updated = kept.join("\n").trim_end().to_string();
    if new_names.is_empty() {
        if !updated.is_empty() {
            updated.push('\n');
        }
        return updated;
    }
    if !updated.is_empty() {
        updated.push('\n');
    }
    updated.push_str(to_key);
    updated.push_str(":\n");
    for name in new_names {
        updated.push_str("- ");
        updated.push_str(name);
        updated.push('\n');
    }
    updated
}

/// For each `(old, new)` pair, replace every `[old](old)` link
/// occurrence with `[new](new)` in `summary`. Idempotent if links are
/// already renamed.
fn summary_rename_fix_links(summary: &str, pairs: &[(String, String)]) -> String {
    let mut out = summary.to_string();
    for (old, new) in pairs {
        let old_link = format!("[{old}]({old})");
        let new_link = format!("[{new}]({new})");
        out = out.replace(&old_link, &new_link);
    }
    out
}

/// Strip `[invalidated-…](invalidated-…)` links from `summary.md`.
/// Used by the publish path to clean stale invalidated-fix references
/// before adding the fresh active link. Also collapses any orphan
/// ` | ` separators left by the removal.
fn summary_drop_invalidated_fix_links(summary: &str) -> String {
    let cleaned: Vec<String> = summary
        .lines()
        .map(|line| {
            let mut s = line.to_string();
            // Drop tokens like `[invalidated-auto-generated-fix*.diff](…)` plus
            // the adjacent ` | ` separator on either side. Iterate
            // until no more invalidated links are present so multiple
            // adjacent series entries collapse cleanly.
            while let Some(idx) = s.find("[invalidated-auto-generated-fix") {
                let Some(end) = s[idx..].find(')').map(|n| idx + n + 1) else {
                    break;
                };
                let mut start = idx;
                let mut stop = end;
                if s[..start].ends_with(" | ") {
                    start -= 3;
                } else if s[stop..].starts_with(" | ") {
                    stop += 3;
                }
                s.replace_range(start..stop, "");
            }
            s
        })
        .collect();
    let mut joined = cleaned.join("\n");
    if summary.ends_with('\n') && !joined.ends_with('\n') {
        joined.push('\n');
    }
    joined
}

/// Replace (or append) a single-line key in `text`. `match_prefix`
/// identifies the line to replace; `rendered` builds the new line
/// (taking the matched line's leading whitespace so YAML indentation
/// is preserved). When no line matches, the rendered line is
/// appended at the end of `text`. The original trailing-newline
/// shape is preserved.
fn set_single_line_field<F>(text: &str, match_prefix: &str, rendered: F) -> String
where
    F: Fn(&str) -> String,
{
    let mut saw = false;
    let lines: Vec<String> = text
        .lines()
        .map(|line| {
            if line.trim_start().starts_with(match_prefix) {
                saw = true;
                let indent_len = line.len() - line.trim_start().len();
                let indent = &line[..indent_len];
                rendered(indent)
            } else {
                line.to_string()
            }
        })
        .collect();
    let mut s = lines.join("\n");
    let trailing_newline = text.ends_with('\n');
    if !saw {
        if !s.is_empty() && !s.ends_with('\n') {
            s.push('\n');
        }
        s.push_str(&rendered(""));
    }
    if trailing_newline && !s.ends_with('\n') {
        s.push('\n');
    }
    s
}

fn metadata_set_status(metadata: &str, status: &str) -> String {
    set_single_line_field(metadata, "status:", |indent| {
        format!("{indent}status: {status}")
    })
}

fn finding_set_status(finding: &str, status: &str) -> String {
    set_single_line_field(finding, "**Status:**", |_indent| {
        format!("**Status:** {status}")
    })
}

fn metadata_with_auto_generated_fix(metadata: &str, fix_name: &str) -> String {
    let mut kept = Vec::new();
    let mut fixes = Vec::<String>::new();
    let mut lines = metadata.lines().peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        let top_level = line == trimmed;
        if top_level {
            if let Some(value) = trimmed.strip_prefix("auto_generated_fix:") {
                push_unique(&mut fixes, value.trim());
                continue;
            }
        }
        if top_level && trimmed == "auto_generated_fixes:" {
            while let Some(next) = lines.peek() {
                let next_trimmed = next.trim_start();
                if let Some(value) = next_trimmed.strip_prefix("- ") {
                    push_unique(&mut fixes, value.trim());
                    lines.next();
                } else if next_trimmed.is_empty() {
                    kept.push((*next).to_string());
                    lines.next();
                } else {
                    break;
                }
            }
            continue;
        }
        kept.push(line.to_string());
    }
    push_unique(&mut fixes, fix_name);

    let mut updated = kept.join("\n").trim_end().to_string();
    if !updated.is_empty() {
        updated.push('\n');
    }
    updated.push_str("auto_generated_fixes:\n");
    for fix in fixes {
        updated.push_str("- ");
        updated.push_str(&fix);
        updated.push('\n');
    }
    updated
}

/// Drop the top-level YAML sequence/mapping rooted at `key:` from
/// `metadata` (the `key:` line, every following list item, every
/// indented continuation, and blank lines inside the block). Return
/// the remaining text trim_end'd. The block ends at the next
/// top-level mapping key.
fn strip_top_level_block(metadata: &str, key: &str) -> String {
    let header = format!("{key}:");
    let mut kept: Vec<&str> = Vec::new();
    let mut lines = metadata.lines().peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        let top_level = line == trimmed;
        if top_level && trimmed == header {
            while let Some(next) = lines.peek() {
                let nt = next.trim_start();
                let next_top_level = *next == nt;
                let drop = !next_top_level || nt.is_empty() || nt.starts_with('-');
                if drop {
                    lines.next();
                } else {
                    break;
                }
            }
            continue;
        }
        kept.push(line);
    }
    kept.join("\n").trim_end().to_string()
}

/// Render `body` onto `base`, prefixing a newline when `base` is
/// non-empty so the appended block always starts on its own line. A
/// no-op when `body` is empty so callers can use this for the
/// drop-only path.
fn append_yaml_block(mut base: String, body: &str) -> String {
    if base.is_empty() && body.is_empty() {
        return base;
    }
    if !base.is_empty() {
        base.push('\n');
    }
    base.push_str(body);
    base
}

fn render_bugs_block(bugs: &[FindingBug]) -> String {
    if bugs.is_empty() {
        return String::new();
    }
    let mut s = String::from("bugs:\n");
    for b in bugs {
        s.push_str("- id: ");
        s.push_str(&yaml_single_quote(&b.id));
        s.push('\n');
        s.push_str("  description: ");
        s.push_str(&yaml_single_quote(&b.description));
        s.push('\n');
    }
    s
}

fn render_results_block(results: &[FindingResult]) -> String {
    if results.is_empty() {
        return String::new();
    }
    let mut s = String::from("results:\n");
    for r in results {
        s.push_str("- bug: ");
        s.push_str(&yaml_single_quote(&r.bug));
        s.push('\n');
        s.push_str("  outcome: ");
        s.push_str(&yaml_single_quote(&r.outcome));
        s.push('\n');
        s.push_str("  evidence: ");
        s.push_str(&yaml_single_quote(&r.evidence));
        s.push('\n');
    }
    s
}

fn metadata_with_bugs(metadata: &str, bugs: &[FindingBug]) -> String {
    append_yaml_block(
        strip_top_level_block(metadata, "bugs"),
        &render_bugs_block(bugs),
    )
}

fn metadata_with_results(metadata: &str, results: &[FindingResult]) -> String {
    append_yaml_block(
        strip_top_level_block(metadata, "results"),
        &render_results_block(results),
    )
}

fn validate_auto_generated_fix_name(fix_name: &str) -> std::io::Result<()> {
    let valid = fix_name == AUTO_GENERATED_FIX_NAME
        || fix_name
            .strip_prefix("auto-generated-fix-")
            .and_then(|suffix| suffix.strip_suffix(".diff"))
            .and_then(|n| n.parse::<u32>().ok())
            .is_some_and(|index| index >= 2);
    if valid {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid auto-generated fix filename: {fix_name}"),
        ))
    }
}

fn summary_with_auto_generated_fix_link(summary: &str, link: &str) -> String {
    let mut lines: Vec<String> = summary.lines().map(str::to_string).collect();
    if let Some(line) = lines
        .iter_mut()
        .find(|line| line.contains(SUMMARY_CROSS_LINK))
    {
        line.push_str(" | ");
        line.push_str(link);
        let mut updated = lines.join("\n");
        if summary.ends_with('\n') {
            updated.push('\n');
        }
        return updated;
    }
    format!("{link}\n\n{summary}")
}

fn push_unique(items: &mut Vec<String>, value: &str) {
    if value.is_empty() || items.iter().any(|item| item == value) {
        return;
    }
    items.push(value.to_string());
}

fn artifact_id_for_dir(dir: &Path) -> String {
    dir.file_name()
        .and_then(|n| n.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("kres-result")
        .to_string()
}

/// Single-quote-encode a YAML scalar onto a single line. YAML
/// single-quoted strings permit line breaks only at folded
/// continuation columns; embedded `\r` / `\n` / `\t` would either
/// fold into spaces (best case) or produce an invalid document (worst
/// case) when the continuation column is wrong. Replace those
/// whitespace characters with single spaces, collapse runs, trim, and
/// then escape embedded single quotes. Every value written into
/// metadata.yaml goes through this so agent-emitted multi-line
/// strings can't corrupt the file.
fn yaml_single_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut prev_space = false;
    for ch in value.chars() {
        let mapped = match ch {
            '\r' | '\n' | '\t' => ' ',
            other => other,
        };
        if mapped == ' ' {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(mapped);
            prev_space = false;
        }
    }
    format!("'{}'", out.trim().replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_artifact_dir_files_quotes_generated_metadata_id() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("fix: psp's bug #1");

        ensure_artifact_dir_files(&dir).unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.join("metadata.yaml")).unwrap(),
            "id: 'fix: psp''s bug #1'\nstatus: active\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("FINDING.md")).unwrap(),
            "# fix: psp's bug #1\n\n**Status:** active\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("summary.md")).unwrap(),
            "# Summary\n"
        );
    }

    #[test]
    fn set_finding_status_updates_status_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("metadata.yaml"), "id: F1\nstatus: active\n").unwrap();
        std::fs::write(
            dir.join("FINDING.md"),
            "# Finding\n\n**Status:** active\n\nbody\n",
        )
        .unwrap();

        let files = set_finding_status_files(dir, "unconfirmed").unwrap();

        assert_eq!(files.len(), 2);
        assert_eq!(
            std::fs::read_to_string(dir.join("metadata.yaml")).unwrap(),
            "id: F1\nstatus: unconfirmed\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("FINDING.md")).unwrap(),
            "# Finding\n\n**Status:** unconfirmed\n\nbody\n"
        );
    }

    #[test]
    fn set_finding_status_updates_summary_when_status_section_present() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("metadata.yaml"), "id: F1\nstatus: active\n").unwrap();
        std::fs::write(dir.join("FINDING.md"), "# F1\n\n**Status:** active\n").unwrap();
        std::fs::write(
            dir.join("summary.md"),
            "[FINDING.md](FINDING.md) | [metadata.yaml](metadata.yaml)\n\n\
             # Subject: x\n\n\
             # Status\n\n\
             Plausible\n\n\
             # Impact\n\n\
             body\n",
        )
        .unwrap();

        let files = set_finding_status_files(dir, "invalidated").unwrap();

        assert_eq!(files.len(), 3);
        assert!(files.iter().any(|p| p.ends_with("summary.md")));
        let summary = std::fs::read_to_string(dir.join("summary.md")).unwrap();
        assert!(
            summary.contains("# Status\n\nInvalidated\n"),
            "summary status block not rewritten: {summary}"
        );
        assert!(
            summary.contains("# Impact\n\nbody"),
            "summary body must be preserved: {summary}"
        );
        assert!(
            !summary.contains("Plausible"),
            "old status value must be replaced: {summary}"
        );
    }

    #[test]
    fn set_finding_status_skips_summary_when_no_status_section() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("metadata.yaml"), "id: F1\nstatus: active\n").unwrap();
        std::fs::write(dir.join("FINDING.md"), "# F1\n\n**Status:** active\n").unwrap();
        let summary_body = "# Subject: x\n\nbody without status section\n";
        std::fs::write(dir.join("summary.md"), summary_body).unwrap();

        let files = set_finding_status_files(dir, "invalidated").unwrap();

        assert_eq!(files.len(), 2);
        assert!(!files.iter().any(|p| p.ends_with("summary.md")));
        assert_eq!(
            std::fs::read_to_string(dir.join("summary.md")).unwrap(),
            summary_body
        );
    }

    #[test]
    fn invalidation_artifacts_record_evidence() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("metadata.yaml"), "id: F1\nstatus: active\n").unwrap();
        std::fs::write(dir.join("FINDING.md"), "# Finding\n").unwrap();

        let full = write_invalidation_artifact(
            dir,
            "The alleged dereference is guarded.",
            "net/foo.c:42 checks ptr before use.",
        )
        .unwrap();
        assert_eq!(full.file_name().unwrap(), INVALIDATION_NAME);
        let full_body = std::fs::read_to_string(dir.join(INVALIDATION_NAME)).unwrap();
        assert!(full_body.contains("The alleged dereference is guarded."));
        assert!(full_body.contains("net/foo.c:42 checks ptr before use."));

        let partial = write_partial_invalidation_artifact(
            dir,
            "fix-b",
            "drop invalid sibling fix",
            "The sibling claim is false.",
            "net/bar.c:7 already rejects it.",
        )
        .unwrap();
        write_partial_invalidation_artifact(
            dir,
            "fix-b",
            "drop invalid sibling fix",
            "The sibling claim is false.",
            "net/bar.c:7 already rejects it.",
        )
        .unwrap();
        assert_eq!(partial.file_name().unwrap(), PARTIAL_INVALIDATION_NAME);
        let partial_body = std::fs::read_to_string(dir.join(PARTIAL_INVALIDATION_NAME)).unwrap();
        assert!(partial_body.contains("Invalidated Todo: drop invalid sibling fix"));
        assert!(partial_body.contains("net/bar.c:7 already rejects it."));
        assert_eq!(
            partial_body
                .matches("kres-partial-invalidation:fix-b")
                .count(),
            1
        );
    }

    #[test]
    fn record_auto_generated_fix_updates_metadata_and_summary() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("metadata.yaml"), "id: F1\nstatus: active\n").unwrap();
        std::fs::write(dir.join("FINDING.md"), "# F1\n").unwrap();
        std::fs::write(
            dir.join("summary.md"),
            "[FINDING.md](FINDING.md) | [metadata.yaml](metadata.yaml)\n\nbody\n",
        )
        .unwrap();

        record_auto_generated_fix(dir).unwrap();
        record_auto_generated_fix(dir).unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.join("metadata.yaml")).unwrap(),
            "id: F1\nstatus: active\nauto_generated_fixes:\n- auto-generated-fix.diff\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("summary.md"))
                .unwrap()
                .matches(AUTO_GENERATED_FIX_LINK)
                .count(),
            1
        );
    }

    #[test]
    fn record_auto_generated_fix_clears_stale_invalidation_artifacts() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("metadata.yaml"), "id: F1\nstatus: active\n").unwrap();
        std::fs::write(dir.join("FINDING.md"), "# F1\n").unwrap();
        std::fs::write(dir.join("summary.md"), "# Summary\n").unwrap();
        std::fs::write(dir.join(INVALIDATION_NAME), "stale invalidation").unwrap();
        std::fs::write(dir.join(PARTIAL_INVALIDATION_NAME), "stale partial").unwrap();

        record_auto_generated_fix(dir).unwrap();

        assert!(!dir.join(INVALIDATION_NAME).exists());
        assert!(!dir.join(PARTIAL_INVALIDATION_NAME).exists());
        assert!(std::fs::read_to_string(dir.join("metadata.yaml"))
            .unwrap()
            .contains("auto_generated_fixes:"));
    }

    #[test]
    fn record_auto_generated_fix_named_appends_series_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("metadata.yaml"), "id: F1\nstatus: active\n").unwrap();
        std::fs::write(dir.join("FINDING.md"), "# F1\n").unwrap();
        std::fs::write(
            dir.join("summary.md"),
            "[FINDING.md](FINDING.md) | [metadata.yaml](metadata.yaml)\n\nbody\n",
        )
        .unwrap();

        record_auto_generated_fix_named(dir, "auto-generated-fix.diff").unwrap();
        record_auto_generated_fix_named(dir, "auto-generated-fix-2.diff").unwrap();
        record_auto_generated_fix_named(dir, "auto-generated-fix-2.diff").unwrap();

        let metadata = std::fs::read_to_string(dir.join("metadata.yaml")).unwrap();
        assert!(metadata.contains("auto_generated_fixes:\n"));
        assert!(metadata.contains("- auto-generated-fix.diff\n"));
        assert!(metadata.contains("- auto-generated-fix-2.diff\n"));
        assert_eq!(metadata.matches("- auto-generated-fix-2.diff").count(), 1);
        let summary = std::fs::read_to_string(dir.join("summary.md")).unwrap();
        assert!(summary.contains("[auto-generated-fix.diff](auto-generated-fix.diff)"));
        assert!(summary.contains("[auto-generated-fix-2.diff](auto-generated-fix-2.diff)"));
        assert!(summary.contains(
            "[metadata.yaml](metadata.yaml) | [auto-generated-fix.diff](auto-generated-fix.diff) | [auto-generated-fix-2.diff](auto-generated-fix-2.diff)"
        ));
    }

    #[test]
    fn record_auto_generated_fix_named_rejects_unexpected_names() {
        let tmp = tempfile::tempdir().unwrap();
        let err = record_auto_generated_fix_named(tmp.path(), "../outside.diff").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn metadata_update_only_rewrites_top_level_fix_keys() {
        let metadata = "\
id: F1
nested:
  auto_generated_fix: keep-me.diff
  auto_generated_fixes:
    - keep-me-2.diff
auto_generated_fixes:
  - auto-generated-fix.diff
status: active
";

        let updated = metadata_with_auto_generated_fix(metadata, "auto-generated-fix-2.diff");

        assert!(updated.contains("  auto_generated_fix: keep-me.diff\n"));
        assert!(updated.contains("  auto_generated_fixes:\n"));
        assert!(updated.contains("    - keep-me-2.diff\n"));
        assert!(updated.contains("status: active\n"));
        assert!(updated.contains("auto_generated_fixes:\n"));
        assert!(updated.contains("- auto-generated-fix.diff\n"));
        assert!(updated.contains("- auto-generated-fix-2.diff\n"));
    }

    #[test]
    fn set_finding_results_writes_block() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("metadata.yaml"), "id: F1\nstatus: active\n").unwrap();
        std::fs::write(dir.join("FINDING.md"), "# F1\n").unwrap();
        std::fs::write(dir.join("summary.md"), "# Summary\n").unwrap();

        let results = vec![
            FindingResult {
                bug: "race".into(),
                outcome: "fixed".into(),
                evidence: "psp_main.c:415".into(),
            },
            FindingResult {
                bug: "refcount".into(),
                outcome: "invalidated".into(),
                evidence: "RCU grace covers psp_main.c:380".into(),
            },
        ];
        set_finding_results(dir, &results).unwrap();
        let updated = std::fs::read_to_string(dir.join("metadata.yaml")).unwrap();
        assert!(updated.contains("results:\n"));
        assert!(updated.contains("- bug: 'race'\n"));
        assert!(updated.contains("  outcome: 'fixed'\n"));
        assert!(updated.contains("  evidence: 'psp_main.c:415'\n"));
        assert!(updated.contains("- bug: 'refcount'\n"));
        assert!(updated.contains("  outcome: 'invalidated'\n"));
        // Original keys preserved.
        assert!(updated.contains("id: F1\n"));
        assert!(updated.contains("status: active\n"));
    }

    #[test]
    fn set_finding_results_replaces_existing_block() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(
            dir.join("metadata.yaml"),
            concat!(
                "id: F1\n",
                "status: active\n",
                "results:\n",
                "- bug: 'old'\n",
                "  outcome: 'unresolved'\n",
                "  evidence: 'stale entry'\n",
            ),
        )
        .unwrap();
        std::fs::write(dir.join("FINDING.md"), "# F1\n").unwrap();
        std::fs::write(dir.join("summary.md"), "# Summary\n").unwrap();

        let results = vec![FindingResult {
            bug: "race".into(),
            outcome: "fixed".into(),
            evidence: "commit abc1234".into(),
        }];
        set_finding_results(dir, &results).unwrap();
        let updated = std::fs::read_to_string(dir.join("metadata.yaml")).unwrap();
        assert!(updated.contains("- bug: 'race'\n"));
        assert!(!updated.contains("'old'"));
        assert!(!updated.contains("stale entry"));
        assert!(updated.contains("status: active\n"));
    }

    #[test]
    fn set_finding_results_empty_drops_block() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(
            dir.join("metadata.yaml"),
            concat!(
                "id: F1\n",
                "status: active\n",
                "results:\n",
                "- bug: 'race'\n",
                "  outcome: 'fixed'\n",
                "  evidence: 'x'\n",
            ),
        )
        .unwrap();
        std::fs::write(dir.join("FINDING.md"), "# F1\n").unwrap();
        std::fs::write(dir.join("summary.md"), "# Summary\n").unwrap();

        set_finding_results(dir, &[]).unwrap();
        let updated = std::fs::read_to_string(dir.join("metadata.yaml")).unwrap();
        assert!(!updated.contains("results:"));
        assert!(updated.contains("status: active\n"));
    }

    #[test]
    fn set_finding_bugs_writes_block() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("metadata.yaml"), "id: F1\nstatus: active\n").unwrap();
        std::fs::write(dir.join("FINDING.md"), "# F1\n").unwrap();
        std::fs::write(dir.join("summary.md"), "# Summary\n").unwrap();

        let bugs = vec![
            FindingBug {
                id: "race".into(),
                description: "psp_dev_unregister races with doit".into(),
            },
            FindingBug {
                id: "refcount".into(),
                description: "missing put on error path".into(),
            },
        ];
        set_finding_bugs(dir, &bugs).unwrap();
        let updated = std::fs::read_to_string(dir.join("metadata.yaml")).unwrap();
        assert!(updated.contains("bugs:\n"));
        assert!(updated.contains("- id: 'race'\n"));
        assert!(updated.contains("  description: 'psp_dev_unregister races with doit'\n"));
        assert!(updated.contains("- id: 'refcount'\n"));
        assert!(updated.contains("id: F1\n"));
        assert!(updated.contains("status: active\n"));
    }

    #[test]
    fn set_finding_bugs_replaces_existing_block() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(
            dir.join("metadata.yaml"),
            concat!(
                "id: F1\n",
                "status: active\n",
                "bugs:\n",
                "- id: 'stale'\n",
                "  description: 'old entry'\n",
            ),
        )
        .unwrap();
        std::fs::write(dir.join("FINDING.md"), "# F1\n").unwrap();
        std::fs::write(dir.join("summary.md"), "# Summary\n").unwrap();

        let bugs = vec![FindingBug {
            id: "fresh".into(),
            description: "new entry".into(),
        }];
        set_finding_bugs(dir, &bugs).unwrap();
        let updated = std::fs::read_to_string(dir.join("metadata.yaml")).unwrap();
        assert!(updated.contains("- id: 'fresh'\n"));
        assert!(!updated.contains("'stale'"));
        assert!(!updated.contains("old entry"));
    }

    #[test]
    fn set_finding_bugs_and_results_coexist() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("metadata.yaml"), "id: F1\nstatus: active\n").unwrap();
        std::fs::write(dir.join("FINDING.md"), "# F1\n").unwrap();
        std::fs::write(dir.join("summary.md"), "# Summary\n").unwrap();

        set_finding_bugs(
            dir,
            &[FindingBug {
                id: "race".into(),
                description: "the race".into(),
            }],
        )
        .unwrap();
        set_finding_results(
            dir,
            &[FindingResult {
                bug: "race".into(),
                outcome: "fixed".into(),
                evidence: "psp_main.c:415".into(),
            }],
        )
        .unwrap();
        let updated = std::fs::read_to_string(dir.join("metadata.yaml")).unwrap();
        assert!(updated.contains("bugs:\n"));
        assert!(updated.contains("- id: 'race'\n"));
        assert!(updated.contains("results:\n"));
        assert!(updated.contains("- bug: 'race'\n"));
        assert!(updated.contains("  outcome: 'fixed'\n"));
    }

    #[test]
    fn set_finding_results_sanitizes_newlines() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("metadata.yaml"), "id: F1\nstatus: active\n").unwrap();
        std::fs::write(dir.join("FINDING.md"), "# F1\n").unwrap();
        std::fs::write(dir.join("summary.md"), "# Summary\n").unwrap();

        let results = vec![FindingResult {
            bug: "race".into(),
            outcome: "fixed".into(),
            // Multi-line evidence with CRLFs, tabs, and trailing
            // whitespace — the agent can produce any of these.
            evidence: "psp_main.c:415\nalso\r\nsee commit\tabc1234   ".into(),
        }];
        set_finding_results(dir, &results).unwrap();
        let updated = std::fs::read_to_string(dir.join("metadata.yaml")).unwrap();
        // Locate the evidence: line and verify it's single-line:
        // single-quoted, no embedded LF/CR/HT/control whitespace,
        // and the closing quote sits on the same line as the key.
        let evidence_line = updated
            .lines()
            .find(|l| l.trim_start().starts_with("evidence:"))
            .expect("evidence line written");
        let value = evidence_line.split_once(':').unwrap().1.trim();
        assert!(value.starts_with('\''));
        assert!(value.ends_with('\''));
        let inner = &value[1..value.len() - 1].replace("''", "'");
        assert!(!inner.contains('\n'));
        assert!(!inner.contains('\r'));
        assert!(!inner.contains('\t'));
        // Adjacent whitespace folded to single space, trimmed.
        assert_eq!(inner, inner.trim());
        assert!(!inner.contains("  "));
    }

    #[test]
    fn set_finding_bugs_sanitizes_newlines() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("metadata.yaml"), "id: F1\nstatus: active\n").unwrap();
        std::fs::write(dir.join("FINDING.md"), "# F1\n").unwrap();
        std::fs::write(dir.join("summary.md"), "# Summary\n").unwrap();

        let bugs = vec![FindingBug {
            id: "race".into(),
            description: "first line\n  second line\n  third line".into(),
        }];
        set_finding_bugs(dir, &bugs).unwrap();
        let updated = std::fs::read_to_string(dir.join("metadata.yaml")).unwrap();
        let description_line = updated
            .lines()
            .find(|l| l.trim_start().starts_with("description:"))
            .expect("description line written");
        let value = description_line.split_once(':').unwrap().1.trim();
        assert!(value.starts_with('\''));
        assert!(value.ends_with('\''));
        let inner = &value[1..value.len() - 1];
        assert!(!inner.contains('\n'));
        assert!(!inner.contains('\r'));
    }

    #[test]
    fn mark_fixes_invalidated_renames_diff_metadata_and_summary() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("metadata.yaml"), "id: F1\nstatus: active\n").unwrap();
        std::fs::write(dir.join("FINDING.md"), "# F1\n\n**Status:** active\n").unwrap();
        std::fs::write(
            dir.join("summary.md"),
            "[FINDING.md](FINDING.md) | [metadata.yaml](metadata.yaml)\n\nbody\n",
        )
        .unwrap();
        // Two prior fixes (series) on disk.
        record_auto_generated_fix_named(dir, "auto-generated-fix.diff").unwrap();
        std::fs::write(dir.join("auto-generated-fix.diff"), "patch1").unwrap();
        record_auto_generated_fix_named(dir, "auto-generated-fix-2.diff").unwrap();
        std::fs::write(dir.join("auto-generated-fix-2.diff"), "patch2").unwrap();

        let touched = mark_fixes_invalidated(dir).unwrap();
        assert!(!touched.is_empty());

        // Renamed .diff files on disk.
        assert!(!dir.join("auto-generated-fix.diff").exists());
        assert!(!dir.join("auto-generated-fix-2.diff").exists());
        assert!(dir.join("invalidated-auto-generated-fix.diff").exists());
        assert!(dir.join("invalidated-auto-generated-fix-2.diff").exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("invalidated-auto-generated-fix.diff")).unwrap(),
            "patch1"
        );

        // metadata.yaml: active block gone, invalidated block present
        // with renamed entries; original keys preserved.
        let m = std::fs::read_to_string(dir.join("metadata.yaml")).unwrap();
        // The active block key starts at column 0 with a newline (or
        // the start of the file) before it. Match anchored.
        assert!(!m.contains("\nauto_generated_fixes:"));
        assert!(!m.starts_with("auto_generated_fixes:"));
        assert!(m.contains("invalidated_auto_generated_fixes:\n"));
        assert!(m.contains("- invalidated-auto-generated-fix.diff\n"));
        assert!(m.contains("- invalidated-auto-generated-fix-2.diff\n"));
        assert!(m.contains("id: F1\n"));

        // summary.md: links updated.
        let s = std::fs::read_to_string(dir.join("summary.md")).unwrap();
        assert!(!s.contains("[auto-generated-fix.diff](auto-generated-fix.diff)"));
        assert!(s.contains(
            "[invalidated-auto-generated-fix.diff](invalidated-auto-generated-fix.diff)"
        ));
    }

    #[test]
    fn record_auto_generated_fix_after_invalidation_restores_active_state() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("metadata.yaml"), "id: F1\nstatus: active\n").unwrap();
        std::fs::write(dir.join("FINDING.md"), "# F1\n\n**Status:** active\n").unwrap();
        std::fs::write(
            dir.join("summary.md"),
            "[FINDING.md](FINDING.md) | [metadata.yaml](metadata.yaml)\n\nbody\n",
        )
        .unwrap();

        // Round 1: publish a fix, then invalidate it.
        std::fs::write(dir.join("auto-generated-fix.diff"), "patch1").unwrap();
        record_auto_generated_fix_named(dir, "auto-generated-fix.diff").unwrap();
        set_finding_status_files(dir, "invalidated").unwrap();
        mark_fixes_invalidated(dir).unwrap();
        assert!(dir.join("invalidated-auto-generated-fix.diff").exists());

        // Round 2: publish a new fix. Active state is restored.
        std::fs::write(dir.join("auto-generated-fix.diff"), "patch2").unwrap();
        record_auto_generated_fix_named(dir, "auto-generated-fix.diff").unwrap();

        // Stale invalidated .diff is gone.
        assert!(!dir.join("invalidated-auto-generated-fix.diff").exists());
        // New fix exists.
        assert!(dir.join("auto-generated-fix.diff").exists());

        // metadata: invalidated block removed, active block present, status active.
        let m = std::fs::read_to_string(dir.join("metadata.yaml")).unwrap();
        assert!(!m.contains("invalidated_auto_generated_fixes:"));
        assert!(m.contains("\nauto_generated_fixes:") || m.starts_with("auto_generated_fixes:"));
        assert!(m.contains("- auto-generated-fix.diff\n"));
        assert!(m.contains("status: active\n"));

        // FINDING.md: status reset.
        let f = std::fs::read_to_string(dir.join("FINDING.md")).unwrap();
        assert!(f.contains("**Status:** active"));
        assert!(!f.contains("**Status:** invalidated"));

        // summary.md: no invalidated-fix link, active link present.
        let s = std::fs::read_to_string(dir.join("summary.md")).unwrap();
        assert!(!s.contains("invalidated-auto-generated-fix.diff"));
        assert!(s.contains("[auto-generated-fix.diff](auto-generated-fix.diff)"));
    }

    #[test]
    fn mark_fixes_invalidated_handles_quoted_filenames() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(
            dir.join("metadata.yaml"),
            concat!(
                "id: F1\n",
                "status: active\n",
                "auto_generated_fixes:\n",
                "- 'auto-generated-fix.diff'\n",
            ),
        )
        .unwrap();
        std::fs::write(dir.join("FINDING.md"), "# F1\n\n**Status:** active\n").unwrap();
        std::fs::write(
            dir.join("summary.md"),
            "[FINDING.md](FINDING.md) | [metadata.yaml](metadata.yaml)\n",
        )
        .unwrap();
        std::fs::write(dir.join("auto-generated-fix.diff"), "patch1").unwrap();

        mark_fixes_invalidated(dir).unwrap();

        // Bare filename used for the rename, not the quoted form.
        assert!(!dir.join("auto-generated-fix.diff").exists());
        assert!(dir.join("invalidated-auto-generated-fix.diff").exists());
        // Metadata block uses bare names too.
        let m = std::fs::read_to_string(dir.join("metadata.yaml")).unwrap();
        assert!(m.contains("- invalidated-auto-generated-fix.diff\n"));
        assert!(!m.contains("'auto-generated-fix.diff'"));
    }

    #[test]
    fn mark_fixes_invalidated_is_noop_without_prior_fix() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("metadata.yaml"), "id: F1\nstatus: active\n").unwrap();
        std::fs::write(dir.join("FINDING.md"), "# F1\n").unwrap();
        std::fs::write(dir.join("summary.md"), "# Summary\n").unwrap();

        let touched = mark_fixes_invalidated(dir).unwrap();
        assert!(touched.is_empty());
        // Nothing changed.
        assert_eq!(
            std::fs::read_to_string(dir.join("metadata.yaml")).unwrap(),
            "id: F1\nstatus: active\n"
        );
    }

    #[test]
    fn set_finding_results_quotes_special_chars() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("metadata.yaml"), "id: F1\nstatus: active\n").unwrap();
        std::fs::write(dir.join("FINDING.md"), "# F1\n").unwrap();
        std::fs::write(dir.join("summary.md"), "# Summary\n").unwrap();

        let results = vec![FindingResult {
            bug: "user's-bug".into(),
            outcome: "fixed".into(),
            evidence: "see commit 'abc' and file:line".into(),
        }];
        set_finding_results(dir, &results).unwrap();
        let updated = std::fs::read_to_string(dir.join("metadata.yaml")).unwrap();
        // Single quotes inside values are doubled per YAML.
        assert!(updated.contains("- bug: 'user''s-bug'\n"));
        assert!(updated.contains("  evidence: 'see commit ''abc'' and file:line'\n"));
    }
}
