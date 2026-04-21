//! Per-task pipeline mode.
//!
//! `Analysis` is the historical default: the fast+main loop gathers,
//! the slow agent fans out across lenses, and the consolidator +
//! merger fold each lens's findings into the cumulative list.
//!
//! `Coding` swaps the slow-agent system prompt for one that writes
//! source code (reproducers, PoCs, selftests). The pipeline skips the
//! lens fan-out, the consolidator, and the cross-task merger entirely
//! — a coding task produces files and prose notes, not findings. The
//! goal agent still judges completion and is what drives follow-on
//! coding turns when needed.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskMode {
    Analysis,
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

impl Default for TaskMode {
    fn default() -> Self {
        Self::Analysis
    }
}

impl TaskMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Analysis => "analysis",
            Self::Coding => "coding",
        }
    }
}
