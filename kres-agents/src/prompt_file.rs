//! Prompt-file parsing.
//!
//! A `--prompt <file>` argument points at markdown that can carry:
//!
//! - Plain prose (the `original_prompt` / preamble).
//! - Markdown todo bullets of the form
//!   `- [ ] **[<kind>]** <name> — <reason> (after: a, b) `#id``. These
//!   become session-wide slow-agent lenses (NOT separate tasks).
//! - Indented sub-bullets immediately following a todo bullet are
//!   folded into that item's `reason` (joined with `"; "`).
//! - Lines beginning with `- [x]` are treated as *done* and skipped.
//!
//! Port of

use kres_core::lens::LensSpec;

#[derive(Debug, Clone, Default)]
pub struct PromptFile {
    /// Prose portion, with lens-definition lines stripped.
    pub prompt: String,
    /// Session-wide lenses parsed out of the markdown todo bullets.
    pub lenses: Vec<LensSpec>,
}

pub fn parse(raw: &str) -> PromptFile {
    let mut prose_lines: Vec<&str> = Vec::new();
    // `current` is populated by a matched todo bullet and flushed
    // either on the next non-indented non-todo line or when a new
    // bullet starts.
    let mut current: Option<ParsedBullet> = None;
    let mut lenses: Vec<LensSpec> = Vec::new();

    for line in raw.lines() {
        let stripped = line.trim();

        // Markdown-todo bullet: `- [x] **[kind]** body` or the
        // non-bold variant `- [x] [kind] body`.
        if let Some(b) = parse_todo_bullet(stripped) {
            if let Some(prev) = current.take() {
                push_lens(&mut lenses, prev);
            }
            if !b.done {
                current = Some(b);
            }
            continue;
        }

        // Indented continuation of the current bullet → extend reason.
        if let Some(ref mut cur) = current {
            if !line.is_empty()
                && matches!(line.chars().next(), Some(' ') | Some('\t'))
                && !stripped.is_empty()
            {
                let mut extra: &str = stripped;
                if let Some(rest) = extra
                    .strip_prefix("- ")
                    .or_else(|| extra.strip_prefix("* "))
                    .or_else(|| extra.strip_prefix("+ "))
                {
                    extra = rest.trim();
                }
                if !extra.is_empty() {
                    if cur.reason.is_empty() {
                        cur.reason = extra.to_string();
                    } else {
                        cur.reason.push_str("; ");
                        cur.reason.push_str(extra);
                    }
                }
                continue;
            }
        }

        // Everything else: flush the current bullet if any and
        // contribute the line to the preamble.
        if let Some(prev) = current.take() {
            push_lens(&mut lenses, prev);
        }
        prose_lines.push(line);
    }
    if let Some(prev) = current.take() {
        push_lens(&mut lenses, prev);
    }
    let prompt = prose_lines.join("\n").trim().to_string();
    PromptFile { prompt, lenses }
}

#[derive(Debug, Clone)]
struct ParsedBullet {
    done: bool,
    kind: String,
    name: String,
    reason: String,
    id: Option<String>,
    depends_on: Vec<String>,
}

fn push_lens(lenses: &mut Vec<LensSpec>, b: ParsedBullet) {
    if b.done || b.name.is_empty() {
        return;
    }
    let id =
        b.id.unwrap_or_else(|| slug(&format!("{}-{}", b.kind, b.name)));
    lenses.push(LensSpec {
        id,
        kind: b.kind,
        name: b.name,
        reason: b.reason,
    });
    let _ = b.depends_on; // Lenses don't carry depends_on; the field
                          // is preserved for callers that use the
                          // parser to load todos elsewhere.
}

/// Match a full markdown-todo bullet. Returns `ParsedBullet` with
/// populated fields, or None for any other shape.
fn parse_todo_bullet(line: &str) -> Option<ParsedBullet> {
    // `- [x] **[kind]** rest` or `- [x] [kind] rest`
    let rest = line.strip_prefix("- [")?;
    let (status, after_check) = rest.split_once("] ")?;
    if status.len() != 1 {
        return None;
    }
    let done = status == "x" || status == "X";
    let (kind, body) = extract_kind(after_check.trim_start())?;
    let body = body.trim();
    let (body, id) = extract_id(body);
    let (body, depends_on) = extract_depends(body.trim());
    // Reason is everything after an em-dash (or `-- ` for ascii fallback).
    let (name, reason) = split_name_reason(body.trim());
    Some(ParsedBullet {
        done,
        kind: kind.to_string(),
        name: name.trim().to_string(),
        reason: reason.trim().to_string(),
        id,
        depends_on,
    })
}

fn extract_kind(s: &str) -> Option<(&str, &str)> {
    if let Some(rest) = s.strip_prefix("**[") {
        let (kind, tail) = rest.split_once("]**")?;
        let kind = kind.trim();
        if kind.is_empty() {
            return None;
        }
        return Some((kind, tail));
    }
    if let Some(rest) = s.strip_prefix('[') {
        let (kind, tail) = rest.split_once(']')?;
        let kind = kind.trim();
        if kind.is_empty() {
            return None;
        }
        return Some((kind, tail));
    }
    None
}

/// Split out the optional `` `#id` `` or bare `#id` suffix.
/// Returns (body_without_id, id_or_none).
fn extract_id(body: &str) -> (String, Option<String>) {
    // Backticked form: `foo ... `#my-id``.
    if let Some(start) = body.rfind("`#") {
        // Must end with ` and contain only id-safe chars.
        let after = &body[start + 2..];
        if let Some(end) = after.find('`') {
            let id = after[..end].to_string();
            if !id.is_empty()
                && id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                let prefix = body[..start].trim_end();
                return (prefix.to_string(), Some(id));
            }
        }
    }
    // Bare `#id` at end-of-line.
    let t = body.trim_end();
    if let Some(pos) = t.rfind('#') {
        let before = &t[..pos];
        let id = &t[pos + 1..];
        // `id` must be nonempty and id-safe.
        if !id.is_empty()
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            // Require whitespace or start-of-string before the `#`.
            if before.is_empty()
                || before
                    .chars()
                    .last()
                    .map(|c| c.is_whitespace())
                    .unwrap_or(false)
            {
                // Drop trailing colon that sometimes precedes the id.
                let mut prefix = before.trim_end().to_string();
                if prefix.ends_with(':') {
                    prefix.pop();
                    prefix = prefix.trim_end().to_string();
                }
                return (prefix, Some(id.to_string()));
            }
        }
    }
    (body.to_string(), None)
}

/// Split out the optional `(after: a, b)` dependency clause.
fn extract_depends(body: &str) -> (String, Vec<String>) {
    let Some(start) = body.find("(after:") else {
        return (body.to_string(), Vec::new());
    };
    let Some(rel_end) = body[start..].find(')') else {
        return (body.to_string(), Vec::new());
    };
    let end = start + rel_end;
    let inside = &body[start + "(after:".len()..end];
    let deps: Vec<String> = inside
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let mut out = String::with_capacity(body.len());
    out.push_str(&body[..start]);
    out.push_str(&body[end + 1..]);
    (out.trim().to_string(), deps)
}

/// Split `name — reason` on the first em-dash.
fn split_name_reason(body: &str) -> (&str, &str) {
    if let Some(idx) = body.find(" — ") {
        let (n, r) = body.split_at(idx);
        return (n, &r[" — ".len()..]);
    }
    // Accept ASCII fallback `" -- "` too; some editors auto-replace.
    if let Some(idx) = body.find(" -- ") {
        let (n, r) = body.split_at(idx);
        return (n, &r[" -- ".len()..]);
    }
    (body, "")
}

fn slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_hyphen = true;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_hyphen = false;
        } else if !last_hyphen {
            out.push('-');
            last_hyphen = true;
        }
    }
    if out.ends_with('-') {
        out.pop();
    }
    if out.len() > 40 {
        out.truncate(40);
        if out.ends_with('-') {
            out.pop();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_prose_only() {
        let raw = "just some prompt\nline 2";
        let pf = parse(raw);
        assert_eq!(pf.prompt, "just some prompt\nline 2");
        assert!(pf.lenses.is_empty());
    }

    #[test]
    fn parses_markdown_todo_basic() {
        let raw = "\
Analyse io_uring.

- [ ] **[investigate]** memory allocations — check kmalloc + GFP_KERNEL flags
- [ ] **[investigate]** bounds checks
- [x] **[investigate]** done already — skipped";
        let pf = parse(raw);
        assert_eq!(pf.lenses.len(), 2);
        assert_eq!(pf.lenses[0].kind, "investigate");
        assert_eq!(pf.lenses[0].name, "memory allocations");
        assert!(pf.lenses[0].reason.contains("kmalloc"));
        assert_eq!(pf.lenses[1].name, "bounds checks");
        assert!(pf.prompt.contains("Analyse io_uring"));
    }

    #[test]
    fn extracts_backticked_id() {
        let raw = "- [ ] **[investigate]** memory — something `#mem-alloc`";
        let pf = parse(raw);
        assert_eq!(pf.lenses[0].id, "mem-alloc");
    }

    #[test]
    fn extracts_bare_trailing_id() {
        let raw = "- [ ] **[investigate]** memory: #mem";
        let pf = parse(raw);
        assert_eq!(pf.lenses[0].id, "mem");
    }

    #[test]
    fn extracts_depends_on() {
        let raw = "- [ ] **[investigate]** races (after: mem, setup) — check locks";
        let pf = parse(raw);
        assert_eq!(pf.lenses[0].name, "races");
        // depends_on is stripped from `name`, present in reason? kres
        // LensSpec doesn't carry depends_on; the parser returns ().
        assert_eq!(pf.lenses[0].reason, "check locks");
    }

    #[test]
    fn folds_indented_sub_bullets_into_reason() {
        let raw = "\
- [ ] **[investigate]** memory — primary reason
    - more detail line
    - yet more detail";
        let pf = parse(raw);
        assert_eq!(pf.lenses.len(), 1);
        assert!(pf.lenses[0].reason.contains("primary reason"));
        assert!(pf.lenses[0].reason.contains("more detail line"));
        assert!(pf.lenses[0].reason.contains("yet more detail"));
    }

    #[test]
    fn done_items_are_skipped() {
        let raw = "\
- [x] **[investigate]** already done
- [ ] **[investigate]** still pending";
        let pf = parse(raw);
        assert_eq!(pf.lenses.len(), 1);
        assert_eq!(pf.lenses[0].name, "still pending");
    }

    #[test]
    fn bracket_lines_stay_in_prose() {
        let raw = "[note] this is not a lens\n[investigate] not a markdown todo";
        let pf = parse(raw);
        assert_eq!(pf.lenses.len(), 0);
        assert!(pf.prompt.contains("[note]"));
        assert!(pf.prompt.contains("[investigate]"));
    }

    #[test]
    fn lens_ids_are_unique_slugs() {
        let raw = "\
- [ ] **[investigate]** memory allocations
- [ ] **[investigate]** memory leaks";
        let pf = parse(raw);
        assert_eq!(pf.lenses.len(), 2);
        assert_ne!(pf.lenses[0].id, pf.lenses[1].id);
    }

    #[test]
    fn slug_cap_at_40() {
        let id = slug("investigate-this-is-a-very-long-and-detailed-lens-name-that-rambles");
        assert!(id.len() <= 40);
        assert!(!id.ends_with('-'));
    }

    #[test]
    fn leading_whitespace_on_markdown_todo_ok() {
        let pf = parse("    - [ ] **[investigate]** memory");
        assert_eq!(pf.lenses.len(), 1);
        assert_eq!(pf.lenses[0].name, "memory");
    }

    #[test]
    fn preamble_survives_interleaved_todos() {
        let raw = "\
top line
- [ ] **[investigate]** one
middle line
- [ ] **[investigate]** two
bottom line";
        let pf = parse(raw);
        assert_eq!(pf.lenses.len(), 2);
        assert!(pf.prompt.contains("top line"));
        assert!(pf.prompt.contains("middle line"));
        assert!(pf.prompt.contains("bottom line"));
    }
}
