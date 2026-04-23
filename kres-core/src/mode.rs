//! Per-task pipeline mode.
//!
//! Three flows:
//!
//! `Audit` — the defect-review flow. The fast+main loop gathers
//! context, the slow agent fans out across session-wide lenses
//! (from the review-template), and the consolidator + cross-task
//! merger fold per-lens findings into the cumulative list. Picked
//! when the operator asked to "review", "audit", or "find bugs
//! in" a target. Degrades to a single slow call when no lenses
//! are configured.
//!
//! `Generic` — just the main/fast/slow/goal loop, no lens fan-out.
//! One slow call per task, findings still merge into the cumulative
//! list.  Good for free-form questions ("explain X", "what does this
//! do", "trace the call path from Y to Z", efficiency reviews) where
//! the multi-angle defect spread would be overkill.
//!
//! `Coding` swaps the slow-agent system prompt for one that writes
//! files (source code for reproducers/PoCs/selftests, OR prose
//! documents like markdown reports to an operator-named path). The
//! pipeline skips the lens fan-out, the consolidator, and the
//! findings pipeline entirely — a coding task produces files and
//! prose notes, not findings. The goal agent still judges
//! completion and is what drives follow-on coding turns when
//! needed.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskMode {
    Audit,
    /// The fallback mode. Matches the goal-agent classifier's stated
    /// "Default to 'generic' when the prompt is ambiguous" rule —
    /// when the classifier misbehaves (empty mode, unparseable mode,
    /// network failure), the Rust code falls back here instead of
    /// silently routing to the defect-review pipeline.
    #[default]
    Generic,
    Coding,
}

/// One file emitted by a coding-mode slow-agent turn. `path` is a
/// forward-slash relative path (no leading `/`); `content` is the
/// verbatim file body; `purpose` is a one-line description the
/// reaper can log when persisting. The Session writes each
/// CodeFile under `<results>/code/<path>`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CodeFile {
    pub path: String,
    pub content: String,
    #[serde(default)]
    pub purpose: String,
}

/// One string-replacement edit emitted by a coding-mode slow-agent
/// turn. Shape mirrors Claude Code's Edit primitive; `file_path` is
/// resolved via the workspace / consent path the same way `read`
/// and `edit` actions are. Reaper applies each entry via
/// `kres_agents::tools::edit_file`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CodeEdit {
    #[serde(alias = "path")]
    pub file_path: String,
    pub old_string: String,
    pub new_string: String,
    #[serde(default)]
    pub replace_all: bool,
}

impl TaskMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Audit => "audit",
            Self::Generic => "generic",
            Self::Coding => "coding",
        }
    }

    /// True for modes that feed the findings pipeline (findings merger
    /// runs, /summary output is meaningful). Coding tasks produce
    /// files instead of findings, so they return false.
    pub fn produces_findings(self) -> bool {
        matches!(self, Self::Audit | Self::Generic)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_generic() {
        // Pinned: the Rust fallback must match goal.txt's classifier
        // policy ("Default to 'generic' when the prompt is
        // ambiguous"). Changing this back to Audit would silently
        // route every misclassified prompt to defect-review.
        assert_eq!(TaskMode::default(), TaskMode::Generic);
    }

    #[test]
    fn as_str_matches_wire_encoding() {
        // as_str and the serde rename_all="lowercase" must agree —
        // otherwise logs say one thing and the wire says another.
        for m in [TaskMode::Audit, TaskMode::Generic, TaskMode::Coding] {
            let wire = serde_json::to_string(&m).unwrap();
            let unquoted = wire.trim_matches('"');
            assert_eq!(unquoted, m.as_str(), "{:?}", m);
        }
    }

    #[test]
    fn produces_findings_excludes_coding_only() {
        assert!(TaskMode::Audit.produces_findings());
        assert!(TaskMode::Generic.produces_findings());
        assert!(!TaskMode::Coding.produces_findings());
    }
}
