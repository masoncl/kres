//! Agent `*.system.md` prompts compiled into the kres binary.
//!
//! The agent-role system prompts (fast-code-agent, main-agent,
//! slow-code-agent, slow-code-agent-coding, slow-code-agent-generic,
//! todo-agent) are included via `include_str!` at build time. A
//! freshly-rebuilt kres already knows the current prompts — no
//! `setup.sh --overwrite` dance is needed every time the repo's
//! prompts change.
//!
//! Disk still wins: an operator who wants to customize an agent
//! prompt drops a file at `~/.kres/system-prompts/<basename>` and
//! kres reads it ahead of the embedded copy. The embedded entry is
//! the fallback when the disk path is absent (the normal case —
//! the `system-prompts/` directory is empty by default).
//!
//! Not covered here: slash-command templates invoked via
//! `--prompt "word: extra"`, `--prompt "/word extra"`, or REPL
//! commands like `/review` / `/summary` / `/summary-markdown`.
//! Those live in the separate `kres_agents::user_commands` module
//! with their own override directory (`~/.kres/commands/`). The
//! split exists so agent-role prompts and operator-authored
//! prompt content keep distinct override surfaces.

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
        "slow-code-agent-audit.system.md",
        include_str!("../../configs/prompts/slow-code-agent-audit.system.md"),
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
        "condense-task.system.md",
        include_str!("../../configs/prompts/condense-task.system.md"),
    ),
];

/// Translate legacy prompt-file basenames to their post-rename
/// equivalents. Operator configs installed from an older repo keep
/// the old `system_file` path; applying this shim on lookup lets
/// those configs resolve against the new embedded table without
/// the operator having to re-run setup.sh.
///
/// Add new entries here each time a `configs/prompts/*.system.md`
/// file gets renamed; remove them when the old basename has had
/// enough deprecation time. Each entry is a deliberate one-way
/// translation — the key is what operators might still emit, the
/// value is what the embedded TABLE keys on now.
fn translate_legacy_basename(basename: &str) -> &str {
    match basename {
        // b01c1ae (Analysis → Audit): defect-review system prompt
        // file renamed from slow-code-agent.system.md to
        // slow-code-agent-audit.system.md.
        "slow-code-agent.system.md" => "slow-code-agent-audit.system.md",
        other => other,
    }
}

/// Return the embedded prompt body for a filename's basename, if
/// one is bundled in this build. `basename` is the final path
/// component with any directory prefix stripped (e.g.
/// `"main-agent.system.md"` for a config field
/// `"prompts/main-agent.system.md"`). Legacy basenames from
/// pre-rename installs are translated to the current key via
/// [`translate_legacy_basename`] before the table lookup.
pub fn lookup(basename: &str) -> Option<&'static str> {
    let key = translate_legacy_basename(basename);
    TABLE.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
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
            assert!(!body.trim().is_empty(), "embedded prompt {name} is empty");
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
    fn legacy_slow_code_agent_basename_translates_to_audit() {
        // Operator configs installed before b01c1ae point at
        // `slow-code-agent.system.md`; the embedded table now keys
        // on `slow-code-agent-audit.system.md`. The translation
        // shim must resolve the old basename to the new prompt
        // body WITHOUT the operator needing to edit their config
        // or re-run setup.sh.
        let legacy = lookup("slow-code-agent.system.md")
            .expect("legacy basename must resolve via translation");
        let new = lookup("slow-code-agent-audit.system.md")
            .expect("new basename must resolve directly");
        assert_eq!(legacy, new, "translation must return identical body");
    }

    #[test]
    fn translate_legacy_passes_through_unknown_basenames() {
        // Non-legacy basenames must not be rewritten — the shim is
        // opt-in per entry.
        assert_eq!(translate_legacy_basename("todo-agent.system.md"), "todo-agent.system.md");
        assert_eq!(translate_legacy_basename("does-not-exist.md"), "does-not-exist.md");
    }

    #[test]
    fn all_expected_agent_prompts_are_present() {
        for expected in [
            "fast-code-agent.system.md",
            "main-agent.system.md",
            "slow-code-agent-audit.system.md",
            "slow-code-agent-coding.system.md",
            "slow-code-agent-generic.system.md",
            "todo-agent.system.md",
            "condense-task.system.md",
        ] {
            assert!(
                lookup(expected).is_some(),
                "expected embedded prompt {expected} not found"
            );
        }
    }
}
