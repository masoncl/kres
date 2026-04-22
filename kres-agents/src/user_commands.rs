//! Slash-command templates: `/review`, `/summary`, `/summary-markdown`.
//!
//! Each name maps to an `.md` body that is compiled into the kres
//! binary via `include_str!`. An operator who wants to override a
//! command drops a file at `~/.kres/commands/<name>.md` and kres
//! reads it ahead of the embedded copy. The default install has no
//! files under that directory; the embedded copies do all the work.
//!
//! Two code paths feed this table:
//!
//! - CLI `--prompt "word: extra"` and `--prompt "/word extra"` both
//!   resolve via `lookup(word)` and prepend `extra` to the body.
//! - REPL slash commands `/review <target>`, `/summary`, and
//!   `/summary-markdown` read the body through the same lookup.
//!
//! Distinct from `kres_agents::embedded_prompts`: that module
//! bundles the agent `*.system.md` prompts (fast/slow/main/todo
//! system text), whose override directory is
//! `~/.kres/system-prompts/`. Slash-command templates are
//! operator-invoked prompts, not agent system prompts, so they
//! get their own directory (`~/.kres/commands/`) and override
//! path.

/// Name → body. Keep aligned with the shipped files under
/// `configs/prompts/`.
const TABLE: &[(&str, &str)] = &[
    (
        "review",
        include_str!("../../configs/prompts/review-template.md"),
    ),
    (
        "summary",
        include_str!("../../configs/prompts/bug-summary.md"),
    ),
    (
        "summary-markdown",
        include_str!("../../configs/prompts/bug-summary-markdown.md"),
    ),
];

/// Return the body for `name` — disk override wins, then the
/// embedded default, else None. The disk override path is
/// `~/.kres/commands/<name>.md`; non-existent and empty files
/// fall through to the embedded copy.
pub fn lookup(name: &str) -> Option<String> {
    if let Some(home) = dirs::home_dir() {
        let p = home
            .join(".kres")
            .join("commands")
            .join(format!("{name}.md"));
        if let Ok(s) = std::fs::read_to_string(&p) {
            if !s.trim().is_empty() {
                return Some(s);
            }
        }
    }
    TABLE
        .iter()
        .find(|(k, _)| *k == name)
        .map(|(_, v)| (*v).to_string())
}

/// Every command name that has an embedded default. Consumers iterate
/// this for discovery (e.g. the `/help` listing or the CLI synopsis).
pub fn embedded_names() -> impl Iterator<Item = &'static str> {
    TABLE.iter().map(|(k, _)| *k)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_embedded_body_is_non_empty() {
        for name in embedded_names() {
            let body = lookup(name).unwrap_or_default();
            assert!(
                !body.trim().is_empty(),
                "command {name} body is empty"
            );
        }
    }

    #[test]
    fn all_expected_commands_are_present() {
        for expected in ["review", "summary", "summary-markdown"] {
            assert!(
                lookup(expected).is_some(),
                "expected embedded command {expected} not found"
            );
        }
    }

    #[test]
    fn unknown_name_returns_none() {
        assert!(lookup("no-such-command").is_none());
    }

    #[test]
    fn review_body_contains_template_markers() {
        // Sanity check — the review template is the lens-bullet
        // markdown file, which the prompt-file parser keys on
        // `[investigate]` bullets. If the include_str stops pointing
        // at the right file this would silently pick up a different
        // body; asserting a literal marker catches that.
        let body = lookup("review").unwrap();
        assert!(
            body.contains("[investigate]"),
            "review body missing [investigate] marker"
        );
    }
}
