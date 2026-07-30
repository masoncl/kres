//! Followup types the agents use to request data.
//!
//! The shape matches the existing wire format. We accept the
//! minor variations already handles (e.g.
//! `file` vs `path` aliases) so old agent prompts continue to
//! interoperate.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Followup {
    /// "survey", "source", "type", "callers", "callees", "search", "file",
    /// "read", "git", "question".
    #[serde(rename = "type")]
    pub kind: String,
    /// What to fetch: a symbol name, a regex, a path, etc.
    pub name: String,
    #[serde(default)]
    pub reason: String,
    /// Optional scoping path for search/file types.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// `true` marks a deferred / nice-to-have audit that does not
    /// block terminal classification. `false` (default) marks a
    /// blocking evidence request that must be gathered before the
    /// workflow can produce a terminal status. Workflow evals that
    /// require "no remaining followups" before declaring a terminal
    /// status only count entries with `nice_to_have == false`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub nice_to_have: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl Followup {
    /// Return a canonical cache key so the fast agent's dedup logic
    /// has something stable to compare against.
    pub fn cache_key(&self) -> String {
        if let Some(p) = &self.path {
            format!("{}::{}::{}", self.kind, self.name, p)
        } else {
            format!("{}::{}", self.kind, self.name)
        }
    }
}

/// `true` when no entry in the slice is a blocking followup
/// (i.e. every entry has `nice_to_have == true`).
pub fn no_blocking_followups(items: &[Followup]) -> bool {
    items.iter().all(|f| f.nice_to_have)
}

/// Same check against a `serde_json::Value` array, for code paths
/// that receive untyped JSON (e.g. the consolidator-output path).
/// A non-array value is treated as "no blocking followups".
pub fn no_blocking_followups_json(value: Option<&serde_json::Value>) -> bool {
    let Some(items) = value.and_then(|v| v.as_array()) else {
        return true;
    };
    !items.iter().any(|item| {
        item.as_object()
            .and_then(|obj| obj.get("nice_to_have"))
            .and_then(serde_json::Value::as_bool)
            .map(|b| !b) // nice_to_have=false → blocking
            .unwrap_or(true) // missing/non-bool → blocking
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let f = Followup {
            kind: "search".into(),
            name: "foo.*bar".into(),
            reason: "[EXTEND] see what calls this".into(),
            path: Some("drivers/net".into()),
            nice_to_have: true,
        };
        let s = serde_json::to_string(&f).unwrap();
        let back: Followup = serde_json::from_str(&s).unwrap();
        assert_eq!(back, f);
    }

    #[test]
    fn nice_to_have_defaults_false_on_legacy_payload() {
        let f: Followup =
            serde_json::from_str(r#"{"type":"source","name":"foo","reason":"why"}"#).unwrap();
        assert!(!f.nice_to_have);
    }

    #[test]
    fn nice_to_have_false_is_omitted_from_wire() {
        let f = Followup {
            kind: "source".into(),
            name: "foo".into(),
            reason: "".into(),
            path: None,
            nice_to_have: false,
        };
        let s = serde_json::to_string(&f).unwrap();
        assert!(!s.contains("nice_to_have"), "serialized: {s}");
    }

    #[test]
    fn no_blocking_followups_recognizes_all_nice_to_have() {
        let items = vec![
            Followup {
                kind: "source".into(),
                name: "a".into(),
                reason: "".into(),
                path: None,
                nice_to_have: true,
            },
            Followup {
                kind: "git".into(),
                name: "log".into(),
                reason: "".into(),
                path: None,
                nice_to_have: true,
            },
        ];
        assert!(no_blocking_followups(&items));
        let mut mixed = items.clone();
        mixed.push(Followup {
            kind: "source".into(),
            name: "b".into(),
            reason: "".into(),
            path: None,
            nice_to_have: false,
        });
        assert!(!no_blocking_followups(&mixed));
    }

    #[test]
    fn no_blocking_followups_json_handles_missing_and_legacy() {
        let none = serde_json::json!([]);
        assert!(no_blocking_followups_json(Some(&none)));
        let legacy = serde_json::json!([{"type":"source","name":"x"}]);
        assert!(!no_blocking_followups_json(Some(&legacy)));
        let nth = serde_json::json!([
            {"type":"source","name":"x","nice_to_have":true},
            {"type":"git","name":"log","nice_to_have":true}
        ]);
        assert!(no_blocking_followups_json(Some(&nth)));
        let mixed = serde_json::json!([
            {"type":"source","name":"x","nice_to_have":true},
            {"type":"source","name":"y","nice_to_have":false}
        ]);
        assert!(!no_blocking_followups_json(Some(&mixed)));
        // No followups field at all — empty.
        assert!(no_blocking_followups_json(None));
    }

    #[test]
    fn cache_key_includes_path_when_present() {
        let f = Followup {
            kind: "search".into(),
            name: "x".into(),
            reason: "".into(),
            path: Some("dir".into()),
            nice_to_have: false,
        };
        assert_eq!(f.cache_key(), "search::x::dir");
        let mut f2 = f.clone();
        f2.path = None;
        assert_eq!(f2.cache_key(), "search::x");
    }
}
