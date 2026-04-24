//! Shrinking helpers for oversized payloads.
//!
//! bugs.md#L5: ignored `previous_findings`
//! size. When the oversize payload was mostly findings, the loop
//! returned `None` and the caller sent the oversize message anyway.
//!
//! This module provides a deterministic, severity-aware trim of a
//! `Vec<Finding>` down to a target char budget. Rule:
//!
//! 1. Always keep High findings.
//! 2. Drop Low, then Medium, until the budget fits.
//! 3. Within the same severity, findings without a
//!    `last_updated_task` label are dropped first (None sorts
//!    before Some(...)), followed by the lowest `last_updated_task`
//!    string in lexicographic order.
//!
//! The estimator is chars/4 ≈ tokens (matches
//! cheap heuristic).

use crate::findings::{Finding, Severity};
use serde_json::Value;

/// Cheap char-based sizing of a JSON value — serializes and takes
/// length. Matches the estimator style used elsewhere in this module.
pub fn json_char_size(v: &Value) -> usize {
    serde_json::to_string(v).map(|s| s.len()).unwrap_or(0)
}

/// Cheap chars/4 → tokens estimate. Matches at
/// — good enough to decide whether to call
/// the exact `messages/count_tokens` endpoint.
pub fn estimate_tokens(total_chars: usize) -> usize {
    total_chars / 4
}

/// Pre-send budget check. Returns `(estimate, within)` where `within`
/// is true if the estimate is within `max_input_tokens`. A 30 % slack
/// is built in: the estimate is deliberately conservative and the
/// caller will pay only the server-reported tokens — so we don't trim
/// until the char estimate exceeds 70 % of the budget, matching
/// = max_input_tokens * 4 * 0.7`.
///
/// Callers should invoke [`shrink_json_list_to_budget`] /
/// [`shrink_findings_to_budget`] on the oversized slots when this
/// returns `false`, then retry the check.
pub fn fit_payload(total_chars: usize, max_input_tokens: usize) -> (usize, bool) {
    let estimate = estimate_tokens(total_chars);
    let threshold = (max_input_tokens as f64 * 0.7) as usize;
    (estimate, estimate <= threshold)
}

/// Trim a slice of JSON values (symbols or context blobs) to fit a
/// char budget. Drops oldest entries first (front of the slice) so
/// the most recently-fetched data is preserved — that's what the
/// current gather loop cares about when the slow agent is about to
/// look at the result.
pub fn shrink_json_list_to_budget(values: &[Value], char_budget: usize) -> Vec<Value> {
    let total: usize = values.iter().map(json_char_size).sum();
    if total <= char_budget {
        return values.to_vec();
    }
    let mut keep_rev: Vec<Value> = Vec::with_capacity(values.len());
    let mut used = 0usize;
    for v in values.iter().rev() {
        let n = json_char_size(v);
        if used.saturating_add(n) > char_budget {
            break;
        }
        used += n;
        keep_rev.push(v.clone());
    }
    keep_rev.reverse();
    keep_rev
}

/// Rough character-based size of a finding's serialised form. Counts
/// summary + mechanism + reproducer + impact + each symbol body.
pub fn finding_char_size(f: &Finding) -> usize {
    let mut n =
        f.title.len() + f.summary.len() + f.reproducer_sketch.len() + f.impact.len() + f.id.len();
    if let Some(ref md) = f.mechanism_detail {
        n += md.len();
    }
    if let Some(ref fx) = f.fix_sketch {
        n += fx.len();
    }
    n += f.open_questions.iter().map(|q| q.len()).sum::<usize>();
    n += f
        .relevant_symbols
        .iter()
        .map(|s| s.name.len() + s.filename.len() + s.definition.len())
        .sum::<usize>();
    n += f
        .relevant_file_sections
        .iter()
        .map(|s| s.filename.len() + s.content.len())
        .sum::<usize>();
    n
}

pub fn total_char_size(findings: &[Finding]) -> usize {
    findings.iter().map(finding_char_size).sum()
}

/// §12: rewrite the last user-message's embedded `symbols` / `context`
/// JSON to fit within `target_chars`. Drops the largest `symbols`
/// entries first (by definition length), then the largest `context`
/// entries, until the estimate fits. Returns the new user-message
/// content string, or `None` if the last message isn't a user turn or
/// doesn't carry a parseable JSON payload with those fields.
///
/// Matches in
/// intent: preserve the outer JSON envelope + the question; trim
/// bodies to fit.
pub fn shrink_last_user_message(content: &str, target_chars: usize) -> Option<String> {
    let mut payload: Value = serde_json::from_str(content).ok()?;
    let obj = payload.as_object_mut()?;
    // Helper: cumulative size of a JSON array.
    let arr_size = |arr: &Vec<Value>| arr.iter().map(json_char_size).sum::<usize>();
    let baseline_without_arrays = {
        let mut clone = Value::Object(obj.clone());
        if let Some(o) = clone.as_object_mut() {
            o.remove("symbols");
            o.remove("context");
        }
        json_char_size(&clone)
    };
    // Compute context size up front so we don't alias obj while we
    // hold a mutable ref on its `symbols` entry.
    let ctx_size_fixed = obj
        .get("context")
        .and_then(|v| v.as_array())
        .map(arr_size)
        .unwrap_or(0);
    // Drop largest symbols first.
    if let Some(syms) = obj.get_mut("symbols").and_then(|v| v.as_array_mut()) {
        let ctx_size = ctx_size_fixed;
        while baseline_without_arrays + arr_size(syms) + ctx_size > target_chars {
            // Find index of largest entry.
            let Some((idx, _)) = syms
                .iter()
                .enumerate()
                .max_by_key(|(_, v)| json_char_size(v))
            else {
                break;
            };
            syms.remove(idx);
            if syms.is_empty() {
                break;
            }
        }
    }
    // Then drop largest context entries.
    let sym_size = obj
        .get("symbols")
        .and_then(|v| v.as_array())
        .map(arr_size)
        .unwrap_or(0);
    if let Some(ctx) = obj.get_mut("context").and_then(|v| v.as_array_mut()) {
        while baseline_without_arrays + sym_size + arr_size(ctx) > target_chars {
            let Some((idx, _)) = ctx
                .iter()
                .enumerate()
                .max_by_key(|(_, v)| json_char_size(v))
            else {
                break;
            };
            ctx.remove(idx);
            if ctx.is_empty() {
                break;
            }
        }
    }
    serde_json::to_string_pretty(&payload).ok()
}

/// Trim `findings` down to fit within `char_budget`. Returns a new
/// Vec in the original ordering, minus the dropped entries.
///
/// When the budget is larger than the current total, `findings` is
/// returned unchanged.
pub fn shrink_findings_to_budget(findings: &[Finding], char_budget: usize) -> Vec<Finding> {
    if total_char_size(findings) <= char_budget {
        return findings.to_vec();
    }
    // Produce a drop order: Low first, then Medium. Keep High
    // always — High is now the top tier, inheriting the "never
    // dropped for budget" protection the retired Critical tier had.
    // Within a tier, oldest last_updated_task first (None counts as
    // "oldest" — no task label attached).
    let mut indexed: Vec<(usize, &Finding)> = findings.iter().enumerate().collect();
    let mut drop_order: Vec<usize> = Vec::new();
    for tier in [Severity::Low, Severity::Medium] {
        let mut in_tier: Vec<&(usize, &Finding)> =
            indexed.iter().filter(|(_, f)| f.severity == tier).collect();
        in_tier.sort_by(|a, b| {
            // None < Some(...), so None sorts first → dropped first.
            a.1.last_updated_task
                .as_deref()
                .cmp(&b.1.last_updated_task.as_deref())
        });
        for (idx, _) in in_tier {
            drop_order.push(*idx);
        }
    }

    let mut dropped: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
    let mut remaining = total_char_size(findings);
    for idx in drop_order {
        if remaining <= char_budget {
            break;
        }
        dropped.insert(idx);
        remaining = remaining.saturating_sub(finding_char_size(&findings[idx]));
    }
    indexed.retain(|(i, _)| !dropped.contains(i));
    indexed.into_iter().map(|(_, f)| f.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::findings::{Finding, Severity, Status};

    fn make(id: &str, sev: Severity, body_len: usize) -> Finding {
        Finding {
            id: id.to_string(),
            title: "t".into(),
            severity: sev,
            status: Status::Active,
            relevant_symbols: vec![],
            relevant_file_sections: vec![],
            summary: "s".repeat(body_len),
            reproducer_sketch: "r".into(),
            impact: "i".into(),
            mechanism_detail: None,
            fix_sketch: None,
            open_questions: vec![],
            first_seen_task: None,
            last_updated_task: None,
            related_finding_ids: vec![],
            reactivate: false,
            details: vec![],
            introduced_by: None,
            first_seen_at: None,
        }
    }

    #[test]
    fn under_budget_returns_unchanged() {
        let f = vec![make("a", Severity::High, 100)];
        let out = shrink_findings_to_budget(&f, 10_000);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn drops_low_before_medium_before_high() {
        // 400 char total. Budget = 250 → must drop at least 150.
        let f = vec![
            make("high1", Severity::High, 100),
            make("med1", Severity::Medium, 100),
            make("low1", Severity::Low, 100),
            make("low2", Severity::Low, 100),
        ];
        let out = shrink_findings_to_budget(&f, 250);
        let ids: Vec<&str> = out.iter().map(|f| f.id.as_str()).collect();
        assert!(ids.contains(&"high1"), "must keep High: {ids:?}");
        // The tier-based walk drops Low first — both Lows go before
        // any Medium is considered.
        assert!(!ids.contains(&"low1") || !ids.contains(&"low2"));
    }

    #[test]
    fn always_keeps_high_even_when_over_budget() {
        // High is now the top tier — the "never drop for budget"
        // protection that used to apply to Critical transferred to
        // High when the Critical tier was retired.
        let f = vec![
            make("big-high", Severity::High, 1_000_000),
            make("low", Severity::Low, 100),
        ];
        let out = shrink_findings_to_budget(&f, 100);
        assert!(out.iter().any(|f| f.id == "big-high"));
    }

    #[test]
    fn preserves_original_order_of_kept_entries() {
        let f = vec![
            make("a", Severity::High, 50),
            make("b", Severity::Low, 50),
            make("c", Severity::High, 50),
        ];
        let out = shrink_findings_to_budget(&f, 120);
        let ids: Vec<&str> = out.iter().map(|f| f.id.as_str()).collect();
        // Low (`b`) gets dropped first; the remaining `a`, `c` keep
        // their relative order.
        assert_eq!(ids, vec!["a", "c"]);
    }

    #[test]
    fn shrink_json_list_under_budget_returns_all() {
        let v = vec![serde_json::json!({"a": 1}), serde_json::json!({"b": 2})];
        let out = shrink_json_list_to_budget(&v, 10_000);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn shrink_json_list_drops_oldest_first() {
        let v = vec![
            serde_json::json!({"old": "x".repeat(100)}),
            serde_json::json!({"new": "y".repeat(100)}),
        ];
        // Budget fits only one of them.
        let out = shrink_json_list_to_budget(&v, 130);
        assert_eq!(out.len(), 1);
        assert!(out[0].get("new").is_some(), "newer entry must be kept");
    }

    #[test]
    fn finding_char_size_covers_optional_fields() {
        let mut f = make("x", Severity::High, 10);
        f.mechanism_detail = Some("md".to_string());
        f.fix_sketch = Some("fx".to_string());
        f.open_questions = vec!["q1".into(), "q2".into()];
        let n = finding_char_size(&f);
        // summary(10) + id(1) + title(1) + repro(1) + impact(1) +
        // md(2) + fx(2) + q1(2) + q2(2) = 22.
        assert_eq!(n, 22);
    }
}
