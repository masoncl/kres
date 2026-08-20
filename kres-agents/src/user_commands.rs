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

const KERNEL_PROBLEM_DESCRIPTION: &str =
    include_str!("../../configs/prompts/kernel-problem-description.md");
const COMMIT_LOG_DESCRIPTORS: &str =
    include_str!("../../configs/prompts/commit-log-descriptors.md");
const KERNEL_FIX_DESCRIPTION: &str =
    include_str!("../../configs/prompts/commit-kernel-template.md");

pub fn kernel_problem_prompt(specific: &str) -> String {
    format!(
        "{}\n\n{}\n\n{}",
        KERNEL_PROBLEM_DESCRIPTION.trim_end(),
        COMMIT_LOG_DESCRIPTORS.trim(),
        specific.trim_start()
    )
}

pub fn kernel_fix_prompt() -> String {
    kernel_problem_prompt(KERNEL_FIX_DESCRIPTION)
}

/// Name → command-specific body. Kernel problem-writing rules and the shared
/// descriptor catalog are prepended by [`lookup_with_root`] so summaries and
/// fix messages cannot drift.
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

/// Return the body for `name` — disk override wins for the command-specific
/// section, then the embedded default, else None. Kernel commands always have
/// the shared problem-description rules and descriptor catalog prepended in
/// Rust. The disk override path is `~/.kres/commands/<name>.md`; non-existent
/// and empty files fall through to the embedded command-specific section.
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
    let disk_body = commands_dir.and_then(|dir| {
        let p = dir.join(format!("{name}.md"));
        if let Ok(s) = std::fs::read_to_string(&p) {
            if !s.trim().is_empty() {
                return Some(s);
            }
        }
        None
    });
    let has_disk_body = disk_body.is_some();
    let specific = disk_body.or_else(|| {
        TABLE
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| (*v).to_string())
    })?;
    Some(if name == "commit-kernel" && !has_disk_body {
        kernel_fix_prompt()
    } else {
        kernel_problem_prompt(&specific)
    })
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
        let body = lookup("commit-kernel").unwrap();
        let normalized = body.split_whitespace().collect::<Vec<_>>().join(" ");
        for marker in [
            // Emphasis, not structure. A template can enforce trailer
            // order and column width, yet a message that obeys both can
            // still bury the reason a maintainer would care.
            "Lead with the symptom",
            "Say so when the defect was reproduced",
            "Describe the defect, not only the edit",
            "Argue once",
            "One defect, one patch",
            "When the doc and this template disagree, the doc wins",
            "Rule 0 — 75-column wrap (the most important one)",
            "Make a non-prose descriptor from the appended \"Non-prose technical description techniques\" catalog the default way to explain the bug",
            "Prose is supporting material only",
            "do not turn the body into an exhaustive proof of every path that was checked",
            "Non-prose technical description techniques",
            "Hard rule: never draw boxes",
            "Linear call chain",
            "Before/after state block",
            "Verbatim source excerpt for a bug",
            "Pseudocode excerpt for a solution",
            "Optimisation and trade-off claims",
            "If a backtrace helps document the call chain, distill it",
            "Prose that restates, summarizes, or walks through a descriptor",
            "Kernel fix description rules",
            "must not exceed 55 chars",
            "required when the change repairs a regression introduced by a specific commit",
            "NEVER add `Cc: stable@vger.kernel.org`",
            "Assisted-by: kres:<model-id>",
            "commit -s -F .kres-commit-msg.tmp",
            "Self-check before emitting",
        ] {
            assert!(
                normalized.contains(marker),
                "commit prompt missing {marker:?}"
            );
        }
        assert_eq!(
            body.matches("# Non-prose technical description techniques")
                .count(),
            1
        );
    }

    #[test]
    fn summary_templates_share_problem_rules_without_fix_rules() {
        for name in ["summary", "summary-markdown"] {
            let body = lookup_with_root(None, name).unwrap();
            let normalized = body.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(body.contains("Kernel problem description rules"), "{name}");
            assert!(!body.contains("Kernel fix description rules"), "{name}");
            assert!(
                normalized.contains("Do not propose or describe a fix"),
                "{name}"
            );
            assert!(body.contains("Two-column CPU or thread timeline"), "{name}");
            assert!(
                normalized.contains("If a backtrace helps document the call chain, distill it"),
                "{name}"
            );
            assert!(body.contains("Hard rule: never draw boxes"), "{name}");
            assert!(body.contains("Verbatim source excerpt for a bug"), "{name}");
            assert!(
                normalized.contains("Never emit pseudocode in a bug summary"),
                "{name}"
            );
            assert!(body.contains("[ ... ] // omitted: <reason>"), "{name}");
            assert_eq!(
                body.matches("# Non-prose technical description techniques")
                    .count(),
                1,
                "{name}"
            );
            assert!(!body.contains("Assisted-by:"), "{name}");
        }
    }

    #[test]
    fn descriptor_catalog_has_no_box_diagrams() {
        assert!(
            !COMMIT_LOG_DESCRIPTORS
                .chars()
                .any(|c| ('\u{2500}'..='\u{257f}').contains(&c)),
            "descriptor catalog contains Unicode box-drawing characters"
        );
        for line in COMMIT_LOG_DESCRIPTORS.lines() {
            let trimmed = line.trim();
            let ascii_border = (trimmed.starts_with('+') && trimmed.ends_with('+'))
                || (trimmed.starts_with('|') && trimmed.ends_with('|'));
            assert!(
                !ascii_border,
                "descriptor catalog contains a box row: {line}"
            );
        }
        assert!(
            !COMMIT_LOG_DESCRIPTORS.contains("]["),
            "descriptor catalog contains adjacent bracketed fields that resemble boxes"
        );
    }

    #[test]
    fn descriptor_catalog_is_applicable_not_mandatory_and_bug_safe() {
        let normalized = COMMIT_LOG_DESCRIPTORS
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(COMMIT_LOG_DESCRIPTORS.contains("whenever it makes"));
        assert!(COMMIT_LOG_DESCRIPTORS.contains("Do not force a descriptor"));
        assert!(!COMMIT_LOG_DESCRIPTORS.contains("not mandatory"));
        assert!(!COMMIT_LOG_DESCRIPTORS.contains("    right:"));
        assert!(!COMMIT_LOG_DESCRIPTORS.contains("source or pseudocode excerpt"));
        assert!(COMMIT_LOG_DESCRIPTORS.contains("copy it verbatim"));
        assert!(normalized.contains("Never replace a single source line"));
        assert!(normalized.contains("Do not use source-language comment syntax"));
        assert!(normalized.contains("Never use an omission marker to hide control flow"));
        assert!(COMMIT_LOG_DESCRIPTORS.contains(
            "Pseudocode is allowed only when explaining a proposed or implemented solution"
        ));
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
        assert!(got.starts_with(KERNEL_PROBLEM_DESCRIPTION.trim_start()));
        assert!(got.contains("# Non-prose technical description techniques"));
        assert!(got.ends_with("OPERATOR SUMMARY OVERRIDE"));
        assert!(!got.contains("Plain-text validated finding summary"));
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
        assert!(
            got.contains("Plain-text validated finding summary"),
            "got {got:?}"
        );
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
            body.contains("Kernel problem description rules")
                && body.contains("Non-prose technical description techniques")
                && body.contains("Kernel fix description rules"),
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
