//! Slash-command templates used by non-workflow commands.
//!
//! Each name maps to an `.md` body that is compiled into the kres
//! binary via `include_str!`. An operator who wants to override a
//! command drops a file at `~/.kres/commands/<name>.md` and kres
//! reads it ahead of the embedded copy. The default install has no
//! files under that directory; the embedded copies do all the work.
//!
//! Two code paths feed this table:
//!
//! - Summary rendering uses the `summary` and `summary-markdown`
//!   bodies directly.
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
        "summary",
        include_str!("../../configs/prompts/bug-summary.md"),
    ),
    (
        "summary-markdown",
        include_str!("../../configs/prompts/bug-summary-markdown.md"),
    ),
    (
        "commit-kernel",
        include_str!("../../configs/prompts/commit-kernel-template.md"),
    ),
];

/// Return the body for `name` — disk override wins, then the
/// embedded default, else None. The disk override path is
/// `~/.kres/commands/<name>.md`; non-existent and empty files
/// fall through to the embedded copy.
///
/// Names are restricted to `[a-zA-Z0-9_-]+` — a stray `/`, `\`,
/// or path segment would otherwise resolve to a file outside the
/// commands directory. Callers whose input is already restricted
/// (e.g. `kres/src/main.rs::resolve_prompt_arg` filters the same
/// character set) will never hit the reject path, but keeping the
/// guard here means a future caller that forgets to sanitize
/// still can't escape the directory.
///
/// Workflow-owned commands are intentionally not slash templates.
/// `/fix`, `/review`, `/triage`, and `/validate` dispatch through
/// the workflow runner only.
pub fn lookup(name: &str) -> Option<String> {
    lookup_with_root(
        dirs::home_dir().map(|h| h.join(".kres").join("commands")),
        name,
    )
}

/// Testable core of `lookup`. `commands_dir` is the directory to
/// consult for disk overrides (pass `None` to skip the disk step
/// entirely — useful in tests that want to pin the embedded
/// default). `name` is validated against the same character set
/// as the public `lookup`.
pub fn lookup_with_root(commands_dir: Option<std::path::PathBuf>, name: &str) -> Option<String> {
    if !is_valid_name(name) {
        return None;
    }
    if matches!(name, "fix" | "review" | "triage" | "validate") {
        return None;
    }
    if let Some(dir) = commands_dir {
        let p = dir.join(format!("{name}.md"));
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

/// A command name is a non-empty run of ASCII alphanumerics, `-`,
/// and `_`. Anything else risks turning the lookup into a
/// directory-traversal primitive (`../etc/passwd`) or hitting
/// a file whose basename collides with the command name by
/// accident (a dotfile, a dot-segment, etc.).
fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Every command name that has an embedded default. Consumers iterate
/// this for discovery (e.g. the `/help` listing or the CLI synopsis).
pub fn embedded_names() -> impl Iterator<Item = &'static str> {
    TABLE.iter().map(|(k, _)| *k)
}

/// Compose a full prompt from a command-template name and trailing
/// extra text.
///
/// This is intentionally not used for workflow-owned commands
/// (`fix`, `review`, `triage`, `validate`) or summary rendering
/// (`summary`, `summary-markdown`). Those have dedicated command
/// paths; treating them as prompt templates would create a second
/// execution model.
/// Returns `Some((source-label, body))` when `name` resolves to
/// a known command, `None` when the lookup fails.
///
pub fn compose(name: &str, extra: &str) -> Option<(String, String)> {
    if matches!(name, "summary" | "summary-markdown") {
        return None;
    }
    let body = lookup(name)?;
    let extra = extra.trim();
    let composed = if extra.is_empty() {
        body
    } else {
        format!("{extra}\n\n{body}")
    };
    Some((format!("/{name} (user_commands)"), composed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_embedded_body_is_non_empty() {
        for name in embedded_names() {
            let body = lookup(name).unwrap_or_default();
            assert!(!body.trim().is_empty(), "command {name} body is empty");
        }
    }

    #[test]
    fn all_expected_commands_are_present() {
        for expected in ["summary", "summary-markdown", "commit-kernel"] {
            assert!(
                lookup(expected).is_some(),
                "expected embedded command {expected} not found"
            );
        }
    }

    #[test]
    fn commit_kernel_body_contains_template_markers() {
        // The commit-kernel template encodes the kernel-style
        // commit-message rules (Rule 0: 72-char wrap; subject ≤70;
        // imperative mood; -s for sign-off). If include_str! ever
        // points at the wrong file, those rules silently disappear
        // and the agent reverts to free-form prose. Anchor on the
        // distinctive "Rule 0" marker plus the AVOID list header.
        let body = lookup("commit-kernel").unwrap();
        assert!(
            body.contains("Rule 0"),
            "commit-kernel body missing Rule 0 (72-char wrap)"
        );
        assert!(
            body.contains("What to AVOID"),
            "commit-kernel body missing the AVOID checklist"
        );
    }

    #[test]
    fn unknown_name_returns_none() {
        assert!(lookup("no-such-command").is_none());
    }

    #[test]
    fn disk_override_wins_over_embedded() {
        let dir = std::env::temp_dir().join(format!("kres-cmd-override-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("summary.md"), "OPERATOR SUMMARY OVERRIDE").unwrap();
        let got = lookup_with_root(Some(dir.clone()), "summary").expect("override should resolve");
        assert_eq!(got, "OPERATOR SUMMARY OVERRIDE");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn disk_override_empty_falls_through_to_embedded() {
        // An empty file at the override path should NOT shadow the
        // embedded copy (consistent with the agent-prompt loader's
        // behaviour) — returning empty prompt text would brick the
        // command silently.
        let dir =
            std::env::temp_dir().join(format!("kres-cmd-empty-override-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("summary.md"), "   \n\t\n").unwrap();
        let got = lookup_with_root(Some(dir.clone()), "summary")
            .expect("should fall through to embedded");
        assert!(got.contains("Subject:"), "got {got:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn traversal_name_is_rejected() {
        // `../foo` and friends must never be turned into a disk
        // path by the loader. `lookup` returns None for any name
        // that isn't ASCII alphanumeric + `-`/`_`.
        assert!(lookup("../etc/passwd").is_none());
        assert!(lookup("a/b").is_none());
        assert!(lookup("").is_none());
        assert!(lookup(".").is_none());
        assert!(lookup("..").is_none());
        // lookup_with_root is equally strict — even when the
        // caller hands it a seemingly safe commands_dir.
        let dir = std::env::temp_dir();
        assert!(lookup_with_root(Some(dir), "../etc/passwd").is_none());
    }

    #[test]
    fn compose_prepends_extra_to_body() {
        let (src, body) = compose("commit-kernel", "describe the change").unwrap();
        assert!(
            src.contains("commit-kernel"),
            "source label should name the command: {src}"
        );
        assert!(
            body.starts_with("describe the change\n\n"),
            "extra text must lead the composed body: {body:?}"
        );
        assert!(
            body.contains("Rule 0"),
            "template body must follow: {body:?}"
        );
    }

    #[test]
    fn compose_empty_extra_returns_bare_body() {
        // Unique job of this test: an empty `extra` argument must
        // not prepend a blank `extra\n\n` block to the body. The
        let (_, body_empty) = compose("commit-kernel", "").unwrap();
        let (_, body_ws) = compose("commit-kernel", "  \n\t ").unwrap();
        let expected = lookup("commit-kernel").unwrap();
        assert_eq!(
            body_empty, expected,
            "empty extra must yield the bare template body"
        );
        assert_eq!(
            body_ws, expected,
            "whitespace-only extra (trimmed to empty) must behave the same"
        );
    }

    #[test]
    fn compose_unknown_name_returns_none() {
        assert!(compose("no-such-command", "target").is_none());
        assert!(compose("summary", "target").is_none());
        assert!(compose("summary-markdown", "target").is_none());
    }

    #[test]
    fn workflow_commands_are_not_templates() {
        let dir =
            std::env::temp_dir().join(format!("kres-workflow-override-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["fix", "review", "triage", "validate"] {
            std::fs::write(dir.join(format!("{name}.md")), "operator override").unwrap();
            assert!(lookup(name).is_none());
            assert!(lookup_with_root(None, name).is_none());
            assert!(
                lookup_with_root(Some(dir.clone()), name).is_none(),
                "disk commands/{name}.md overrides must not resurrect workflow commands"
            );
            assert!(compose(name, "/path/to/target").is_none());
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn compose_commit_kernel_does_not_self_append() {
        // /commit-kernel must not append itself recursively.
        let (_, body) = compose("commit-kernel", "describe the change").unwrap();
        assert!(
            !body.contains("# COMMIT MESSAGE STYLE (appended by /fix)"),
            "commit-kernel body unexpectedly self-appends"
        );
    }
}
