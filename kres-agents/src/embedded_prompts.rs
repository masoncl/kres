//! LLM system prompts compiled into the kres binary.
//!
//! Markdown files under `configs/prompts/` that carry system-prompt
//! text for an LLM call (agent `*.system.md` prompts plus the
//! `bug-summary` templates for the `/summary` pipeline) are
//! included via `include_str!` at build time. A freshly-rebuilt
//! kres already knows the current prompts — no `setup.sh
//! --overwrite` dance is needed every time the repo's prompts
//! change.
//!
//! Disk still wins: an operator who wants to customize a prompt
//! drops a file at `~/.kres/system-prompts/<basename>` and kres
//! reads it ahead of the embedded copy. The embedded entry is the
//! fallback when the disk path is absent (the normal case — the
//! `system-prompts/` directory is empty by default).
//!
//! Excluded on purpose: the prompt TEMPLATES the operator wires
//! via `--prompt "word: extra"` (`review-template.md`,
//! `<word>-template.md`). Those are user-authored content, not
//! LLM system prompts, and they continue to live on disk under
//! `~/.kres/prompts/`.

/// Basename → verbatim prompt body. Keep the list aligned with
/// `configs/prompts/*.system.md` in the repo; a missing entry falls
/// through to "no embedded prompt" and the caller surfaces the disk
/// error as before.
const TABLE: &[(&str, &str)] = &[
    (
        "fast-code-agent.system.md",
        include_str!("../../configs/prompts/fast-code-agent.system.md"),
    ),
    (
        "main-agent.system.md",
        include_str!("../../configs/prompts/main-agent.system.md"),
    ),
    (
        "slow-code-agent.system.md",
        include_str!("../../configs/prompts/slow-code-agent.system.md"),
    ),
    (
        "slow-code-agent-coding.system.md",
        include_str!("../../configs/prompts/slow-code-agent-coding.system.md"),
    ),
    (
        "slow-code-agent-generic.system.md",
        include_str!("../../configs/prompts/slow-code-agent-generic.system.md"),
    ),
    (
        "todo-agent.system.md",
        include_str!("../../configs/prompts/todo-agent.system.md"),
    ),
    (
        "bug-summary.md",
        include_str!("../../configs/prompts/bug-summary.md"),
    ),
    (
        "bug-summary-markdown.md",
        include_str!("../../configs/prompts/bug-summary-markdown.md"),
    ),
];

/// Return the embedded prompt body for a filename's basename, if
/// one is bundled in this build. `basename` is the final path
/// component with any directory prefix stripped (e.g.
/// `"main-agent.system.md"` for a config field
/// `"prompts/main-agent.system.md"`).
pub fn lookup(basename: &str) -> Option<&'static str> {
    TABLE
        .iter()
        .find(|(k, _)| *k == basename)
        .map(|(_, v)| *v)
}

/// Every basename that has an embedded copy. Useful for logging /
/// diagnostics.
pub fn embedded_names() -> impl Iterator<Item = &'static str> {
    TABLE.iter().map(|(k, _)| *k)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_embedded_prompt_is_non_empty() {
        for name in embedded_names() {
            let body = lookup(name).expect("lookup must succeed for listed name");
            assert!(
                !body.trim().is_empty(),
                "embedded prompt {name} is empty"
            );
        }
    }

    #[test]
    fn unknown_basename_returns_none() {
        assert!(lookup("does-not-exist.system.md").is_none());
    }

    #[test]
    fn lookup_is_exact_basename_match() {
        // Callers pass the basename only; a full path with a
        // directory prefix does not match.
        assert!(lookup("prompts/main-agent.system.md").is_none());
        assert!(lookup("main-agent.system.md").is_some());
    }

    #[test]
    fn all_expected_agent_prompts_are_present() {
        for expected in [
            "fast-code-agent.system.md",
            "main-agent.system.md",
            "slow-code-agent.system.md",
            "slow-code-agent-coding.system.md",
            "slow-code-agent-generic.system.md",
            "todo-agent.system.md",
        ] {
            assert!(
                lookup(expected).is_some(),
                "expected embedded prompt {expected} not found"
            );
        }
    }

    #[test]
    fn bug_summary_templates_are_present() {
        // bug-summary{,-markdown}.md back /summary and kres
        // --summary; they are LLM system prompts for the
        // summariser call, so they ride the same embed/override
        // pipeline as the agent prompts.
        assert!(lookup("bug-summary.md").is_some());
        assert!(lookup("bug-summary-markdown.md").is_some());
    }
}
