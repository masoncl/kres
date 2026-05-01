//! Plan: a pre-computed breakdown of the operator's top-level
//! prompt into concrete steps the pipeline intends to execute.
//!
//! A plan is produced after every operator-typed prompt (by
//! `kres_agents::define_plan` running right after `define_goal`)
//! and lives alongside the todo list on the [`crate::TaskManager`].
//! Three other writers can ALSO reshape it while a task runs:
//!   - the first slow-agent call per top-level prompt, when the
//!     operator-typed task has `allow_plan_rewrite=true`;
//!   - the todo-agent, on every completed task — it may return a
//!     rewritten `plan` alongside the updated todo list;
//!   - the goal-not-met todo-agent injection, for the same reason.
//!
//! Linkage is bidirectional. Each [`PlanStep`] carries `todo_ids`
//! pointing DOWN at todos (populated rarely; mainly for tests and
//! persisted pre-step_id state); each [`crate::TodoItem`] carries
//! `step_id` pointing UP at a step (populated by the todo-agent).
//! A step is `Done` once every linked todo is terminal.
//!
//! Plans are persisted into `<results>/session.json` on mutation
//! so a Ctrl-C / `--turns` cap / crash can be resumed on the next
//! invocation pointed at the same results directory.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::mode::TaskMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlanStepStatus {
    Pending,
    InProgress,
    Done,
    Skipped,
}

impl PlanStepStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done | Self::Skipped)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStep {
    /// Stable kebab-case slug id (e.g. "audit-ring-buffer-init").
    /// Defaulted on deserialize so a forgotten `id` in an LLM
    /// reply does not fail the whole rewrite; `normalize_steps`
    /// synthesises a slug from `title` when this is empty.
    #[serde(default)]
    pub id: String,
    /// Short imperative title ("audit release path in foo()").
    /// Defaulted on deserialize for the same reason; rows with
    /// empty titles are filtered by `normalize_steps`.
    #[serde(default)]
    pub title: String,
    /// Free-form prose describing what success looks like for this
    /// step. Consumed by the goal judge.
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_pending")]
    pub status: PlanStepStatus,
    /// IDs (or names, when id is empty) of the todo items that
    /// execute this step. A step flips to `Done` when every linked
    /// todo is terminal.
    #[serde(default)]
    pub todo_ids: Vec<String>,
    /// Additional context injected into the prompt for todos
    /// linked to this step. When non-empty, the dispatch path
    /// prepends it to the derived task's prompt so the slow agent
    /// sees the step's protocol (e.g. review lenses) alongside
    /// the coding system prompt. Complements `description` (which
    /// rides in the plan JSON) by putting the protocol directly
    /// in the question where it's unmissable.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub context: String,
}

fn default_pending() -> PlanStepStatus {
    PlanStepStatus::Pending
}

impl PlanStep {
    pub fn new(id: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            description: String::new(),
            status: PlanStepStatus::Pending,
            todo_ids: Vec::new(),
            context: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    /// The operator's raw prompt that produced this plan. Stored so
    /// a resumed session can reconstruct context without re-prompting.
    pub prompt: String,
    /// The goal-judge's completion criterion (define_goal output).
    pub goal: String,
    pub mode: TaskMode,
    #[serde(default)]
    pub steps: Vec<PlanStep>,
    pub created_at: DateTime<Utc>,
}

/// Wire-format for rewrites emitted by the slow agent or the
/// todo agent. LLMs forget fields all the time; accepting only
/// `{steps: [...]}` means the plan's identifying metadata
/// (`prompt`, `goal`, `mode`, `created_at`) cannot be accidentally
/// clobbered. The caller merges a `PlanRewrite` with the existing
/// plan via `apply_to`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct PlanRewrite {
    #[serde(default)]
    pub steps: Vec<PlanStep>,
}

impl PlanRewrite {
    /// Build a full `Plan` by taking the rewrite's steps and
    /// inheriting identifying metadata from `prior`. When `prior`
    /// is `None` (rewrite received with no current plan — should
    /// not happen in the normal flow, but defensive), returns a
    /// Plan with empty prompt / goal and a fresh timestamp.
    ///
    /// The rewrite's steps are passed through `normalize_steps`
    /// before being placed on the Plan: empty-title rows filtered,
    /// missing or duplicate ids replaced with a slug derived from
    /// the title. The LLM cannot corrupt the plan's step-id
    /// invariants no matter how sloppy its reply is.
    pub fn apply_to(self, prior: Option<&Plan>) -> Plan {
        let mut steps = normalize_steps(self.steps);
        // Carry forward template-authored `context` from prior steps
        // with matching ids. The context field is set by embedded
        // plan templates (fix-template.md) and is not something
        // agents produce, so a rewrite that omits it should inherit
        // rather than silently drop.
        if let Some(p) = &prior {
            for step in &mut steps {
                if step.context.is_empty() {
                    if let Some(prior_step) = p.steps.iter().find(|s| s.id == step.id) {
                        if !prior_step.context.is_empty() {
                            step.context.clone_from(&prior_step.context);
                        }
                    }
                }
            }
        }
        match prior {
            Some(p) => Plan {
                prompt: p.prompt.clone(),
                goal: p.goal.clone(),
                mode: p.mode,
                steps,
                created_at: p.created_at,
            },
            None => Plan {
                prompt: String::new(),
                goal: String::new(),
                mode: TaskMode::default(),
                steps,
                created_at: Utc::now(),
            },
        }
    }
}

/// Filter empty-title rows and synthesise any missing or collided
/// step ids from a kebab-case slug of the title. Runs the same
/// invariants the `define_plan` path enforces so rewrite replies
/// from the slow agent or todo agent cannot produce a plan with
/// empty ids, duplicate ids, or titleless rows.
///
/// - Empty title → filtered. Never survives to the Plan.
/// - Empty id OR collision with an earlier row → synthesise from
///   `slugify_step_id(title)`, then walk `-2`, `-3`, … to find a
///   free slot. Titleless slug falls back to `step-<N>` where N
///   is the 1-based position among kept rows.
/// - Non-empty unique id → kept verbatim. This preserves operator
///   or planner intent when the LLM cooperated.
pub fn normalize_steps(steps: Vec<PlanStep>) -> Vec<PlanStep> {
    let mut out: Vec<PlanStep> = Vec::with_capacity(steps.len());
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut synth_n: usize = 0;
    for mut s in steps.into_iter() {
        let title = s.title.trim().to_string();
        if title.is_empty() {
            continue;
        }
        s.title = title;
        synth_n += 1;
        let id = s.id.trim().to_string();
        s.id = if !id.is_empty() && seen.insert(id.clone()) {
            id
        } else {
            let base = slugify_step_id(&s.title);
            let base = if base.is_empty() {
                format!("step-{synth_n}")
            } else {
                base
            };
            let mut candidate = base.clone();
            let mut suffix = 1u32;
            while !seen.insert(candidate.clone()) {
                suffix += 1;
                candidate = format!("{base}-{suffix}");
            }
            candidate
        };
        out.push(s);
    }
    out
}

/// Extract an embedded plan from prompt text. A template can include
/// a `PLAN:` block with a JSON plan; when found, the block is stripped
/// from the text and the steps are returned. On parse failure or
/// missing marker, returns the original text and None.
pub fn extract_embedded_plan(text: &str) -> (String, Option<Vec<PlanStep>>) {
    let marker_pos = text
        .lines()
        .enumerate()
        .find(|(_, line)| line.trim().starts_with("PLAN:"))
        .map(|(i, _)| i);
    let Some(marker_line) = marker_pos else {
        return (text.to_string(), None);
    };

    let lines: Vec<&str> = text.lines().collect();
    let after_marker = lines[marker_line]
        .trim()
        .strip_prefix("PLAN:")
        .unwrap()
        .trim();

    let json_start_line;
    let json_text_start;
    if after_marker.starts_with('{') {
        json_start_line = marker_line;
        json_text_start = text.find("PLAN:").unwrap() + "PLAN:".len();
        let rest = &text[json_text_start..];
        let trimmed_offset = rest.len() - rest.trim_start().len();
        let json_text_start = json_text_start + trimmed_offset;
        let _ = json_text_start; // used below
    } else {
        json_start_line = marker_line + 1;
    }
    let _ = json_start_line;

    // Find the JSON by scanning for balanced braces from the first '{' after PLAN:
    let plan_marker = text.find("PLAN:").unwrap();
    let rest = &text[plan_marker + "PLAN:".len()..];
    let brace_offset = match rest.find('{') {
        Some(o) => o,
        None => return (text.to_string(), None),
    };
    let json_start = plan_marker + "PLAN:".len() + brace_offset;
    let mut depth = 0i32;
    let mut json_end = json_start;
    for (i, ch) in text[json_start..].char_indices() {
        match ch {
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth -= 1;
                if depth == 0 {
                    json_end = json_start + i + ch.len_utf8();
                    break;
                }
            }
            _ => {}
        }
    }
    if depth != 0 {
        tracing::warn!(
            target: "kres_core",
            "PLAN: block has unbalanced braces; ignoring embedded plan"
        );
        return (text.to_string(), None);
    }

    let json_str = &text[json_start..json_end];
    let parsed: Result<PlanRewrite, _> = serde_json::from_str(json_str);
    match parsed {
        Ok(rewrite) if !rewrite.steps.is_empty() => {
            // Strip the PLAN: line and the JSON block from the text.
            // Find the full block boundaries (PLAN: marker through end of JSON).
            let block_start = plan_marker;
            let mut block_end = json_end;
            // Consume trailing whitespace/newlines after the JSON.
            while block_end < text.len()
                && text
                    .as_bytes()
                    .get(block_end)
                    .is_some_and(|b| b.is_ascii_whitespace())
            {
                block_end += 1;
            }
            let mut stripped = String::with_capacity(text.len());
            stripped.push_str(&text[..block_start]);
            stripped.push_str(&text[block_end..]);
            // Trim trailing whitespace from the join point.
            let stripped = stripped.trim_end().to_string();
            (stripped, Some(rewrite.steps))
        }
        Ok(_) => {
            tracing::warn!(
                target: "kres_core",
                "PLAN: block parsed but contains no steps; ignoring"
            );
            (text.to_string(), None)
        }
        Err(e) => {
            tracing::warn!(
                target: "kres_core",
                "PLAN: block JSON parse failed: {e}; ignoring embedded plan"
            );
            (text.to_string(), None)
        }
    }
}

/// Produce a kebab-case slug from a step title, truncated to 60
/// chars. Keeps ASCII letters / digits and collapses everything
/// else into single `-` separators; strips leading/trailing `-`;
/// lowercases. Returns an empty string when the title contains
/// no slug-able characters — callers fall back to `step-<N>` in
/// that case.
pub fn slugify_step_id(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut last_dash = true; // suppress leading `-`
    for ch in title.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
        if out.len() >= 60 {
            break;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

impl Plan {
    pub fn new(prompt: impl Into<String>, goal: impl Into<String>, mode: TaskMode) -> Self {
        Self {
            prompt: prompt.into(),
            goal: goal.into(),
            mode,
            steps: Vec::new(),
            created_at: Utc::now(),
        }
    }

    /// Look up the context for a step by id. Returns an empty
    /// string when the step doesn't exist or has no context.
    pub fn step_context(&self, step_id: &str) -> &str {
        self.steps
            .iter()
            .find(|s| s.id == step_id)
            .map(|s| s.context.as_str())
            .unwrap_or("")
    }

    /// Flip a step's status by id.
    pub fn mark_step(&mut self, id: &str, status: PlanStepStatus) -> bool {
        if let Some(s) = self.steps.iter_mut().find(|s| s.id == id) {
            s.status = status;
            true
        } else {
            false
        }
    }

    /// Recompute step statuses from a linked todo list. This is a
    /// full rederive — not a one-way "promote only" update — because
    /// the TaskManager drain path can flip InProgress todos back to
    /// Pending (ctrl-c, --turns cap); a step whose todos have all
    /// regressed to Pending must likewise regress from InProgress
    /// back to Pending so the plan does not lie about what is still
    /// running.
    ///
    /// Linkage direction: a todo points UP at a plan step via
    /// `TodoItem.step_id`; the step can also point DOWN at todo ids
    /// via `PlanStep.todo_ids`. This function accepts either. The
    /// `step_id` direction is easier for the todo-agent to populate
    /// (one field per emitted todo) and the preferred mechanism
    /// going forward; `todo_ids` stays as a compatibility path for
    /// plans that carry pre-populated links (tests, persisted
    /// pre-step_id state).
    ///
    /// Rules, in order of precedence:
    ///   - step is already terminal (`Done`/`Skipped`) → leave alone
    ///   - no linkage resolves to any todo → leave alone (planner
    ///     hasn't wired up the links yet)
    ///   - every linked todo is terminal → `Done`
    ///   - any linked todo is `InProgress` → `InProgress`
    ///   - otherwise → `Pending`
    pub fn sync_from_todo(&mut self, todo: &[crate::TodoItem]) {
        for step in self.steps.iter_mut() {
            if step.status.is_terminal() {
                continue;
            }
            // Collect linked todos via step_id first (todo → step);
            // then union with whatever `step.todo_ids` claims, so
            // both linkage directions contribute. Dedupe by todo
            // pointer identity using the index, since a todo can
            // only appear once in the input slice.
            let mut linked_idx: std::collections::BTreeSet<usize> =
                std::collections::BTreeSet::new();
            for (n, i) in todo.iter().enumerate() {
                if !i.step_id.is_empty() && i.step_id == step.id {
                    linked_idx.insert(n);
                }
            }
            for tid in &step.todo_ids {
                if let Some(n) = todo
                    .iter()
                    .position(|i| (!i.id.is_empty() && i.id == *tid) || i.name == *tid)
                {
                    linked_idx.insert(n);
                }
            }
            if linked_idx.is_empty() {
                continue;
            }
            let linked: Vec<&crate::TodoItem> = linked_idx.iter().map(|n| &todo[*n]).collect();
            let all_terminal = linked.iter().all(|i| {
                matches!(
                    i.status,
                    crate::TodoStatus::Done | crate::TodoStatus::Skipped
                )
            });
            if all_terminal {
                step.status = PlanStepStatus::Done;
                continue;
            }
            let any_inprogress = linked
                .iter()
                .any(|i| i.status == crate::TodoStatus::InProgress);
            step.status = if any_inprogress {
                PlanStepStatus::InProgress
            } else {
                PlanStepStatus::Pending
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::todo::{TodoItem, TodoStatus};

    #[test]
    fn plan_serde_roundtrip() {
        let mut p = Plan::new("review foo", "every fn audited", TaskMode::Audit);
        p.steps.push(PlanStep::new("s1", "audit foo()"));
        p.steps[0].todo_ids.push("t1".into());
        let s = serde_json::to_string(&p).unwrap();
        let back: Plan = serde_json::from_str(&s).unwrap();
        assert_eq!(back.prompt, "review foo");
        assert_eq!(back.steps.len(), 1);
        assert_eq!(back.steps[0].status, PlanStepStatus::Pending);
    }

    #[test]
    fn step_status_terminal() {
        assert!(PlanStepStatus::Done.is_terminal());
        assert!(PlanStepStatus::Skipped.is_terminal());
        assert!(!PlanStepStatus::Pending.is_terminal());
        assert!(!PlanStepStatus::InProgress.is_terminal());
    }

    #[test]
    fn sync_from_todo_marks_done_when_all_linked_terminal() {
        let mut p = Plan::new("p", "g", TaskMode::Audit);
        let mut step = PlanStep::new("s1", "one");
        step.todo_ids = vec!["a".into(), "b".into()];
        p.steps.push(step);
        let mut a = TodoItem::new("a", "investigate");
        a.status = TodoStatus::Done;
        let mut b = TodoItem::new("b", "investigate");
        b.status = TodoStatus::Skipped;
        p.sync_from_todo(&[a, b]);
        assert_eq!(p.steps[0].status, PlanStepStatus::Done);
    }

    #[test]
    fn sync_from_todo_inprogress_when_any_running() {
        let mut p = Plan::new("p", "g", TaskMode::Audit);
        let mut step = PlanStep::new("s1", "one");
        step.todo_ids = vec!["a".into(), "b".into()];
        p.steps.push(step);
        let mut a = TodoItem::new("a", "investigate");
        a.status = TodoStatus::InProgress;
        let b = TodoItem::new("b", "investigate");
        p.sync_from_todo(&[a, b]);
        assert_eq!(p.steps[0].status, PlanStepStatus::InProgress);
    }

    #[test]
    fn sync_from_todo_leaves_pending_alone() {
        let mut p = Plan::new("p", "g", TaskMode::Audit);
        let mut step = PlanStep::new("s1", "one");
        step.todo_ids = vec!["a".into()];
        p.steps.push(step);
        let a = TodoItem::new("a", "investigate");
        p.sync_from_todo(&[a]);
        assert_eq!(p.steps[0].status, PlanStepStatus::Pending);
    }

    #[test]
    fn sync_from_todo_regresses_stale_inprogress_to_pending() {
        // After TaskManager::reset_in_progress_to_pending flips the
        // linked todos back to Pending, a step that was InProgress
        // must also regress — otherwise the live plan lies about
        // what is still running.
        let mut p = Plan::new("p", "g", TaskMode::Audit);
        let mut step = PlanStep::new("s1", "one");
        step.status = PlanStepStatus::InProgress;
        step.todo_ids = vec!["a".into()];
        p.steps.push(step);
        let a = TodoItem::new("a", "investigate"); // default Pending
        p.sync_from_todo(&[a]);
        assert_eq!(p.steps[0].status, PlanStepStatus::Pending);
    }

    #[test]
    fn sync_from_todo_links_via_step_id() {
        // New linkage direction: todo.step_id points up at the plan
        // step. sync_from_todo must find the linked todo without any
        // entry in step.todo_ids.
        let mut p = Plan::new("p", "g", TaskMode::Audit);
        p.steps.push(PlanStep::new("s1", "audit foo"));
        let mut t = TodoItem::new("audit-foo", "investigate");
        t.step_id = "s1".into();
        t.status = TodoStatus::Done;
        p.sync_from_todo(&[t]);
        assert_eq!(p.steps[0].status, PlanStepStatus::Done);
    }

    #[test]
    fn sync_from_todo_unions_step_id_and_todo_ids() {
        // Both linkage directions must contribute. Step.todo_ids
        // claims todo "a"; todo "b" points back via step_id. Step
        // is Done only when BOTH reach terminal status.
        let mut p = Plan::new("p", "g", TaskMode::Audit);
        let mut step = PlanStep::new("s1", "audit");
        step.todo_ids = vec!["a".into()];
        p.steps.push(step);
        let mut a = TodoItem::new("a", "investigate");
        a.status = TodoStatus::Done;
        let mut b = TodoItem::new("b", "investigate");
        b.step_id = "s1".into();
        b.status = TodoStatus::InProgress;
        p.sync_from_todo(&[a, b]);
        assert_eq!(p.steps[0].status, PlanStepStatus::InProgress);
    }

    #[test]
    fn slugify_step_id_samples() {
        assert_eq!(
            slugify_step_id("Audit ring buffer init"),
            "audit-ring-buffer-init"
        );
        assert_eq!(
            slugify_step_id("Walk io_uring/fs.c fault paths"),
            "walk-io-uring-fs-c-fault-paths"
        );
        assert_eq!(slugify_step_id("  ---  "), "");
        assert_eq!(slugify_step_id("one"), "one");
    }

    fn step(id: &str, title: &str) -> PlanStep {
        PlanStep::new(id, title)
    }

    #[test]
    fn normalize_steps_filters_empty_titles() {
        let out = normalize_steps(vec![step("keep-id", ""), step("", "Kept title")]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "kept-title");
    }

    #[test]
    fn normalize_steps_synthesises_and_dedup_ids() {
        let out = normalize_steps(vec![
            step("", "Audit foo"),
            step("", "Audit foo"),
            step("audit-foo", "Audit bar"),
        ]);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].id, "audit-foo");
        // Second row with empty id slugs to "audit-foo" which is
        // taken, so walks to "audit-foo-2".
        assert_eq!(out[1].id, "audit-foo-2");
        // Third row has a non-empty id "audit-foo" but that's now
        // taken; slugs to "audit-bar" which is free.
        assert_eq!(out[2].id, "audit-bar");
    }

    #[test]
    fn normalize_steps_titleless_slug_falls_back_to_step_n() {
        let out = normalize_steps(vec![step("", "!!!"), step("", "@@@")]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, "step-1");
        assert_eq!(out[1].id, "step-2");
    }

    #[test]
    fn plan_step_deserialises_with_missing_id_and_title() {
        // Regression guard: when an LLM forgets fields the step
        // still parses — earlier behaviour was a hard fail that
        // silently dropped the whole rewrite.
        let s: PlanStep = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(s.id, "");
        assert_eq!(s.title, "");
        assert_eq!(s.status, PlanStepStatus::Pending);
        assert!(s.context.is_empty());
    }

    #[test]
    fn step_context_serde_roundtrip() {
        let mut s = PlanStep::new("review", "Review the patch");
        s.context = "Apply review lenses.".into();
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("Apply review lenses."));
        let back: PlanStep = serde_json::from_str(&json).unwrap();
        assert_eq!(back.context, "Apply review lenses.");
    }

    #[test]
    fn step_context_omitted_when_empty() {
        let s = PlanStep::new("write", "Write the fix");
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("context"));
    }

    #[test]
    fn plan_step_context_returns_text() {
        let mut p = Plan::new("fix", "fix the bug", TaskMode::Coding);
        let mut review = PlanStep::new("review-patch", "Review");
        review.context = "Apply lenses.".into();
        p.steps.push(PlanStep::new("write-fix", "Write"));
        p.steps.push(review);
        assert_eq!(p.step_context("review-patch"), "Apply lenses.");
        assert_eq!(p.step_context("write-fix"), "");
        assert_eq!(p.step_context("nonexistent"), "");
    }

    #[test]
    fn apply_to_inherits_prior_metadata_and_normalises_steps() {
        let prior = Plan::new("review fs", "find bugs", TaskMode::Audit);
        // The rewrite forgot the id on one step and left a title
        // blank on another — without normalisation, apply_to would
        // land a broken plan.
        let rewrite = PlanRewrite {
            steps: vec![step("", "Audit foo"), step("bad", "")],
        };
        let built = rewrite.apply_to(Some(&prior));
        assert_eq!(built.prompt, "review fs");
        assert_eq!(built.goal, "find bugs");
        assert_eq!(built.mode, TaskMode::Audit);
        assert_eq!(built.steps.len(), 1);
        assert_eq!(built.steps[0].id, "audit-foo");
    }

    #[test]
    fn apply_to_carries_forward_context_from_matching_prior_steps() {
        let mut prior = Plan::new("fix", "fix bug", TaskMode::Coding);
        let mut review = PlanStep::new("review-patch", "Review");
        review.context = "REVIEW PROTOCOL\nlens checklist here".into();
        prior.steps.push(PlanStep::new("write-fix", "Write"));
        prior.steps.push(review);
        // Rewrite keeps the same step ids but omits context (as an
        // LLM rewrite would).
        let rewrite = PlanRewrite {
            steps: vec![
                step("write-fix", "Write the fix"),
                step("review-patch", "Review the patch"),
            ],
        };
        let built = rewrite.apply_to(Some(&prior));
        assert_eq!(built.steps.len(), 2);
        assert!(built.steps[0].context.is_empty());
        assert_eq!(
            built.steps[1].context,
            "REVIEW PROTOCOL\nlens checklist here"
        );
    }

    #[test]
    fn apply_to_does_not_override_explicit_context() {
        let mut prior = Plan::new("fix", "fix bug", TaskMode::Coding);
        let mut review = PlanStep::new("review-patch", "Review");
        review.context = "old context".into();
        prior.steps.push(review);
        let mut rewrite_step = step("review-patch", "Review v2");
        rewrite_step.context = "new context".into();
        let rewrite = PlanRewrite {
            steps: vec![rewrite_step],
        };
        let built = rewrite.apply_to(Some(&prior));
        assert_eq!(built.steps[0].context, "new context");
    }

    #[test]
    fn apply_to_with_no_prior_produces_default_metadata() {
        let rewrite = PlanRewrite {
            steps: vec![step("", "Only step")],
        };
        let built = rewrite.apply_to(None);
        assert!(built.prompt.is_empty());
        assert!(built.goal.is_empty());
        assert_eq!(built.mode, TaskMode::default());
        assert_eq!(built.steps.len(), 1);
    }

    #[test]
    fn sync_from_todo_skips_terminal_steps() {
        let mut p = Plan::new("p", "g", TaskMode::Audit);
        let mut step = PlanStep::new("s1", "one");
        step.status = PlanStepStatus::Skipped;
        step.todo_ids = vec!["a".into()];
        p.steps.push(step);
        let mut a = TodoItem::new("a", "investigate");
        a.status = TodoStatus::InProgress;
        p.sync_from_todo(&[a]);
        assert_eq!(p.steps[0].status, PlanStepStatus::Skipped);
    }

    #[test]
    fn extract_embedded_plan_parses_valid_block() {
        let text = r#"Some preamble text.

PLAN:
{"steps": [
  {"id": "research", "title": "Research the bug"},
  {"id": "write-fix", "title": "Write the fix"}
]}

More text after."#;
        let (stripped, steps) = extract_embedded_plan(text);
        let steps = steps.expect("should parse");
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].id, "research");
        assert_eq!(steps[1].title, "Write the fix");
        assert!(stripped.contains("Some preamble text."));
        assert!(stripped.contains("More text after."));
        assert!(!stripped.contains("PLAN:"));
    }

    #[test]
    fn extract_embedded_plan_no_marker() {
        let text = "Just a normal prompt with no plan.";
        let (stripped, steps) = extract_embedded_plan(text);
        assert!(steps.is_none());
        assert_eq!(stripped, text);
    }

    #[test]
    fn extract_embedded_plan_bad_json() {
        let text = "Before\n\nPLAN:\n{not valid json\n\nAfter";
        let (stripped, steps) = extract_embedded_plan(text);
        assert!(steps.is_none());
        assert_eq!(stripped, text);
    }

    #[test]
    fn extract_embedded_plan_inline_json() {
        let text = r#"PLAN: {"steps": [{"id": "s1", "title": "Do it"}]}
Rest of prompt."#;
        let (stripped, steps) = extract_embedded_plan(text);
        let steps = steps.expect("should parse");
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].id, "s1");
        assert!(stripped.contains("Rest of prompt."));
        assert!(!stripped.contains("PLAN:"));
    }

    #[test]
    fn extract_embedded_plan_with_step_context() {
        let text = r#"Preamble.

PLAN:
{"steps": [
  {"id": "write-fix", "title": "Write the fix"},
  {"id": "review-patch", "title": "Review the patch", "context": "Apply review lenses."}
]}

Tail."#;
        let (stripped, steps) = extract_embedded_plan(text);
        let steps = steps.expect("should parse");
        assert_eq!(steps.len(), 2);
        assert!(steps[0].context.is_empty());
        assert_eq!(steps[1].context, "Apply review lenses.");
        assert!(!stripped.contains("PLAN:"));
        assert!(stripped.contains("Tail."));
    }

    #[test]
    fn extract_embedded_plan_indented_marker() {
        let text = r#"Preamble.

   PLAN:
   {"steps": [{"id": "x", "title": "Step X"}]}

Tail."#;
        let (stripped, steps) = extract_embedded_plan(text);
        let steps = steps.expect("should parse");
        assert_eq!(steps.len(), 1);
        assert!(!stripped.contains("PLAN:"));
    }
}
