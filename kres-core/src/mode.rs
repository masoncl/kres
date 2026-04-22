//! Per-task pipeline mode.
//!
//! Three flows:
//!
//! `Analysis` — the review flow.  The fast+main loop gathers context,
//! the slow agent fans out across session-wide lenses (from the
//! review-template), and the consolidator + cross-task merger fold
//! per-lens findings into the cumulative list.  Degrades to a single
//! slow call when no lenses are configured.
//!
//! `Generic` — just the main/fast/slow/goal loop, no lens fan-out.
//! One slow call per task, findings still merge into the cumulative
//! list.  Good for free-form questions ("explain X", "what does this
//! do", "trace the call path from Y to Z") where the review-template
//! multi-angle spread would be overkill.
//!
//! `Coding` swaps the slow-agent system prompt for one that writes
//! source code (reproducers, PoCs, selftests). The pipeline skips the
//! lens fan-out, the consolidator, and the cross-task merger entirely
//! — a coding task produces files and prose notes, not findings. The
//! goal agent still judges completion and is what drives follow-on
//! coding turns when needed.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskMode {
    #[default]
    Analysis,
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
            Self::Analysis => "analysis",
            Self::Generic => "generic",
            Self::Coding => "coding",
        }
    }

    /// True for modes that feed the findings pipeline (findings merger
    /// runs, /summary output is meaningful). Coding tasks produce
    /// files instead of findings, so they return false.
    pub fn produces_findings(self) -> bool {
        matches!(self, Self::Analysis | Self::Generic)
    }
}
