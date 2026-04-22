//! Todo items.
//!
//! Mirrors shape but with strong typing for the
//! status enum and validated construction.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Optional coverage prose.
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
