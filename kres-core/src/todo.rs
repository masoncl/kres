//! Todo items.
//!
//! Mirrors shape but with strong typing for the
//! status enum and validated construction.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Done,
    Blocked,
    Skipped,
}

impl TodoStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, TodoStatus::Done | TodoStatus::Skipped)
    }
}

/// Stamped on a done row when no real coverage sentence exists yet.
///
/// A done row with EMPTY coverage is worse than a vague one: the todo
/// agent's dedup step reads coverage to decide whether a proposed
/// followup is already covered, and an empty field silently drops the
/// row out of that comparison. So Rust fills one in.
///
/// It is a fallback, not an answer. [`coverage_is_unwritten`] treats
/// it as absent so the agent's real sentence can still replace it.
pub const PLACEHOLDER_COVERAGE: &str = "completed by the reaped task";

/// Whether a coverage field still needs the agent's real sentence.
///
/// Coverage is write-once so a later round cannot paraphrase settled
/// evidence — but the placeholder is not settled evidence. Guarding
/// write-once on `is_empty()` alone meant `mark_todo_done` stamping
/// the placeholder the moment a task finished, before the todo agent
/// was ever asked, and every real sentence being discarded on
/// arrival. Observed on the 2026-08-07 kernel/sched/fair.c review:
/// 74 of 74 done rows stored the placeholder while the agent had
/// returned substantive coverage for 35 of them.
pub fn coverage_is_unwritten(coverage: &str) -> bool {
    let trimmed = coverage.trim();
    trimmed.is_empty() || trimmed == PLACEHOLDER_COVERAGE
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TodoItem {
    /// Short stable slug.
    pub name: String,
    /// Item type: "investigate", "question", "read", etc.
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default = "default_pending")]
    pub status: TodoStatus,
    /// Why this item was added.
    #[serde(default)]
    pub reason: String,
    /// Names of items that must reach `Done` before this one runs.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// What the completed task actually examined, written once by the
    /// todo agent from that task's analysis.
    ///
    /// This is the field the agent's DEDUP step reads to decide
    /// whether a proposed followup is already covered, so a done row
    /// carrying only [`PLACEHOLDER_COVERAGE`] is invisible to it —
    /// see [`coverage_is_unwritten`].
    #[serde(default)]
    pub coverage: String,
    /// Short stable ID assigned by the todo-agent. Distinct from
    /// `name` so the agent can REPRIORITIZE without breaking dep
    /// references that cite the old id.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// Optional pointer to the plan step this todo is executing.
    /// Written by the todo-agent when a plan is in play (the agent
    /// sees the plan in its user JSON and picks the best-matching
    /// step id); consumed by `crate::plan::Plan::sync_from_todo` to
    /// roll up step status. Empty string means "not yet linked" —
    /// most common for todos created before a plan existed, or for
    /// followups the agent couldn't confidently attribute.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub step_id: String,
}

fn default_pending() -> TodoStatus {
    TodoStatus::Pending
}

impl TodoItem {
    pub fn new(name: impl Into<String>, kind: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            kind: kind.into(),
            status: TodoStatus::Pending,
            reason: String::new(),
            depends_on: Vec::new(),
            coverage: String::new(),
            id: String::new(),
            step_id: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_terminal() {
        assert!(TodoStatus::Done.is_terminal());
        assert!(TodoStatus::Skipped.is_terminal());
        assert!(!TodoStatus::Pending.is_terminal());
        assert!(!TodoStatus::InProgress.is_terminal());
        assert!(!TodoStatus::Blocked.is_terminal());
    }

    #[test]
    fn serde_roundtrip_lowercase() {
        let t = TodoItem::new("x", "investigate");
        let s = serde_json::to_string(&t).unwrap();
        assert!(s.contains("\"status\":\"pending\""));
        let back: TodoItem = serde_json::from_str(&s).unwrap();
        assert_eq!(back.name, "x");
        assert_eq!(back.status, TodoStatus::Pending);
    }

    #[test]
    fn status_default_is_pending() {
        let s = r#"{"name": "a", "type": "question"}"#;
        let t: TodoItem = serde_json::from_str(s).unwrap();
        assert_eq!(t.status, TodoStatus::Pending);
    }
}
