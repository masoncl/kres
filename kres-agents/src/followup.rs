//! Followup types the agents use to request data.
//!
//! The shape matches the existing wire format. We accept the
//! minor variations already handles (e.g.
//! `file` vs `path` aliases) so old agent prompts continue to
//! interoperate.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Followup {
    /// "survey", "source", "type", "callers", "callees", "search", "file",
    /// "read", "git", "question".
    #[serde(rename = "type")]
    pub kind: String,
    /// What to fetch: a symbol name, a regex, a path, etc.
    pub name: String,
    pub reason: String,
    /// Optional scoping path for search/file types.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// `true` (default) marks a blocking evidence request: the
    /// emitting agent cannot reach a terminal answer without it.
    /// `false` marks a deferred audit — worth doing, but the current
    /// verdict does not depend on it. Workflow evals that require "no
    /// remaining followups" before declaring a terminal status only
    /// count entries with `required_for_progress == true`.
    ///
    /// Defaults to `true` so an agent that omits the field is treated
    /// as blocked rather than satisfied. The conservative direction is
    /// the safe one: a spurious extra gather round costs tokens, while
    /// a wrongly-cleared followup lets a step declare a terminal
    /// status on evidence it never obtained.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub required_for_progress: bool,
}

fn default_true() -> bool {
    true
}

fn is_true(b: &bool) -> bool {
    *b
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
/// (i.e. no entry has `required_for_progress == true`).
pub fn no_blocking_followups(items: &[Followup]) -> bool {
    items.iter().all(|f| !f.required_for_progress)
}

/// How many times one task may requeue itself to fetch evidence a
/// slow agent asked for and did not have.
///
/// A requeue is not a retry: the response was valid and the task keeps
/// its dispatch slot (the semaphore permit in `TaskManager::spawn` is
/// held for the whole of `work`, so nothing else can claim it). What
/// repeats is fetch-then-analyse, with the newly fetched evidence
/// appended to the same gather.
///
/// Three is a budget, not a target. Each round costs one fetch plus a
/// re-analysis, and a chain that is still unresolved after three hops
/// is better expressed as a followup for a future task than paid for
/// again inside this one.
pub const MAX_TASK_REQUEUES: u32 = 3;

/// `true` when this task is the plan's opening step, which must not
/// requeue.
///
/// The opening step builds the map the rest of the plan is written
/// against, and later steps depend on it, so its cost is fully serial
/// while every other task's is paid in parallel. On the 2026-08-19
/// arch/x86/kvm/mmu review it was the only task to run in the first
/// nineteen minutes: two requeues, each a gather plus a five-lens
/// fan-out, with the whole run waiting behind it. The evidence a
/// requeue would add there is evidence the parallel analysis steps
/// fetch for themselves anyway.
///
/// The test is positional, not textual. A plan step id is
/// model-generated prose; matching on what it says would be the
/// substring-classifier AGENTS.md prohibits. Being first in
/// `plan.steps` is a fact about the plan's structure.
pub fn is_opening_plan_step(plan: Option<&kres_core::Plan>, active_step_id: Option<&str>) -> bool {
    let (Some(plan), Some(active)) = (plan, active_step_id) else {
        return false;
    };
    plan.steps.first().is_some_and(|first| first.id == active)
}

/// The evidence a slow agent said it needed and did not get.
///
/// Selection is entirely from typed fields — never from prose:
///
/// * `required_for_progress` — the declared contract for "the emitting
///   agent cannot reach a terminal answer without this". A deferred
///   audit does not stall a task that already produced valid output.
/// * the kind must name something fetchable. `question` is addressed
///   to a human or a later task and no fetcher can satisfy it, so a
///   task that only asked questions must not spin.
/// * not already fetched. `seen` carries the keys this task has
///   requested so far, so a request repeated verbatim after it was
///   served cannot drive another round.
///
/// Returns every qualifying request, in emitted order, deduplicated.
///
/// There is deliberately no cap on how many a round may fetch. A round
/// costs one gather plus one full lens fan-out; the fetches themselves
/// are main-agent/semcode work, so bounding them saves almost nothing
/// and starves the mechanism of the evidence it exists to deliver. The
/// bound that matters is [`MAX_TASK_REQUEUES`], on rounds.
///
/// A cap of three was tried and removed. It capped the cheap dimension
/// — an ordinary gather in the same run fetched 148 distinct symbols —
/// and because the caller pools every lens's requests before selecting,
/// truncation handed all three slots to whichever lens came first. On
/// the 2026-08-19 arch/x86/kvm/mmu review the `general` lens twice named
/// `__kvm_mmu_prepare_zap_page` as its FIRST request and lost both times
/// to `memory-lifetime`'s list, so the one body the analysis was missing
/// was never fetched across nine separate blocking requests.
pub fn requeue_evidence_requests(items: &[Followup], seen: &mut HashSet<String>) -> Vec<Followup> {
    items
        .iter()
        .filter(|f| f.required_for_progress)
        .filter(|f| f.kind != "question")
        .filter(|f| !f.name.trim().is_empty())
        .filter(|f| seen.insert(f.cache_key()))
        .cloned()
        .collect()
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
            .and_then(|obj| obj.get("required_for_progress"))
            .and_then(serde_json::Value::as_bool)
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
            required_for_progress: false,
        };
        let s = serde_json::to_string(&f).unwrap();
        let back: Followup = serde_json::from_str(&s).unwrap();
        assert_eq!(back, f);
    }

    #[test]
    fn required_for_progress_defaults_true_on_legacy_payload() {
        let f: Followup =
            serde_json::from_str(r#"{"type":"source","name":"foo","reason":"why"}"#).unwrap();
        // An agent that omits the field is blocked, not satisfied.
        assert!(f.required_for_progress);
    }

    #[test]
    fn reason_is_required_by_wire_schema() {
        assert!(serde_json::from_str::<Followup>(r#"{"type":"source","name":"foo"}"#).is_err());
        let schema = serde_json::to_value(schemars::schema_for!(Followup)).unwrap();
        assert!(schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|field| field == "reason"));
    }

    #[test]
    fn required_for_progress_true_is_omitted_from_wire() {
        let f = Followup {
            kind: "source".into(),
            name: "foo".into(),
            reason: "".into(),
            path: None,
            required_for_progress: true,
        };
        let s = serde_json::to_string(&f).unwrap();
        assert!(!s.contains("required_for_progress"), "serialized: {s}");
    }

    #[test]
    fn no_blocking_followups_recognizes_all_deferred() {
        let items = vec![
            Followup {
                kind: "source".into(),
                name: "a".into(),
                reason: "".into(),
                path: None,
                required_for_progress: false,
            },
            Followup {
                kind: "git".into(),
                name: "log".into(),
                reason: "".into(),
                path: None,
                required_for_progress: false,
            },
        ];
        assert!(no_blocking_followups(&items));
        let mut mixed = items.clone();
        mixed.push(Followup {
            kind: "source".into(),
            name: "b".into(),
            reason: "".into(),
            path: None,
            required_for_progress: true,
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
            {"type":"source","name":"x","required_for_progress":false},
            {"type":"git","name":"log","required_for_progress":false}
        ]);
        assert!(no_blocking_followups_json(Some(&nth)));
        let mixed = serde_json::json!([
            {"type":"source","name":"x","required_for_progress":false},
            {"type":"source","name":"y","required_for_progress":true}
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
            required_for_progress: true,
        };
        assert_eq!(f.cache_key(), "search::x::dir");
        let mut f2 = f.clone();
        f2.path = None;
        assert_eq!(f2.cache_key(), "search::x");
    }

    fn fu(kind: &str, name: &str, required: bool) -> Followup {
        Followup {
            kind: kind.into(),
            name: name.into(),
            reason: String::new(),
            path: None,
            required_for_progress: required,
        }
    }

    #[test]
    fn requeue_selects_only_blocking_fetchable_requests() {
        let items = vec![
            fu("source", "__kvm_mmu_prepare_zap_page", true),
            fu("read", "mm/x.c:10+20", true),
            // Deferred audits must not stall a task that already
            // produced valid output.
            fu("source", "deferred_extra", false),
            // No fetcher can satisfy a question; a task that asked
            // only questions must not spin.
            fu("question", "is this reachable?", true),
            fu("source", "   ", true),
        ];
        let mut seen = HashSet::new();
        let got = requeue_evidence_requests(&items, &mut seen);
        let names: Vec<&str> = got.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["__kvm_mmu_prepare_zap_page", "mm/x.c:10+20"]);
    }

    fn plan_with_steps(ids: &[&str]) -> kres_core::Plan {
        kres_core::Plan {
            prompt: String::new(),
            goal: String::new(),
            mode: kres_core::TaskMode::Audit,
            steps: ids
                .iter()
                .map(|id| kres_core::PlanStep::new(*id, "t"))
                .collect(),
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn only_the_plans_first_step_is_exempt_from_requeue() {
        let plan = plan_with_steps(&["inventory-sources", "audit-a", "audit-b"]);
        assert!(is_opening_plan_step(Some(&plan), Some("inventory-sources")));
        assert!(!is_opening_plan_step(Some(&plan), Some("audit-a")));
        // A task with no plan, or none active, is ordinary work.
        assert!(!is_opening_plan_step(None, Some("inventory-sources")));
        assert!(!is_opening_plan_step(Some(&plan), None));
    }

    #[test]
    fn requeue_takes_every_request_no_matter_how_many() {
        // The caller pools every lens's requests into one list. Any cap
        // here is applied after that concatenation, so it hands the
        // whole budget to whichever lens came first and starves the
        // rest — which is how the one body an analysis needed went
        // unfetched across nine blocking requests.
        let items: Vec<Followup> = (0..40)
            .map(|i| fu("source", &format!("sym{i}"), true))
            .collect();
        let mut seen = HashSet::new();
        let got = requeue_evidence_requests(&items, &mut seen);
        assert_eq!(got.len(), 40);
        assert_eq!(got.last().unwrap().name, "sym39");
    }

    #[test]
    fn requeue_does_not_refetch_a_request_already_served() {
        let items = vec![fu("source", "make_mmu_pages_available", true)];
        let mut seen = HashSet::new();
        assert_eq!(requeue_evidence_requests(&items, &mut seen).len(), 1);
        // Re-asking for the same evidence cannot drive another round:
        // that is how a lens that never accepts an answer would burn
        // the whole budget.
        assert!(requeue_evidence_requests(&items, &mut seen).is_empty());
    }
}
