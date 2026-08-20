//! Findings records and the delta-based store.
//!
//! Historically each turn rewrote the whole findings list after an
//! LLM-based merge pass. That wastes tokens (the merge prompt carries
//! the full prior list) and disk (a `findings-N.json` snapshot per
//! turn). The new model:
//!
//! - Slow agents (and every other inference call that emits findings)
//!   produce a `findings` array that is interpreted as a DELTA:
//!   matching-id entries update an existing finding, new ids add,
//!   `status: invalidated` on an existing id marks it, and obvious
//!   semantic duplicates with different ids merge into the existing
//!   record.
//! - The store applies the delta with deterministic Rust rules, no
//!   LLM round-trip. See [`FindingsStore::apply_delta`].
//! - Persistence is handed to the `jsondb` crate: every write guard
//!   drop atomically writes the canonical `findings.json`. No more
//!   `findings-N.json` history.
//!
//! The canonical on-disk schema mirrors [`FindingsFile`] exactly,
//! wrapped in jsondb's top-level `version` field.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use jsondb::{JsonDb, SchemaV0};
use serde::{Deserialize, Serialize};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum FindingsError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("jsondb error: {0}")]
    JsonDb(#[from] jsondb::Error),

    #[error("base findings path {0} has no parent directory")]
    NoParent(PathBuf),
}

#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq, PartialOrd, Ord,
)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum Severity {
    /// Also the value an omitted `severity` takes. `merge_into` raises
    /// severity by max, and Low is that max's identity, so a delta
    /// that does not mention severity cannot change one.
    #[default]
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum Status {
    #[default]
    Active,
    Unconfirmed,
    Fixed,
    Invalidated,
}

/// Why a finding is invalidated, as a typed claim instead of prose.
///
/// An invalidation always rests on something being TRUE about the
/// code — a lock that is held, a bound already enforced, a flag
/// combination the API rejects. Naming that claim is what makes the
/// invalidation falsifiable: a later pass that reads the same code and
/// finds the claim does not hold can reverse the status without having
/// to re-derive the whole finding.
///
/// Recorded because the 2026-08-20 arch/x86/kvm/mmu review invalidated
/// `mirror_root_dirty_log_kvm_bug_on` on the claim that
/// `check_memory_region_flags()` makes `KVM_MEM_GUEST_MEMFD` and
/// `KVM_MEM_LOG_DIRTY_PAGES` mutually exclusive. That claim is only
/// true of the flags in one ioctl request, not of a slot over its
/// lifetime, and the same run later filed the `KVM_MR_FLAGS_ONLY`
/// bypass as its own high finding. The refutation existed; nothing
/// connected it to the invalidation it destroyed, because the claim
/// lived only in `summary` prose.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InvalidationBasis {
    /// The single claim the invalidation rests on, phrased so that
    /// the claim being true means the finding is not a bug.
    pub premise: String,
    /// `filename:line` citations that establish `premise`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
}

impl InvalidationBasis {
    /// A basis Rust will accept: a premise, and at least one citation
    /// for it. Without the citation the premise is an assertion, which
    /// is the thing being guarded against.
    pub fn is_well_formed(&self) -> bool {
        !self.premise.trim().is_empty() && self.evidence.iter().any(|e| !e.trim().is_empty())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RelevantSymbol {
    pub name: String,
    pub filename: String,
    pub line: u32,
    pub definition: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RelevantFileSection {
    pub filename: String,
    pub line_start: u32,
    pub line_end: u32,
    pub content: String,
}

/// Per-task narrative detail captured on a Finding. Each entry is
/// the full analysis prose produced by one task that touched this
/// finding (either on its introductory add or on a subsequent
/// update). Retained for operator diagnostics and explicit exports; summary
/// validation redacts it before invoking an agent.
///
/// These entries are NEVER forwarded to another LLM call. Every
/// site that hands findings to an agent strips the field first —
/// slow-agent `previous_findings`, consolidator lens outputs
/// (which come from freshly-deserialised agent replies and don't
/// carry it anyway), and the promoter's narrowed existing_findings
/// all run through [`Finding::redacted_for_agent`].
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FindingDetail {
    /// Provenance stamp. Same format as `last_updated_task` —
    /// `"<uuid-simple>/<todo-tag>"` or bare uuid.
    pub task: String,
    /// The task's effective_analysis prose verbatim.
    pub analysis: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub id: String,
    /// Defaulted, like `summary` and `severity`, so that an UPDATE can
    /// be expressed: reuse an existing id and send only the fields
    /// that changed.
    ///
    /// The layer beneath already worked that way — `prefer_longer`
    /// ignores an empty incoming string and severity merges by max —
    /// but the wire type demanded a whole record, so the two shapes
    /// this system asks agents for could not be parsed at all. The
    /// retirement shape the review prompt instructs,
    /// `{id, status: invalidated, invalidation}`, failed on `missing
    /// field title`; so did `{id, open_questions}`. One such entry
    /// fails the entire response, so the lens is re-run: measured over
    /// 26 concurrent reviews on 2026-08-22, 1,766 repair calls against
    /// 7,398 lens responses, 91 of them for a missing title and 28 for
    /// a missing summary.
    ///
    /// A record that names no existing id and carries no title is not
    /// an update — `apply_delta_to_list` refuses it rather than
    /// storing a nameless finding.
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub severity: Severity,
    #[serde(default)]
    pub status: Status,
    #[serde(default)]
    pub relevant_symbols: Vec<RelevantSymbol>,
    #[serde(default)]
    pub relevant_file_sections: Vec<RelevantFileSection>,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub reproducer_sketch: String,
    #[serde(default)]
    pub impact: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mechanism_detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix_sketch: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_questions: Vec<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    pub first_seen_task: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    pub last_updated_task: Option<String>,

    /// Wall-clock timestamp of the first apply_delta that inserted
    /// this finding. Stamped once on insert; never updated by
    /// subsequent applies so the "when was this discovered" signal
    /// stays stable. Missing on findings loaded from pre-field
    /// findings.json files — those have no authoritative discovery
    /// date on record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(skip)]
    pub first_seen_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related_finding_ids: Vec<String>,

    /// Per-task narrative captured from the task's effective_analysis at
    /// apply_delta time. Store-local and NEVER forwarded to another LLM.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(skip)]
    pub details: Vec<FindingDetail>,

    /// Wire-only signal: open questions this delta has SETTLED.
    ///
    /// `open_questions` is unioned across deltas, so without an
    /// explicit close channel a question can never leave a finding.
    /// Observed on the 2026-08-07 kernel/sched/fair.c review: 1,527
    /// open questions across 107 findings, one finding carrying 108,
    /// including entries whose text began "RESOLVED (negative): ..."
    /// — answers filed in the open list because there was nowhere
    /// else to put them — and the same question re-appended up to
    /// eight times as "OPEN:", then "STILL OPEN:".
    ///
    /// Each entry must be the question text being closed. Matching is
    /// exact after trimming: Rust does not classify prose, so a
    /// paraphrase closes nothing (see AGENTS.md). Never serialized on
    /// stored records — `merge_into` consumes the signal.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resolved_questions: Vec<String>,

    /// Wire-only signal: when `true` on an incoming delta AND the
    /// matching-id existing record is `Status::Invalidated`, the
    /// existing record flips back to `Status::Active`. Intended for
    /// slow-agent turns that discover new evidence reversing a
    /// prior invalidation (see slow-code-agent-audit.system.md). Never
    /// serialized on stored records — `merge_into` consumes the
    /// signal and doesn't propagate it; on a new-id apply the flag
    /// is stripped before the entry enters the list.
    #[serde(default, skip_serializing_if = "is_false")]
    pub reactivate: bool,

    /// Commit that introduced the bug, once a task has attributed
    /// the finding to a specific SHA. Left `None` until a later
    /// investigation fills it in. Only `sha` is mandatory; the
    /// subject line is a best-effort convenience so consumers
    /// (exports, summaries, review comments) don't need a second
    /// `git show` round-trip to print the attribution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub introduced_by: Option<IntroducedBy>,

    /// The claim a `Status::Invalidated` record rests on. Set by
    /// `merge_into` when it accepts an invalidation, cleared when the
    /// record leaves that status.
    ///
    /// Required: an incoming delta that flips an existing record to
    /// invalidated without a well-formed basis is REFUSED, and the
    /// record keeps its prior status. See [`InvalidationBasis`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalidation: Option<InvalidationBasis>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IntroducedBy {
    pub sha: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub subject: String,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl Finding {
    /// Return a clone suitable for inclusion in an LLM prompt —
    /// Store-owned provenance and details are cleared so they cannot become
    /// part of the model-facing wire contract.
    ///
    /// This is ALSO applied to findings a model just emitted, before
    /// they are stored, so it must never drop anything the store needs
    /// to keep. Source bodies in particular: `value_to_findings` runs
    /// it on parsed output that becomes `findings_delta`, so stripping
    /// `relevant_symbols[].definition` here would silently store every
    /// new finding without its evidence, and `/summary` renders that
    /// evidence as `exact_text`. To shrink a PROMPT, use
    /// [`Finding::without_source_bodies`] at the point the prompt is
    /// built instead.
    pub fn redacted_for_agent(&self) -> Finding {
        let mut c = self.clone();
        c.first_seen_task = None;
        c.last_updated_task = None;
        c.first_seen_at = None;
        c.details.clear();
        c
    }

    /// Return a clone with the source each finding carries removed:
    /// `relevant_symbols[].definition` and
    /// `relevant_file_sections[].content`.
    ///
    /// For `previous_findings` only. Every finding is still sent, in
    /// full — this drops no finding and no claim, and `name`,
    /// `filename` and `line` survive so every symbol stays citable and
    /// re-fetchable via a `source` or `read` followup.
    ///
    /// Measured on the 2026-08-06 mm/swapfile.c review: at 99 findings
    /// `previous_findings` reached 1187 KB, of which definitions were
    /// 427 KB and file-section contents 114 KB. The cached head alone
    /// then passed the 1,048,576-character cap on the codex-codes
    /// JSON-RPC transport, so the `general` lens failed with -32602 on
    /// twelve tasks and halted the session twice. Replaying the
    /// largest failing request through this takes it from 1368 KB to
    /// 900 KB.
    ///
    /// Never apply this to findings on their way INTO the store.
    pub fn without_source_bodies(&self) -> Finding {
        let mut c = self.clone();
        for symbol in &mut c.relevant_symbols {
            symbol.definition.clear();
        }
        for section in &mut c.relevant_file_sections {
            section.content.clear();
        }
        c
    }
}

/// Prepare prior findings for a prompt: agent-redacted AND without the
/// source bodies. The one entry point for `previous_findings`, so the
/// prioritizer's cached head stays byte-identical to the lens fan-out's.
pub fn findings_for_prompt_history(findings: &[Finding]) -> Vec<Finding> {
    findings
        .iter()
        .map(|f| f.redacted_for_agent().without_source_bodies())
        .collect()
}

#[cfg(test)]
mod open_question_tests {
    use super::*;

    fn with_questions(id: &str, questions: &[&str]) -> Finding {
        Finding {
            id: id.into(),
            title: "t".into(),
            severity: Severity::Medium,
            status: Status::Active,
            relevant_symbols: vec![],
            relevant_file_sections: vec![],
            summary: "s".into(),
            reproducer_sketch: "r".into(),
            impact: "i".into(),
            mechanism_detail: None,
            fix_sketch: None,
            open_questions: questions.iter().map(|q| (*q).to_string()).collect(),
            first_seen_task: None,
            last_updated_task: None,
            related_finding_ids: vec![],
            reactivate: false,
            resolved_questions: vec![],
            details: vec![],
            introduced_by: None,
            invalidation: None,
            first_seen_at: None,
        }
    }

    /// Without a close channel `open_questions` only ever grows: it is
    /// unioned across deltas, so a settled question stays forever and
    /// answers get filed as new "RESOLVED ..." entries beside it.
    #[test]
    fn a_delta_can_close_an_open_question() {
        let mut list = vec![with_questions(
            "f",
            &["Is PARANOID_AVG default-on?", "Bound on se->slice?"],
        )];
        let mut delta = with_questions("f", &[]);
        delta.resolved_questions = vec!["Is PARANOID_AVG default-on?".into()];
        let counts = apply_delta_to_list(&mut list, &[delta], Some("task"), None);
        assert!(counts.changed);
        assert_eq!(
            list[0].open_questions,
            vec!["Bound on se->slice?".to_string()]
        );
    }

    #[test]
    fn closing_matches_exactly_not_by_paraphrase() {
        // Rust must not decide from prose whether a question is
        // answered; a near-miss closes nothing.
        let mut list = vec![with_questions("f", &["Is PARANOID_AVG default-on?"])];
        let mut delta = with_questions("f", &[]);
        delta.resolved_questions = vec!["is paranoid_avg default on".into()];
        apply_delta_to_list(&mut list, &[delta], Some("task"), None);
        assert_eq!(list[0].open_questions.len(), 1, "paraphrase must not close");

        // Whitespace differences do not count as a paraphrase.
        let mut delta = with_questions("f", &[]);
        delta.resolved_questions = vec!["  Is PARANOID_AVG default-on?  ".into()];
        apply_delta_to_list(&mut list, &[delta], Some("task"), None);
        assert!(list[0].open_questions.is_empty(), "trimmed match closes");
    }

    #[test]
    fn a_delta_can_add_and_close_in_one_turn() {
        let mut list = vec![with_questions("f", &["old question"])];
        let mut delta = with_questions("f", &["new question"]);
        delta.resolved_questions = vec!["old question".into()];
        apply_delta_to_list(&mut list, &[delta], Some("task"), None);
        assert_eq!(list[0].open_questions, vec!["new question".to_string()]);
    }

    #[test]
    fn the_close_signal_is_never_stored() {
        // Wire-only, like `reactivate`: a stored record must not carry
        // it into the next prompt.
        let mut list: Vec<Finding> = Vec::new();
        let mut delta = with_questions("fresh", &["q"]);
        delta.resolved_questions = vec!["something".into()];
        apply_delta_to_list(&mut list, &[delta], Some("task"), None);
        assert!(list[0].resolved_questions.is_empty());

        let mut delta = with_questions("fresh", &[]);
        delta.resolved_questions = vec!["q".into()];
        apply_delta_to_list(&mut list, &[delta], Some("task"), None);
        assert!(list[0].resolved_questions.is_empty());
        assert!(list[0].open_questions.is_empty());
    }
}

#[cfg(test)]
mod redaction_tests {
    use super::*;

    fn finding_with_bodies() -> Finding {
        Finding {
            id: "f1".into(),
            title: "t".into(),
            severity: Severity::High,
            status: Status::Active,
            relevant_symbols: vec![RelevantSymbol {
                name: "swap_dup_entries_cluster".into(),
                filename: "mm/swapfile.c".into(),
                line: 42,
                definition: "static int swap_dup_entries_cluster(void) { /* body */ }".into(),
            }],
            relevant_file_sections: vec![RelevantFileSection {
                filename: "mm/swapfile.c".into(),
                line_start: 40,
                line_end: 60,
                content: "twenty lines of source".into(),
            }],
            summary: "s".into(),
            reproducer_sketch: "r".into(),
            impact: "i".into(),
            mechanism_detail: None,
            fix_sketch: None,
            open_questions: vec![],
            first_seen_task: Some("task".into()),
            last_updated_task: Some("task".into()),
            related_finding_ids: vec![],
            reactivate: false,
            resolved_questions: vec![],
            details: vec![],
            introduced_by: None,
            invalidation: None,
            first_seen_at: None,
        }
    }

    /// The bug this guards against: `redacted_for_agent` also runs on
    /// findings a model just EMITTED, on their way INTO the store
    /// (`value_to_findings`). Stripping source there would store every
    /// new finding without its evidence, and `/summary` renders that
    /// evidence as `exact_text`.
    #[test]
    fn agent_redaction_keeps_source_because_it_also_runs_into_the_store() {
        let redacted = finding_with_bodies().redacted_for_agent();
        assert!(
            !redacted.relevant_symbols[0].definition.is_empty(),
            "redacted_for_agent must not drop source: it sanitizes findings being stored"
        );
        assert!(!redacted.relevant_file_sections[0].content.is_empty());
        // What it does strip: store-owned provenance.
        assert!(redacted.first_seen_task.is_none());
        assert!(redacted.details.is_empty());
    }

    #[test]
    fn prompt_history_drops_source_bodies_but_keeps_every_citation() {
        let shipped = findings_for_prompt_history(&[finding_with_bodies()]);
        assert_eq!(shipped.len(), 1, "no finding is ever dropped");
        let f = &shipped[0];
        assert!(f.relevant_symbols[0].definition.is_empty());
        assert!(f.relevant_file_sections[0].content.is_empty());
        // Everything needed to cite or re-fetch survives.
        assert_eq!(f.relevant_symbols[0].name, "swap_dup_entries_cluster");
        assert_eq!(f.relevant_symbols[0].filename, "mm/swapfile.c");
        assert_eq!(f.relevant_symbols[0].line, 42);
        assert_eq!(f.relevant_file_sections[0].line_start, 40);
        assert_eq!(f.summary, "s");
        assert_eq!(f.impact, "i");
        // Prompt history is still agent-redacted.
        assert!(f.first_seen_task.is_none());
    }

    #[test]
    fn stripping_does_not_mutate_the_stored_finding() {
        // The store keeps the bodies; only the prompt copy loses them,
        // which is what /summary and the export path depend on.
        let stored = finding_with_bodies();
        let _ = stored.without_source_bodies();
        assert!(!stored.relevant_symbols[0].definition.is_empty());
        assert!(!stored.relevant_file_sections[0].content.is_empty());
    }
}

/// Apply [`Finding::redacted_for_agent`] to every entry. Convenience
/// for the common case where a whole slice is about to be shipped
/// to an agent.
pub fn redact_findings_for_agent(findings: &[Finding]) -> Vec<Finding> {
    findings.iter().map(Finding::redacted_for_agent).collect()
}

/// Per-task narrative captured at the file level, independent of
/// whether the task produced any findings. Storage site for the
/// broader investigation prose a slow-agent run emits alongside
/// its delta — overview paragraphs, summary tables, per-function
/// walk-throughs, "Question 1/2" multi-step proofs, conclusions —
/// content that isn't attributable to a single finding body.
///
/// Observed gap: session `kres-findings2` on 2026-04-23 had 21
/// `### <heading>` sections in report.md (Summary table,
/// Conclusion, Step 1-4, per-function walk-throughs) that were not
/// recoverable from any `Finding.details[].analysis` or
/// `mechanism_detail`. This entry exists so those bodies get a
/// canonical home without needing `/summary` to re-read report.md.
///
/// NEVER forwarded to another LLM. Agents see findings via
/// [`redact_findings_for_agent`] on `&[Finding]`, which never
/// touches the file-level `task_prose` list. Keep it that way.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskProse {
    /// Provenance stamp. Same format used by
    /// [`FindingDetail::task`] / `last_updated_task` —
    /// `"<uuid-simple>/<todo-tag>"` or bare uuid.
    pub task: String,
    /// Wall-clock timestamp of the append. Useful for ordering in
    /// `/summary` rendering when multiple tasks land out of order.
    pub created_at: DateTime<Utc>,
    /// The broader-than-finding investigation narrative verbatim.
    pub prose: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FindingsFile {
    #[serde(default)]
    pub findings: Vec<Finding>,
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
    /// Consecutive task reaps that produced no change to the list.
    /// Used by `--turns 0` stagnation logic; persisted so a resumed
    /// REPL still sees the running counter.
    #[serde(default)]
    pub tasks_since_change: u32,
    /// Turn counter. Monotonic across all writes. Useful for logs and
    /// for operators eyeballing how much churn a session produced.
    #[serde(default)]
    pub turn_n: Option<u32>,
    /// Per-task broader-than-finding narrative (see [`TaskProse`]).
    /// Append-only diagnostic history; NEVER serialised into an agent prompt.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub task_prose: Vec<TaskProse>,
}

impl SchemaV0 for FindingsFile {}

/// Delta-based findings store, backed by jsondb.
///
/// Construct with `FindingsStore::new(path).await` pointing at
/// `<results>/findings.json`. The store loads the existing file if
/// present, else starts with an empty list. Every call to
/// [`Self::apply_delta`] applies the delta with deterministic rules
/// and writes the updated file atomically.
pub struct FindingsStore {
    base_path: PathBuf,
    db: Arc<JsonDb<FindingsFile>>,
}

impl FindingsStore {
    pub async fn new(base_path: impl Into<PathBuf>) -> Result<Self, FindingsError> {
        let base_path: PathBuf = base_path.into();
        let parent = base_path
            .parent()
            .ok_or_else(|| FindingsError::NoParent(base_path.clone()))?;
        std::fs::create_dir_all(parent)?;
        let db = JsonDb::<FindingsFile>::load(base_path.clone()).await?;
        Ok(Self {
            base_path,
            db: Arc::new(db),
        })
    }

    pub fn base_path(&self) -> &Path {
        &self.base_path
    }

    /// Snapshot of the current findings list.
    pub async fn snapshot(&self) -> Vec<Finding> {
        self.db.read().await.findings.clone()
    }

    /// Full file snapshot, including counters and timestamp.
    pub async fn file_snapshot(&self) -> FindingsFile {
        self.db.read().await.clone()
    }

    pub async fn tasks_since_change(&self) -> u32 {
        self.db.read().await.tasks_since_change
    }

    pub async fn last_turn(&self) -> u32 {
        self.db.read().await.turn_n.unwrap_or(0)
    }

    /// Apply an inference-produced delta to the store.
    ///
    /// Rules:
    /// - New id → append, stamp `first_seen_task` / `last_updated_task`.
    /// - Existing id, incoming `status: Invalidated` → flip existing to
    ///   invalidated, preserve the body, take any new summary text.
    /// - Existing id, otherwise → merge in place: union relevant
    ///   symbols / file sections / related_finding_ids /
    ///   open_questions; prefer incoming non-empty prose fields; keep
    ///   the max severity; stamp `last_updated_task`.
    /// - The returned `merged` list reflects the post-apply state.
    /// - `changed` is true iff anything was added, flipped to
    ///   invalidated, or any field on an existing entry changed.
    pub async fn apply_delta(
        &self,
        delta: &[Finding],
        task_id: Option<&str>,
        task_analysis: Option<&str>,
    ) -> Result<ApplyReport, FindingsError> {
        let mut guard = self.db.write().await;
        let counts = apply_delta_to_list(&mut guard.findings, delta, task_id, task_analysis);
        let next_turn = guard.turn_n.unwrap_or(0).saturating_add(1);
        guard.turn_n = Some(next_turn);
        guard.updated_at = Some(Utc::now());
        if counts.changed {
            guard.tasks_since_change = 0;
        } else {
            guard.tasks_since_change = guard.tasks_since_change.saturating_add(1);
        }

        let merged = guard.findings.clone();
        let tasks_since_change = guard.tasks_since_change;
        // Drop the guard to trigger jsondb's atomic save.
        drop(guard);

        Ok(ApplyReport {
            merged,
            added: counts.added,
            updated: counts.updated,
            invalidated: counts.invalidated,
            reactivated: counts.reactivated,
            invalidation_refused: counts.invalidation_refused,
            incomplete_refused: counts.incomplete_refused,
            changed: counts.changed,
            turn_n: next_turn,
            tasks_since_change,
        })
    }

    /// Append a per-task broader-narrative entry to
    /// [`FindingsFile::task_prose`]. Provenance-keyed — callers pass
    /// the same task id string they pass to
    /// [`Self::apply_delta`]. Multiple appends for the same task
    /// stack in call order; callers decide whether to dedupe.
    ///
    /// NEVER forwarded to another LLM. Agents see findings via
    /// [`redact_findings_for_agent`] on `&[Finding]`; the
    /// file-level `task_prose` list never enters an agent payload.
    pub async fn append_task_prose(&self, task: &str, prose: &str) -> Result<(), FindingsError> {
        if prose.is_empty() {
            return Ok(());
        }
        let mut guard = self.db.write().await;
        guard.task_prose.push(TaskProse {
            task: task.to_string(),
            created_at: Utc::now(),
            prose: prose.to_string(),
        });
        guard.updated_at = Some(Utc::now());
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ApplyReport {
    pub merged: Vec<Finding>,
    pub added: u32,
    pub updated: u32,
    pub invalidated: u32,
    /// Count of Invalidated → Active transitions triggered by an
    /// incoming delta's `reactivate: true` flag. Distinct from
    /// `updated` so an operator eyeballing a run can see the rare
    /// case where a prior invalidation was reversed.
    pub reactivated: u32,
    /// Count of `status: invalidated` deltas rejected for want of a
    /// well-formed [`InvalidationBasis`]. Surfaced so an operator can
    /// see a lens repeatedly trying to park an unproven finding in the
    /// invalidated bucket.
    pub invalidation_refused: u32,
    /// Count of records that named no existing finding and lacked a
    /// title or a summary — a delta whose id does not exist. Surfaced
    /// so a model that keeps sending updates for a finding it never
    /// filed is visible rather than silently ignored.
    pub incomplete_refused: u32,
    pub changed: bool,
    pub turn_n: u32,
    pub tasks_since_change: u32,
}

#[derive(Debug, Clone, Default)]
pub struct DeltaCounts {
    pub added: u32,
    pub updated: u32,
    pub invalidated: u32,
    pub reactivated: u32,
    pub invalidation_refused: u32,
    /// Records that named no existing finding and carried no title or
    /// no summary. They are deltas for an id that does not exist, so
    /// there is nothing to merge them into and not enough to store.
    pub incomplete_refused: u32,
    pub changed: bool,
}

/// Apply a delta to an in-memory findings list using the same rules
/// as [`FindingsStore::apply_delta`]. Exposed so the REPL's no-store
/// path and the store can share one implementation.
pub fn apply_delta_to_list(
    current: &mut Vec<Finding>,
    delta: &[Finding],
    task_id: Option<&str>,
    task_analysis: Option<&str>,
) -> DeltaCounts {
    let mut counts = DeltaCounts::default();
    for incoming in delta {
        match current.iter().position(|e| e.id == incoming.id) {
            Some(idx) => {
                let was_invalidated = current[idx].status == Status::Invalidated;
                let outcome = merge_into(&mut current[idx], incoming, task_id);
                record_detail(&mut current[idx], task_id, task_analysis);
                if outcome.invalidation_refused {
                    counts.invalidation_refused += 1;
                }
                if outcome.changed {
                    let is_invalidated = current[idx].status == Status::Invalidated;
                    if !was_invalidated && is_invalidated {
                        counts.invalidated += 1;
                    } else if was_invalidated && !is_invalidated {
                        counts.reactivated += 1;
                    } else {
                        counts.updated += 1;
                    }
                    counts.changed = true;
                }
            }
            None => {
                // Nothing to merge into, so this has to stand alone —
                // and a finding with no title or no summary cannot.
                // Refuse before `semantic_duplicate_index` gets a look
                // at it: that match needs `id_title_token_overlap` to
                // clear 0.70, a titleless record gives it almost
                // nothing to work with, and merging into the wrong
                // record silently destroys a finding.
                if incoming.title.trim().is_empty() || incoming.summary.trim().is_empty() {
                    counts.incomplete_refused += 1;
                    continue;
                }
                if incoming.status != Status::Invalidated {
                    if let Some(idx) = semantic_duplicate_index(current, incoming) {
                        let outcome = merge_into(&mut current[idx], incoming, task_id);
                        record_detail(&mut current[idx], task_id, task_analysis);
                        if outcome.changed {
                            counts.updated += 1;
                            counts.changed = true;
                        }
                        continue;
                    }
                }
                let mut new_entry = incoming.clone();
                // `reactivate` is a transient wire signal; don't let
                // it persist on a newly-inserted record. Same for any
                // stray details an incoming delta tried to carry —
                // details is a store-local concept, not a wire
                // contract the agents know about.
                new_entry.reactivate = false;
                // A brand-new finding cannot have settled a question
                // that was never on it; drop the signal rather than
                // storing it.
                new_entry.resolved_questions.clear();
                new_entry.details.clear();
                if let Some(t) = task_id {
                    if new_entry.first_seen_task.is_none() {
                        new_entry.first_seen_task = Some(t.to_string());
                    }
                    new_entry.last_updated_task = Some(t.to_string());
                }
                if new_entry.first_seen_at.is_none() {
                    new_entry.first_seen_at = Some(Utc::now());
                }
                current.push(new_entry);
                let last_idx = current.len() - 1;
                record_detail(&mut current[last_idx], task_id, task_analysis);
                counts.added += 1;
                counts.changed = true;
            }
        }
    }
    counts
}

/// Append (or refresh) a `FindingDetail` entry on `finding` carrying
/// this task's analysis prose. No-op when either `task_id` or
/// `task_analysis` is None / empty. If an entry already exists for
/// the same task (rare; would require the same task applying the
/// same id twice in one delta), the existing entry's analysis is
/// replaced with the incoming — the latest write wins.
fn record_detail(finding: &mut Finding, task_id: Option<&str>, task_analysis: Option<&str>) {
    let (Some(tid), Some(body)) = (task_id, task_analysis) else {
        return;
    };
    if tid.is_empty() || body.trim().is_empty() {
        return;
    }
    if let Some(existing) = finding.details.iter_mut().find(|d| d.task == tid) {
        existing.analysis = body.to_string();
        return;
    }
    finding.details.push(FindingDetail {
        task: tid.to_string(),
        analysis: body.to_string(),
    });
}

fn semantic_duplicate_index(current: &[Finding], incoming: &Finding) -> Option<usize> {
    current
        .iter()
        .position(|existing| is_semantic_duplicate(existing, incoming))
}

/// Is an incoming finding with a NEW id actually the same defect as one we
/// already hold?
///
/// Sharing a code anchor is necessary but nowhere near sufficient — a single
/// function hosts many distinct defects — so the identity tokens must agree
/// as well.
///
/// `related_finding_ids` used to short-circuit that check: if either side
/// named the other, an anchor match alone merged them. But "related to" is a
/// different relation from "is the same as", and the schema models both
/// separately. Treating the cross-reference as an identity claim destroyed
/// the distinction: on the 2026-08-05 mm/page_alloc.c review six records
/// ended up with an id naming one defect and a title naming another, because
/// `merge_into`'s longest-title-wins rule then overwrote the title of
/// whichever record was merged into. `contig_comp_ignores_bad_page`
/// accumulated nine detail entries from unrelated tasks and finished
/// describing an `unpoison_memory()` double-put.
///
/// A cross-reference now only *permits* a merge; the tokens still have to
/// agree. Being wrong in this direction costs a duplicate record, which a
/// later consolidation can still merge. Being wrong the other way silently
/// destroys a finding.
fn is_semantic_duplicate(existing: &Finding, incoming: &Finding) -> bool {
    if existing.status == Status::Invalidated || incoming.status == Status::Invalidated {
        return false;
    }
    share_code_anchor(existing, incoming) && id_title_token_overlap(existing, incoming) >= 0.70
}

/// Do two findings point at the same place in the source?
///
/// A line number of 0 is not a location. Agents emit `filename:0` when they
/// have not resolved a real line, and matching on it made every such finding
/// share an anchor with every other one in the same file: the 2026-08-05
/// mm/page_alloc.c review had four findings carrying `mm/page_alloc.c:0`,
/// i.e. six false anchor pairs feeding `is_semantic_duplicate`. Require a
/// resolved line, or a matching symbol name, before calling it the same place.
fn share_code_anchor(a: &Finding, b: &Finding) -> bool {
    for asym in &a.relevant_symbols {
        for bsym in &b.relevant_symbols {
            if asym.filename.is_empty() || asym.filename != bsym.filename {
                continue;
            }
            let same_name = !asym.name.is_empty() && asym.name == bsym.name;
            let same_line = asym.line != 0 && asym.line == bsym.line;
            if same_name || same_line {
                return true;
            }
        }
    }
    for asec in &a.relevant_file_sections {
        for bsec in &b.relevant_file_sections {
            // A 0..0 section is the same placeholder in section form.
            if asec.line_end == 0 || bsec.line_end == 0 {
                continue;
            }
            if !asec.filename.is_empty()
                && asec.filename == bsec.filename
                && ranges_overlap(
                    asec.line_start,
                    asec.line_end,
                    bsec.line_start,
                    bsec.line_end,
                )
            {
                return true;
            }
        }
    }
    false
}

fn ranges_overlap(a_start: u32, a_end: u32, b_start: u32, b_end: u32) -> bool {
    let a_lo = a_start.min(a_end);
    let a_hi = a_start.max(a_end);
    let b_lo = b_start.min(b_end);
    let b_hi = b_start.max(b_end);
    a_lo <= b_hi && b_lo <= a_hi
}

fn id_title_token_overlap(a: &Finding, b: &Finding) -> f64 {
    let a_tokens = finding_identity_tokens(a);
    let b_tokens = finding_identity_tokens(b);
    let denom = a_tokens.len().min(b_tokens.len());
    if denom == 0 {
        return 0.0;
    }
    let shared = a_tokens.intersection(&b_tokens).count();
    shared as f64 / denom as f64
}

fn finding_identity_tokens(f: &Finding) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    collect_tokens(&f.id, &mut out);
    collect_tokens(&f.title, &mut out);
    out
}

fn collect_tokens(s: &str, out: &mut std::collections::BTreeSet<String>) {
    for raw in s.split(|c: char| !c.is_ascii_alphanumeric()) {
        let token = raw.to_ascii_lowercase();
        if token.len() < 2 || SEMANTIC_DUP_STOPWORDS.contains(&token.as_str()) {
            continue;
        }
        out.insert(token);
    }
}

const SEMANTIC_DUP_STOPWORDS: &[&str] = &[
    "and", "are", "for", "from", "into", "that", "the", "this", "with", "without",
];

/// Merge `incoming` into `existing` in place. Returns true iff any
/// field on `existing` changed.
///
/// Prose-field policy: to protect against a later task that mentions
/// the same finding id in passing overwriting a richer earlier body,
/// we only take the incoming value when it's at least as long as the
/// existing one. Ties keep `existing` (idempotent). This is a blunt
/// heuristic — a slow agent that rewrites a summary to be more
/// precise but SHORTER loses — but it prevents the common downgrade
/// path (incoming is a one-sentence reminder; existing is the full
/// analysis). Empty incoming is always ignored regardless of length.
#[derive(Default)]
struct StatusTransition {
    changed: bool,
    /// An incoming `status: invalidated` was rejected for want of a
    /// well-formed [`InvalidationBasis`]. The record keeps its prior
    /// status; the rest of the delta still merges.
    invalidation_refused: bool,
}

/// Move `existing.status` according to `incoming`.
///
/// The table, and why each entry is what it is:
///
/// - `reactivate: true` on an invalidated record → Active. Wins over
///   whatever `status` carries, so a contradictory delta produces one
///   transition rather than a flip and a flip back.
/// - → Invalidated, with a well-formed basis → accepted, basis stored.
/// - → Invalidated, without one → REFUSED. An invalidation that cannot
///   name the claim it rests on is not negative evidence, it is an
///   unproven reachability argument wearing negative evidence's badge.
///   On the 2026-08-20 arch/x86/kvm/mmu review two findings were held
///   invalidated by deltas that said so outright — "status remains
///   invalidated pending confirmation of whether…" and "the decisive
///   bodies were not obtained, so status remains invalidated/unresolved
///   rather than disproved". Both are real host-DoS bugs.
/// - → Unconfirmed from Active OR Invalidated → accepted, no flag
///   needed. This is the state those two deltas wanted. Leaving it
///   unreachable is what made `invalidated` the dumping ground for
///   everything that was not provable in one pass.
/// - Unconfirmed → Active → accepted plainly. Only Invalidated → Active
///   needs `reactivate`, because only that one contradicts a recorded
///   claim.
///
/// `Fixed` is not part of the review vocabulary; it arrives from the
/// validate path, which writes the status directly rather than through
/// a delta, so an incoming `Fixed` here is ignored.
fn apply_status_transition(existing: &mut Finding, incoming: &Finding) -> StatusTransition {
    let mut out = StatusTransition::default();

    if incoming.reactivate {
        if existing.status == Status::Invalidated {
            existing.status = Status::Active;
            existing.invalidation = None;
            out.changed = true;
        }
        return out;
    }

    match incoming.status {
        Status::Invalidated => {
            let basis = incoming
                .invalidation
                .as_ref()
                .filter(|b| b.is_well_formed());
            let Some(basis) = basis else {
                // Already-invalidated records are not "refused" — the
                // delta is extending a record whose basis is already on
                // file, which is the ordinary shape of a later pass
                // adding detail.
                out.invalidation_refused = existing.status != Status::Invalidated;
                return out;
            };
            if existing.status != Status::Invalidated {
                existing.status = Status::Invalidated;
                out.changed = true;
            }
            if existing.invalidation.as_ref() != Some(basis) {
                existing.invalidation = Some(basis.clone());
                out.changed = true;
            }
        }
        Status::Unconfirmed => {
            if matches!(existing.status, Status::Active | Status::Invalidated) {
                existing.status = Status::Unconfirmed;
                existing.invalidation = None;
                out.changed = true;
            }
        }
        Status::Active => {
            if existing.status == Status::Unconfirmed {
                existing.status = Status::Active;
                out.changed = true;
            }
        }
        Status::Fixed => {}
    }

    out
}

struct MergeOutcome {
    changed: bool,
    invalidation_refused: bool,
}

fn merge_into(existing: &mut Finding, incoming: &Finding, task_id: Option<&str>) -> MergeOutcome {
    let mut changed = false;

    let transition = apply_status_transition(existing, incoming);
    changed |= transition.changed;

    // Prefer the higher severity.
    if incoming.severity > existing.severity {
        existing.severity = incoming.severity;
        changed = true;
    }

    // Prose fields: longer-wins, guarded against downgrades.
    changed |= prefer_longer(&mut existing.title, &incoming.title);
    changed |= prefer_longer(&mut existing.summary, &incoming.summary);
    changed |= prefer_longer(&mut existing.reproducer_sketch, &incoming.reproducer_sketch);
    changed |= prefer_longer(&mut existing.impact, &incoming.impact);
    changed |= prefer_longer_opt(&mut existing.mechanism_detail, &incoming.mechanism_detail);
    changed |= prefer_longer_opt(&mut existing.fix_sketch, &incoming.fix_sketch);
    changed |= merge_introduced_by(&mut existing.introduced_by, &incoming.introduced_by);

    // Union collections.
    changed |= union_symbols(&mut existing.relevant_symbols, &incoming.relevant_symbols);
    changed |= union_sections(
        &mut existing.relevant_file_sections,
        &incoming.relevant_file_sections,
    );
    changed |= union_strings(&mut existing.open_questions, &incoming.open_questions);
    // Additions first, then the explicit closes, so one delta can
    // both raise a new question and settle an old one.
    changed |= close_questions(&mut existing.open_questions, &incoming.resolved_questions);
    changed |= union_strings(
        &mut existing.related_finding_ids,
        &incoming.related_finding_ids,
    );

    if let Some(t) = task_id {
        let stamp = Some(t.to_string());
        if existing.last_updated_task != stamp {
            existing.last_updated_task = stamp;
            changed = true;
        }
        if existing.first_seen_task.is_none() {
            existing.first_seen_task = Some(t.to_string());
            changed = true;
        }
    }

    MergeOutcome {
        changed,
        invalidation_refused: transition.invalidation_refused,
    }
}

/// Overwrite `existing` with `incoming` when the incoming value is
/// strictly longer, OR when `existing` is empty and `incoming` is
/// not. Ties and downgrades keep `existing`. Returns true iff
/// `existing` changed.
fn prefer_longer(existing: &mut String, incoming: &str) -> bool {
    if incoming.is_empty() || incoming == existing {
        return false;
    }
    if existing.is_empty() || incoming.len() > existing.len() {
        *existing = incoming.to_string();
        return true;
    }
    false
}

/// Merge an incoming `introduced_by` into an existing one. Rules:
///   - Incoming `None` or empty `sha`: no-op.
///   - Existing `None`: take incoming (both sha and subject).
///   - Existing `Some` with same `sha`: take incoming `subject` if it
///     is non-empty AND longer than the current one (matches the
///     prose-downgrade guard used elsewhere).
///   - Existing `Some` with a DIFFERENT non-empty `sha`: latest wins,
///     including subject. A later task may have attributed the bug
///     more precisely, and keeping the old sha silently would mask
///     that.
fn merge_introduced_by(
    existing: &mut Option<IntroducedBy>,
    incoming: &Option<IntroducedBy>,
) -> bool {
    let Some(inc) = incoming else { return false };
    if inc.sha.is_empty() {
        return false;
    }
    match existing {
        None => {
            *existing = Some(inc.clone());
            true
        }
        Some(cur) if cur.sha == inc.sha => {
            if !inc.subject.is_empty() && inc.subject.len() > cur.subject.len() {
                cur.subject = inc.subject.clone();
                return true;
            }
            false
        }
        Some(_) => {
            *existing = Some(inc.clone());
            true
        }
    }
}

fn prefer_longer_opt(existing: &mut Option<String>, incoming: &Option<String>) -> bool {
    let Some(inc) = incoming else { return false };
    if inc.is_empty() {
        return false;
    }
    match existing {
        Some(cur) if cur == inc => false,
        Some(cur) if inc.len() > cur.len() => {
            *existing = Some(inc.clone());
            true
        }
        Some(_) => false,
        None => {
            *existing = Some(inc.clone());
            true
        }
    }
}

/// Return the subset of `store` whose identifying tokens appear in
/// `prose`. "Identifying tokens" means any of:
///   - the Finding's `id`,
///   - the basename or full path of any `relevant_symbols[].filename`
///     or `relevant_file_sections[].filename`,
///   - the `name` of any `relevant_symbols[]` entry (matched as a
///     whole-word identifier).
///
/// Used to narrow the promoter's prompt payload: the audit LLM only
/// needs to see findings that could plausibly match what the prose
/// describes, not the whole store. False negatives (a relevant
/// finding missed by the scan) are handled by the caller's dedup
/// filter, which sees the full store and renames colliding ids —
/// never drops.
///
/// The scan is intentionally generous: when in doubt, include.
pub fn relevant_subset(prose: &str, store: &[Finding]) -> Vec<Finding> {
    if prose.is_empty() || store.is_empty() {
        return Vec::new();
    }
    store
        .iter()
        .filter(|f| finding_mentioned_in_prose(f, prose))
        .cloned()
        .collect()
}

fn finding_mentioned_in_prose(f: &Finding, prose: &str) -> bool {
    // Match the id with identifier boundaries so a short id like
    // "y" doesn't match inside "Only" or similar.
    if !f.id.is_empty() && identifier_in_prose(&f.id, prose) {
        return true;
    }
    for sym in &f.relevant_symbols {
        if !sym.filename.is_empty() && file_in_prose(&sym.filename, prose) {
            return true;
        }
        if !sym.name.is_empty() && identifier_in_prose(&sym.name, prose) {
            return true;
        }
    }
    for sec in &f.relevant_file_sections {
        if !sec.filename.is_empty() && file_in_prose(&sec.filename, prose) {
            return true;
        }
    }
    false
}

/// True iff `path` (or its basename) appears as a substring of
/// `prose`. Substring match is OK here because filenames include
/// slashes and dots that rarely collide with unrelated prose tokens.
fn file_in_prose(path: &str, prose: &str) -> bool {
    if prose.contains(path) {
        return true;
    }
    if let Some(base) = path.rsplit('/').next() {
        if !base.is_empty() && base != path && prose.contains(base) {
            return true;
        }
    }
    false
}

/// True iff `ident` appears in `prose` bounded on both sides by a
/// non-identifier char (or start/end of string). Prevents
/// "free" matching inside "freed" or "cpu_mask" inside
/// "cpu_mask_var". Only ASCII alphanumerics and `_` count as
/// identifier chars; everything else (punctuation, whitespace,
/// UTF-8 letters) is a boundary.
fn identifier_in_prose(ident: &str, prose: &str) -> bool {
    if ident.is_empty() || ident.len() > prose.len() {
        return false;
    }
    let p = prose.as_bytes();
    let n = ident.as_bytes();
    let mut i = 0usize;
    while let Some(hit) = find_from(p, n, i) {
        let before_ok = hit == 0 || !is_ident_byte(p[hit - 1]);
        let after_ok = hit + n.len() == p.len() || !is_ident_byte(p[hit + n.len()]);
        if before_ok && after_ok {
            return true;
        }
        i = hit + 1;
    }
    false
}

fn find_from(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= hay.len() || needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|off| off + from)
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn union_symbols(dst: &mut Vec<RelevantSymbol>, src: &[RelevantSymbol]) -> bool {
    let mut changed = false;
    for s in src {
        let dup = dst
            .iter()
            .any(|e| e.filename == s.filename && e.line == s.line && e.name == s.name);
        if !dup {
            dst.push(s.clone());
            changed = true;
        }
    }
    changed
}

fn union_sections(dst: &mut Vec<RelevantFileSection>, src: &[RelevantFileSection]) -> bool {
    let mut changed = false;
    for s in src {
        let dup = dst
            .iter()
            .any(|e| e.filename == s.filename && e.line_start == s.line_start);
        if !dup {
            dst.push(s.clone());
            changed = true;
        }
    }
    changed
}

/// Drop every open question this delta explicitly settled.
///
/// Exact match after trimming. Rust does not decide from prose
/// whether a question is answered — a model says so through
/// `resolved_questions` or the question stays open.
fn close_questions(dst: &mut Vec<String>, resolved: &[String]) -> bool {
    if resolved.is_empty() {
        return false;
    }
    let closing: std::collections::BTreeSet<&str> = resolved
        .iter()
        .map(|q| q.trim())
        .filter(|q| !q.is_empty())
        .collect();
    if closing.is_empty() {
        return false;
    }
    let before = dst.len();
    dst.retain(|q| !closing.contains(q.trim()));
    before != dst.len()
}

fn union_strings(dst: &mut Vec<String>, src: &[String]) -> bool {
    let mut changed = false;
    for s in src {
        if !dst.iter().any(|e| e == s) {
            dst.push(s.clone());
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(nonce: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "kres-findings-test-{}-{}-{:x}",
            nonce,
            std::process::id(),
            rand_suffix()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn rand_suffix() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    }

    fn sym(name: &str, file: &str, line: u32) -> RelevantSymbol {
        RelevantSymbol {
            name: name.into(),
            filename: file.into(),
            line,
            definition: "body".into(),
        }
    }

    /// Two unrelated defects that both failed to resolve a line number must
    /// not be treated as the same place. Four findings in the 2026-08-05
    /// mm/page_alloc.c review carried `mm/page_alloc.c:0`, which made six
    /// false anchor pairs and fed the duplicate merge.
    #[test]
    fn placeholder_line_zero_is_not_a_shared_anchor() {
        let mut a = sample_finding("nofail_loop_skips_reclaim_compact_first");
        a.relevant_symbols
            .push(sym("__alloc_pages_slowpath", "mm/page_alloc.c", 0));
        let mut b = sample_finding("pcp_batch_zero_infinite_loop");
        b.relevant_symbols
            .push(sym("free_pcppages_bulk", "mm/page_alloc.c", 0));

        assert!(!share_code_anchor(&a, &b));
        assert!(!is_semantic_duplicate(&a, &b));

        // A resolved line at the same location still anchors.
        let mut c = sample_finding("c");
        c.relevant_symbols
            .push(sym("other", "mm/page_alloc.c", 2266));
        let mut d = sample_finding("d");
        d.relevant_symbols
            .push(sym("thing", "mm/page_alloc.c", 2266));
        assert!(share_code_anchor(&c, &d));
    }

    #[test]
    fn degenerate_zero_length_section_is_not_a_shared_anchor() {
        let mut a = sample_finding("a");
        a.relevant_file_sections.push(RelevantFileSection {
            filename: "mm/page_alloc.c".into(),
            line_start: 0,
            line_end: 0,
            content: String::new(),
        });
        let mut b = sample_finding("b");
        b.relevant_file_sections.push(RelevantFileSection {
            filename: "mm/page_alloc.c".into(),
            line_start: 0,
            line_end: 0,
            content: String::new(),
        });
        assert!(!share_code_anchor(&a, &b));
    }

    /// "Related to" is not "is the same as". Cross-referencing used to
    /// short-circuit the token check, so a distinct defect in the same
    /// function was merged and `merge_into`'s longest-title-wins rule then
    /// replaced the title — leaving an id naming one bug and a title naming
    /// another.
    #[test]
    fn related_finding_ids_do_not_make_two_defects_one() {
        let mut existing = sample_finding("contig_comp_ignores_bad_page");
        existing.title = "alloc_contig ignores a bad page during compaction".into();
        existing
            .relevant_symbols
            .push(sym("free_frozen_page_commit", "mm/page_alloc.c", 2940));

        let mut incoming = sample_finding("unpoison_second_put_underflows_live_page");
        incoming.title =
            "unpoison_memory() performs two folio_put() calls against one reference".into();
        incoming
            .relevant_symbols
            .push(sym("free_frozen_page_commit", "mm/page_alloc.c", 2940));
        // The agent legitimately cross-references the two.
        incoming.related_finding_ids = vec!["contig_comp_ignores_bad_page".into()];

        assert!(
            share_code_anchor(&existing, &incoming),
            "same function, so the anchor genuinely matches"
        );
        assert!(
            !is_semantic_duplicate(&existing, &incoming),
            "a cross-reference must not override the identity-token test"
        );

        let mut list = vec![existing];
        apply_delta_to_list(&mut list, std::slice::from_ref(&incoming), None, None);
        assert_eq!(list.len(), 2, "the two defects must stay separate records");
        assert_eq!(list[0].id, "contig_comp_ignores_bad_page");
        assert!(
            list[0].title.starts_with("alloc_contig"),
            "the original title must survive: {}",
            list[0].title
        );
    }

    /// A delta that retires `id`, carrying the basis Rust now demands.
    /// The wire shapes this system asks agents for. Both failed to
    /// deserialize before `title`/`summary`/`severity` were defaulted.
    #[test]
    fn the_delta_shapes_the_prompts_ask_for_actually_parse() {
        let update: Finding =
            serde_json::from_str(r#"{"id":"x","open_questions":["q"]}"#).expect("update parses");
        assert_eq!(update.id, "x");
        assert!(update.title.is_empty());
        assert_eq!(
            update.severity,
            Severity::Low,
            "an omitted severity must be the identity of merge_into's max, or a \
             silent delta would raise every finding it touches"
        );

        let retire: Finding = serde_json::from_str(
            r#"{"id":"x","status":"invalidated","invalidation":{"premise":"p","evidence":["a.c:1"]}}"#,
        )
        .expect("the retirement shape review.json instructs must parse");
        assert_eq!(retire.status, Status::Invalidated);
    }

    #[test]
    fn an_update_keeps_the_stored_title_summary_and_severity() {
        let mut list = vec![sample_finding("f")];
        let mut delta: Finding =
            serde_json::from_str(r#"{"id":"f","open_questions":["is it reachable?"]}"#).unwrap();
        delta.open_questions = vec!["is it reachable?".into()];

        let counts = apply_delta_to_list(&mut list, &[delta], Some("t1"), None);

        assert_eq!(list.len(), 1, "an update must not create a second record");
        assert_eq!(list[0].title, "finding f", "title survived the delta");
        assert_eq!(list[0].summary, "s", "summary survived the delta");
        assert_eq!(
            list[0].severity,
            Severity::High,
            "a delta with no severity must not downgrade"
        );
        assert_eq!(list[0].open_questions, vec!["is it reachable?".to_string()]);
        assert_eq!(counts.updated, 1);
        assert_eq!(counts.incomplete_refused, 0);
    }

    #[test]
    fn an_update_for_an_unknown_id_is_refused_not_stored() {
        // Nothing to merge into and not enough to stand alone. Storing
        // it would put a nameless finding in the report; guessing a
        // merge target from a titleless record is how a real finding
        // gets silently overwritten.
        let mut list = vec![sample_finding("f")];
        let orphan: Finding =
            serde_json::from_str(r#"{"id":"typo","open_questions":["q"]}"#).unwrap();

        let counts = apply_delta_to_list(&mut list, &[orphan], Some("t1"), None);

        assert_eq!(list.len(), 1, "the orphan delta must not be stored");
        assert_eq!(counts.incomplete_refused, 1);
        assert_eq!(counts.added, 0);
        assert!(!counts.changed);
    }

    #[test]
    fn a_complete_new_finding_still_inserts() {
        let mut list: Vec<Finding> = vec![];
        let counts = apply_delta_to_list(&mut list, &[sample_finding("new")], Some("t1"), None);
        assert_eq!(counts.added, 1);
        assert_eq!(counts.incomplete_refused, 0);
        assert_eq!(list.len(), 1);
    }

    fn invalidating_delta(id: &str, premise: &str) -> Finding {
        let mut f = sample_finding(id);
        f.status = Status::Invalidated;
        f.invalidation = Some(InvalidationBasis {
            premise: premise.to_string(),
            evidence: vec!["kernel/thing.c:42".into()],
        });
        f
    }

    fn sample_finding(id: &str) -> Finding {
        Finding {
            id: id.to_string(),
            title: format!("finding {id}"),
            severity: Severity::High,
            status: Status::Active,
            relevant_symbols: vec![],
            relevant_file_sections: vec![],
            summary: "s".into(),
            reproducer_sketch: "r".into(),
            impact: "i".into(),
            mechanism_detail: None,
            fix_sketch: None,
            open_questions: vec![],
            first_seen_task: None,
            last_updated_task: None,
            related_finding_ids: vec![],
            reactivate: false,
            resolved_questions: vec![],
            details: vec![],
            introduced_by: None,
            invalidation: None,
            first_seen_at: None,
        }
    }

    #[test]
    fn generated_schema_carries_the_invalidation_basis() {
        // The lens fan-out is never shown this schema — it learns the
        // shape from `globals.finding_schema` prose. The JSON-repair
        // and finding-repair calls ARE shown it, and they are the only
        // chance a malformed `invalidation` gets to be fixed rather
        // than dropped, so the field has to be in here.
        let schema = serde_json::to_value(schemars::schema_for!(Finding)).unwrap();
        let properties = schema["properties"].as_object().unwrap();
        assert!(properties.contains_key("invalidation"));
        let defs = schema["$defs"].as_object().unwrap();
        let basis = &defs["InvalidationBasis"];
        let required: Vec<&str> = basis["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(required.contains(&"premise"));
    }

    #[test]
    fn model_schema_and_redaction_exclude_store_owned_provenance() {
        let schema = serde_json::to_value(schemars::schema_for!(Finding)).unwrap();
        let properties = schema["properties"].as_object().unwrap();
        assert!(!properties.contains_key("first_seen_task"));
        assert!(!properties.contains_key("last_updated_task"));
        assert!(!properties.contains_key("first_seen_at"));
        assert!(!properties.contains_key("details"));

        let mut finding = sample_finding("owned");
        finding.first_seen_task = Some("forged".into());
        finding.last_updated_task = Some("forged".into());
        finding.first_seen_at = Some(Utc::now());
        let redacted = finding.redacted_for_agent();
        assert!(redacted.first_seen_task.is_none());
        assert!(redacted.last_updated_task.is_none());
        assert!(redacted.first_seen_at.is_none());
    }

    #[tokio::test]
    async fn details_record_one_entry_per_task_and_redact_clears() {
        // apply_delta with a non-empty task_analysis stamps a
        // FindingDetail on every finding it adds or updates. A
        // second apply under a DIFFERENT task_id appends; under
        // the SAME task_id overwrites. redacted_for_agent must
        // then strip every entry.
        let dir = tmp_dir("details");
        let base = dir.join("findings.json");
        let store = FindingsStore::new(&base).await.unwrap();
        store
            .apply_delta(&[sample_finding("a")], Some("t1"), Some("first pass prose"))
            .await
            .unwrap();
        store
            .apply_delta(
                &[sample_finding("a")],
                Some("t2"),
                Some("second pass prose extends"),
            )
            .await
            .unwrap();
        // Same task_id, different prose → overwrite, not append.
        store
            .apply_delta(
                &[sample_finding("a")],
                Some("t2"),
                Some("second pass prose v2"),
            )
            .await
            .unwrap();
        let snap = store.snapshot().await;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].details.len(), 2, "two distinct tasks");
        assert_eq!(snap[0].details[0].task, "t1");
        assert_eq!(snap[0].details[0].analysis, "first pass prose");
        assert_eq!(snap[0].details[1].task, "t2");
        assert_eq!(
            snap[0].details[1].analysis, "second pass prose v2",
            "same task_id overwrites"
        );
        let redacted = redact_findings_for_agent(&snap);
        assert!(
            redacted[0].details.is_empty(),
            "redacted copy must clear details"
        );
        // Empty analysis must NOT record a detail entry.
        store
            .apply_delta(&[sample_finding("a")], Some("t3"), Some(""))
            .await
            .unwrap();
        let snap2 = store.snapshot().await;
        assert_eq!(snap2[0].details.len(), 2, "empty analysis skipped");
        // Incoming delta carrying its own details on a NEW id must
        // not persist them — only apply_delta's task_analysis arg
        // populates the field.
        let mut tainted = sample_finding("b");
        tainted.details.push(FindingDetail {
            task: "forged".into(),
            analysis: "leaked".into(),
        });
        store
            .apply_delta(&[tainted], Some("t4"), Some("legit"))
            .await
            .unwrap();
        let b = store
            .snapshot()
            .await
            .into_iter()
            .find(|f| f.id == "b")
            .unwrap();
        assert_eq!(b.details.len(), 1);
        assert_eq!(b.details[0].task, "t4");
        assert_eq!(b.details[0].analysis, "legit");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn task_prose_appends_and_skips_empty() {
        let dir = tmp_dir("prose");
        let base = dir.join("findings.json");
        let store = FindingsStore::new(&base).await.unwrap();
        store
            .append_task_prose("task-a", "### Summary table\n| x | y |\n|---|---|")
            .await
            .unwrap();
        store
            .append_task_prose("task-b", "### Conclusion\nThe UAF path is gated.")
            .await
            .unwrap();
        // Empty prose is a no-op — don't pollute the list.
        store.append_task_prose("task-c", "").await.unwrap();

        let file = store.file_snapshot().await;
        assert_eq!(file.task_prose.len(), 2, "empty-prose call was skipped");
        assert_eq!(file.task_prose[0].task, "task-a");
        assert!(file.task_prose[0].prose.contains("Summary table"));
        assert_eq!(file.task_prose[1].task, "task-b");

        // The agent-facing redaction path operates on `&[Finding]`
        // and has no visibility into file-level `task_prose`. This
        // asserts the schema wall: the per-finding redaction is
        // unchanged, and nothing on the Finding side carries prose.
        let snap = store.snapshot().await;
        let redacted = redact_findings_for_agent(&snap);
        for f in &redacted {
            assert!(f.details.is_empty());
        }

        // Round-trip through JSON: task_prose must serialize and
        // survive a reload (persistence check for `/summary`).
        let raw = std::fs::read_to_string(&base).unwrap();
        let reloaded: FindingsFile = serde_json::from_str(&raw).unwrap();
        assert_eq!(reloaded.task_prose.len(), 2);
        assert_eq!(reloaded.task_prose[0].prose, file.task_prose[0].prose);

        // Assert the JSON on disk has `task_prose` as a top-level
        // array, i.e. operators / `/summary` can load it without
        // needing deeper traversal into each Finding.
        let root: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(root.get("task_prose").and_then(|v| v.as_array()).is_some());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn first_apply_writes_canonical_file() {
        let dir = tmp_dir("create");
        let base = dir.join("findings.json");
        let store = FindingsStore::new(&base).await.unwrap();
        let rep = store
            .apply_delta(&[sample_finding("a")], Some("t1"), None)
            .await
            .unwrap();
        assert_eq!(rep.added, 1);
        assert_eq!(rep.updated, 0);
        assert!(rep.changed);
        assert_eq!(rep.turn_n, 1);
        assert!(base.exists());
        // Also verify jsondb stamped a `version` field on disk.
        let raw = std::fs::read_to_string(&base).unwrap();
        assert!(raw.contains("\"version\""));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn matching_id_updates_in_place_and_unions_symbols() {
        let dir = tmp_dir("merge");
        let base = dir.join("findings.json");
        let store = FindingsStore::new(&base).await.unwrap();
        let mut a = sample_finding("a");
        a.relevant_symbols.push(RelevantSymbol {
            name: "foo".into(),
            filename: "a.c".into(),
            line: 1,
            definition: "x".into(),
        });
        store.apply_delta(&[a], Some("t1"), None).await.unwrap();

        let mut b = sample_finding("a");
        b.summary = "fresh summary".into();
        b.relevant_symbols.push(RelevantSymbol {
            name: "bar".into(),
            filename: "b.c".into(),
            line: 2,
            definition: "y".into(),
        });
        let rep = store.apply_delta(&[b], Some("t2"), None).await.unwrap();
        assert_eq!(rep.added, 0);
        assert_eq!(rep.updated, 1);
        assert!(rep.changed);
        let snap = store.snapshot().await;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].summary, "fresh summary");
        assert_eq!(snap[0].relevant_symbols.len(), 2);
        assert_eq!(snap[0].first_seen_task.as_deref(), Some("t1"));
        assert_eq!(snap[0].last_updated_task.as_deref(), Some("t2"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn semantically_duplicate_active_findings_merge_by_anchor_and_identity() {
        let dir = tmp_dir("semantic-dup");
        let base = dir.join("findings.json");
        let store = FindingsStore::new(&base).await.unwrap();

        let mut first = sample_finding("geneve_xmit_skb_missing_tx_dstats");
        first.title = "geneve_xmit_skb misses tx dstats accounting".into();
        first.relevant_symbols.push(RelevantSymbol {
            name: "geneve_xmit_skb".into(),
            filename: "drivers/net/geneve.c".into(),
            line: 920,
            definition: "static netdev_tx_t geneve_xmit_skb(...)".into(),
        });
        store.apply_delta(&[first], Some("t1"), None).await.unwrap();

        let mut second = sample_finding("geneve_xmit_skb_missing_tx_stats");
        second.title = "geneve_xmit_skb misses tx stats accounting".into();
        second.summary = "longer duplicate summary proving the same accounting bug".into();
        second.relevant_symbols.push(RelevantSymbol {
            name: "geneve_xmit_skb".into(),
            filename: "drivers/net/geneve.c".into(),
            line: 920,
            definition: "static netdev_tx_t geneve_xmit_skb(...)".into(),
        });
        let rep = store
            .apply_delta(&[second], Some("t2"), None)
            .await
            .unwrap();

        assert_eq!(rep.added, 0);
        assert_eq!(rep.updated, 1);
        let snap = store.snapshot().await;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].id, "geneve_xmit_skb_missing_tx_dstats");
        assert_eq!(
            snap[0].summary,
            "longer duplicate summary proving the same accounting bug"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn semantic_duplicate_check_does_not_merge_different_bugs_in_same_function() {
        let dir = tmp_dir("semantic-distinct");
        let base = dir.join("findings.json");
        let store = FindingsStore::new(&base).await.unwrap();

        let mut accounting = sample_finding("geneve_xmit_skb_missing_tx_dstats");
        accounting.title = "geneve_xmit_skb misses tx dstats accounting".into();
        accounting.relevant_symbols.push(RelevantSymbol {
            name: "geneve_xmit_skb".into(),
            filename: "drivers/net/geneve.c".into(),
            line: 920,
            definition: "static netdev_tx_t geneve_xmit_skb(...)".into(),
        });
        let mut leak = sample_finding("geneve_xmit_skb_dst_leak_on_build_fail");
        leak.title = "geneve_xmit_skb leaks dst on build failure".into();
        leak.relevant_symbols.push(RelevantSymbol {
            name: "geneve_xmit_skb".into(),
            filename: "drivers/net/geneve.c".into(),
            line: 920,
            definition: "static netdev_tx_t geneve_xmit_skb(...)".into(),
        });

        store
            .apply_delta(&[accounting, leak], Some("t1"), None)
            .await
            .unwrap();
        let snap = store.snapshot().await;
        assert_eq!(snap.len(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn reactivate_flag_flips_invalidated_back_to_active() {
        let dir = tmp_dir("reactivate");
        let base = dir.join("findings.json");
        let store = FindingsStore::new(&base).await.unwrap();
        store
            .apply_delta(&[sample_finding("a")], Some("t1"), None)
            .await
            .unwrap();
        let inv = invalidating_delta("a", "the store is under the write lock");
        let rep2 = store.apply_delta(&[inv], Some("t2"), None).await.unwrap();
        assert_eq!(rep2.invalidated, 1);
        assert_eq!(rep2.reactivated, 0);
        assert_eq!(store.snapshot().await[0].status, Status::Invalidated);
        let mut reactive = sample_finding("a");
        reactive.status = Status::Active;
        reactive.reactivate = true;
        reactive.summary = "new evidence reverses it".into();
        let rep3 = store
            .apply_delta(&[reactive], Some("t3"), None)
            .await
            .unwrap();
        // The reactivation must be counted as such — not folded into
        // the generic "updated" bucket.
        assert_eq!(rep3.reactivated, 1);
        assert_eq!(rep3.invalidated, 0);
        assert_eq!(rep3.updated, 0);
        let snap = store.snapshot().await;
        assert_eq!(snap[0].status, Status::Active);
        // `reactivate` must not persist on the stored record.
        assert!(!snap[0].reactivate);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn invalidation_without_a_basis_is_refused() {
        // The 2026-08-20 arch/x86/kvm/mmu shape: a delta that says in
        // as many words that it cannot establish the claim, and sets
        // `invalidated` anyway ("status remains invalidated pending
        // confirmation of whether..."). The finding must survive.
        let dir = tmp_dir("no-basis");
        let store = FindingsStore::new(&dir.join("findings.json"))
            .await
            .unwrap();
        store
            .apply_delta(&[sample_finding("a")], Some("t1"), None)
            .await
            .unwrap();

        let mut bare = sample_finding("a");
        bare.status = Status::Invalidated;
        let rep = store.apply_delta(&[bare], Some("t2"), None).await.unwrap();
        assert_eq!(rep.invalidated, 0);
        assert_eq!(rep.invalidation_refused, 1);
        assert_eq!(store.snapshot().await[0].status, Status::Active);

        // A premise with no citation is an assertion, not evidence.
        let mut uncited = sample_finding("a");
        uncited.status = Status::Invalidated;
        uncited.invalidation = Some(InvalidationBasis {
            premise: "the flags are mutually exclusive".into(),
            evidence: vec![],
        });
        let rep = store
            .apply_delta(&[uncited], Some("t3"), None)
            .await
            .unwrap();
        assert_eq!(rep.invalidation_refused, 1);
        assert_eq!(store.snapshot().await[0].status, Status::Active);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn refused_invalidation_still_merges_the_rest_of_the_delta() {
        // Refusing the status flip must not cost the pass its work:
        // the symbols it fetched and the prose it wrote still land.
        let dir = tmp_dir("refused-merges");
        let store = FindingsStore::new(&dir.join("findings.json"))
            .await
            .unwrap();
        store
            .apply_delta(&[sample_finding("a")], Some("t1"), None)
            .await
            .unwrap();
        let mut bare = sample_finding("a");
        bare.status = Status::Invalidated;
        bare.summary = "a much longer summary carrying real new mechanism detail".into();
        let rep = store.apply_delta(&[bare], Some("t2"), None).await.unwrap();
        assert_eq!(rep.invalidation_refused, 1);
        assert_eq!(rep.updated, 1);
        let snap = store.snapshot().await;
        assert_eq!(snap[0].status, Status::Active);
        assert!(snap[0].summary.starts_with("a much longer"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn unconfirmed_leaves_invalidated_without_the_reactivate_flag() {
        // The cheap exit. A pass that knocks the premise down but
        // cannot yet show the bug fires must be able to say so, and
        // the recorded premise must not survive the move.
        let dir = tmp_dir("unconfirmed-exit");
        let store = FindingsStore::new(&dir.join("findings.json"))
            .await
            .unwrap();
        store
            .apply_delta(&[sample_finding("a")], Some("t1"), None)
            .await
            .unwrap();
        store
            .apply_delta(
                &[invalidating_delta(
                    "a",
                    "the two flags cannot be set together",
                )],
                Some("t2"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(store.snapshot().await[0].status, Status::Invalidated);

        let mut unsure = sample_finding("a");
        unsure.status = Status::Unconfirmed;
        let rep = store
            .apply_delta(&[unsure], Some("t3"), None)
            .await
            .unwrap();
        // Leaving Invalidated is a reactivation for counting purposes.
        assert_eq!(rep.reactivated, 1);
        let snap = store.snapshot().await;
        assert_eq!(snap[0].status, Status::Unconfirmed);
        assert!(snap[0].invalidation.is_none());

        // ...and Unconfirmed → Active needs no flag either.
        let mut back = sample_finding("a");
        back.status = Status::Active;
        store.apply_delta(&[back], Some("t4"), None).await.unwrap();
        assert_eq!(store.snapshot().await[0].status, Status::Active);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn plain_active_still_cannot_reverse_an_invalidation() {
        let dir = tmp_dir("active-no-flag");
        let store = FindingsStore::new(&dir.join("findings.json"))
            .await
            .unwrap();
        store
            .apply_delta(&[sample_finding("a")], Some("t1"), None)
            .await
            .unwrap();
        store
            .apply_delta(
                &[invalidating_delta("a", "the bound is enforced upstream")],
                Some("t2"),
                None,
            )
            .await
            .unwrap();
        let mut plain = sample_finding("a");
        plain.status = Status::Active;
        store.apply_delta(&[plain], Some("t3"), None).await.unwrap();
        let snap = store.snapshot().await;
        assert_eq!(snap[0].status, Status::Invalidated);
        assert_eq!(
            snap[0].invalidation.as_ref().map(|b| b.premise.as_str()),
            Some("the bound is enforced upstream")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_later_pass_can_replace_the_recorded_premise() {
        // An already-invalidated record is not "refused" when a delta
        // extends it without a basis — that is the ordinary shape of a
        // later pass adding detail — but a delta that DOES carry a new
        // premise replaces the old one, so the claim on file always
        // matches the argument currently being made.
        let dir = tmp_dir("premise-replace");
        let store = FindingsStore::new(&dir.join("findings.json"))
            .await
            .unwrap();
        store
            .apply_delta(&[sample_finding("a")], Some("t1"), None)
            .await
            .unwrap();
        store
            .apply_delta(
                &[invalidating_delta("a", "iterators filter mirror roots out")],
                Some("t2"),
                None,
            )
            .await
            .unwrap();

        let mut extend = sample_finding("a");
        extend.status = Status::Invalidated;
        let rep = store
            .apply_delta(&[extend], Some("t3"), None)
            .await
            .unwrap();
        assert_eq!(rep.invalidation_refused, 0);

        let narrowed = invalidating_delta("a", "the flags are exclusive in one request");
        store
            .apply_delta(&[narrowed], Some("t4"), None)
            .await
            .unwrap();
        let snap = store.snapshot().await;
        assert_eq!(
            snap[0].invalidation.as_ref().map(|b| b.premise.as_str()),
            Some("the flags are exclusive in one request")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn reactivate_wins_over_contradictory_invalidated_status() {
        // A misbehaving incoming delta that carries BOTH
        // `status: "invalidated"` AND `reactivate: true` must
        // resolve to Active and must not flip twice internally.
        // reactivate is the more specific signal and wins outright.
        let dir = tmp_dir("reactivate-wins");
        let base = dir.join("findings.json");
        let store = FindingsStore::new(&base).await.unwrap();
        store
            .apply_delta(&[sample_finding("a")], Some("t1"), None)
            .await
            .unwrap();
        let inv = invalidating_delta("a", "the store is under the write lock");
        store.apply_delta(&[inv], Some("t2"), None).await.unwrap();
        assert_eq!(store.snapshot().await[0].status, Status::Invalidated);
        let mut both = invalidating_delta("a", "the store is under the write lock");
        both.reactivate = true;
        store.apply_delta(&[both], Some("t3"), None).await.unwrap();
        let snap = store.snapshot().await;
        assert_eq!(snap[0].status, Status::Active);
        assert!(!snap[0].reactivate);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn shorter_incoming_prose_does_not_overwrite_longer_existing() {
        let dir = tmp_dir("downgrade");
        let base = dir.join("findings.json");
        let store = FindingsStore::new(&base).await.unwrap();
        let mut rich = sample_finding("a");
        rich.summary = "a detailed five-paragraph explanation with lots of context".into();
        rich.impact = "detailed impact statement with concrete code paths".into();
        rich.mechanism_detail = Some("rich mechanism context".into());
        rich.fix_sketch = Some("rich fix with file:line anchors".into());
        store.apply_delta(&[rich], Some("t1"), None).await.unwrap();
        let mut thin = sample_finding("a");
        thin.summary = "brief summary".into();
        thin.impact = "bad".into();
        thin.mechanism_detail = Some("terse".into());
        thin.fix_sketch = Some("patch".into());
        store.apply_delta(&[thin], Some("t2"), None).await.unwrap();
        let snap = store.snapshot().await;
        assert!(snap[0].summary.starts_with("a detailed"));
        assert!(snap[0].impact.starts_with("detailed"));
        assert_eq!(
            snap[0].mechanism_detail.as_deref(),
            Some("rich mechanism context")
        );
        assert_eq!(
            snap[0].fix_sketch.as_deref(),
            Some("rich fix with file:line anchors")
        );
        // last_updated_task still advances even when prose didn't win.
        assert_eq!(snap[0].last_updated_task.as_deref(), Some("t2"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn longer_incoming_prose_overwrites_existing() {
        let dir = tmp_dir("upgrade");
        let base = dir.join("findings.json");
        let store = FindingsStore::new(&base).await.unwrap();
        let mut thin = sample_finding("a");
        thin.summary = "short".into();
        store.apply_delta(&[thin], Some("t1"), None).await.unwrap();
        let mut rich = sample_finding("a");
        rich.summary = "much more detailed summary with concrete specifics".into();
        store.apply_delta(&[rich], Some("t2"), None).await.unwrap();
        let snap = store.snapshot().await;
        assert_eq!(
            snap[0].summary,
            "much more detailed summary with concrete specifics"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prefer_longer_helpers_behaviour() {
        let mut s = String::from("abcd");
        assert!(!prefer_longer(&mut s, ""));
        assert!(!prefer_longer(&mut s, "abcd"));
        assert!(!prefer_longer(&mut s, "xy")); // shorter stays
        assert_eq!(s, "abcd");
        assert!(prefer_longer(&mut s, "abcdef"));
        assert_eq!(s, "abcdef");

        let mut o: Option<String> = None;
        assert!(prefer_longer_opt(&mut o, &Some("hello".into())));
        assert_eq!(o.as_deref(), Some("hello"));
        assert!(!prefer_longer_opt(&mut o, &Some("hi".into())));
        assert_eq!(o.as_deref(), Some("hello"));
        assert!(prefer_longer_opt(&mut o, &Some("hello world".into())));
        assert_eq!(o.as_deref(), Some("hello world"));
        assert!(!prefer_longer_opt(&mut o, &None));
        assert!(!prefer_longer_opt(&mut o, &Some("".into())));
    }

    #[tokio::test]
    async fn invalidation_flips_status_without_losing_body() {
        let dir = tmp_dir("invalidate");
        let base = dir.join("findings.json");
        let store = FindingsStore::new(&base).await.unwrap();
        store
            .apply_delta(&[sample_finding("a")], Some("t1"), None)
            .await
            .unwrap();
        let mut inv = invalidating_delta("a", "the caller already holds the write lock");
        inv.summary = "".into(); // empty: don't overwrite
        let rep = store.apply_delta(&[inv], Some("t2"), None).await.unwrap();
        assert_eq!(rep.invalidated + rep.updated, 1);
        let snap = store.snapshot().await;
        assert_eq!(snap[0].status, Status::Invalidated);
        assert_eq!(snap[0].summary, "s");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn introduced_by_takes_first_attribution_and_latest_wins() {
        let dir = tmp_dir("introduced-by");
        let base = dir.join("findings.json");
        let store = FindingsStore::new(&base).await.unwrap();
        store
            .apply_delta(&[sample_finding("a")], Some("t1"), None)
            .await
            .unwrap();
        assert!(store.snapshot().await[0].introduced_by.is_none());
        // Empty sha is a no-op.
        let mut noop = sample_finding("a");
        noop.introduced_by = Some(IntroducedBy {
            sha: "".into(),
            subject: "ignored".into(),
        });
        store.apply_delta(&[noop], Some("t2"), None).await.unwrap();
        assert!(store.snapshot().await[0].introduced_by.is_none());
        // First real attribution sticks.
        let mut first = sample_finding("a");
        first.introduced_by = Some(IntroducedBy {
            sha: "abc".into(),
            subject: "short".into(),
        });
        store.apply_delta(&[first], Some("t3"), None).await.unwrap();
        let snap = store.snapshot().await;
        let ib = snap[0].introduced_by.as_ref().unwrap();
        assert_eq!(ib.sha, "abc");
        assert_eq!(ib.subject, "short");
        // Same sha, longer subject → subject upgraded.
        let mut upgrade = sample_finding("a");
        upgrade.introduced_by = Some(IntroducedBy {
            sha: "abc".into(),
            subject: "a much longer subject line".into(),
        });
        store
            .apply_delta(&[upgrade], Some("t4"), None)
            .await
            .unwrap();
        let ib = store.snapshot().await[0].introduced_by.clone().unwrap();
        assert_eq!(ib.subject, "a much longer subject line");
        // Different sha → latest wins.
        let mut reattrib = sample_finding("a");
        reattrib.introduced_by = Some(IntroducedBy {
            sha: "def".into(),
            subject: "re-attributed".into(),
        });
        store
            .apply_delta(&[reattrib], Some("t5"), None)
            .await
            .unwrap();
        let ib = store.snapshot().await[0].introduced_by.clone().unwrap();
        assert_eq!(ib.sha, "def");
        assert_eq!(ib.subject, "re-attributed");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn first_seen_at_stamps_on_insert_and_never_shifts() {
        let dir = tmp_dir("first-seen");
        let base = dir.join("findings.json");
        let store = FindingsStore::new(&base).await.unwrap();
        store
            .apply_delta(&[sample_finding("a")], Some("t1"), None)
            .await
            .unwrap();
        let ts_initial = store.snapshot().await[0].first_seen_at.unwrap();
        // Second delta on the same id must NOT bump the stamp.
        let mut updated = sample_finding("a");
        updated.summary = "now with more detail".into();
        store
            .apply_delta(&[updated], Some("t2"), None)
            .await
            .unwrap();
        let ts_after = store.snapshot().await[0].first_seen_at.unwrap();
        assert_eq!(
            ts_initial, ts_after,
            "first_seen_at must be stable across merges"
        );
        // An incoming delta that carries an explicit first_seen_at
        // for a NEW id is preserved (import / migration path).
        let mut imported = sample_finding("b");
        let pinned = chrono::DateTime::parse_from_rfc3339("2020-01-02T03:04:05Z")
            .unwrap()
            .with_timezone(&Utc);
        imported.first_seen_at = Some(pinned);
        store
            .apply_delta(&[imported], Some("t3"), None)
            .await
            .unwrap();
        let b = store
            .snapshot()
            .await
            .into_iter()
            .find(|f| f.id == "b")
            .unwrap();
        assert_eq!(b.first_seen_at, Some(pinned));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn severity_only_escalates() {
        let dir = tmp_dir("severity");
        let base = dir.join("findings.json");
        let store = FindingsStore::new(&base).await.unwrap();
        let mut hi = sample_finding("a");
        hi.severity = Severity::High;
        store.apply_delta(&[hi], Some("t1"), None).await.unwrap();
        let mut lo = sample_finding("a");
        lo.severity = Severity::Low;
        store.apply_delta(&[lo], Some("t2"), None).await.unwrap();
        let snap = store.snapshot().await;
        assert_eq!(snap[0].severity, Severity::High);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn reload_preserves_findings() {
        let dir = tmp_dir("reload");
        let base = dir.join("findings.json");
        {
            let store = FindingsStore::new(&base).await.unwrap();
            store
                .apply_delta(
                    &[sample_finding("a"), sample_finding("b")],
                    Some("t1"),
                    None,
                )
                .await
                .unwrap();
            store.db.flush().await;
        }
        let store = FindingsStore::new(&base).await.unwrap();
        let snap = store.snapshot().await;
        assert_eq!(snap.len(), 2);
        assert_eq!(store.last_turn().await, 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn tasks_since_change_resets_on_change() {
        let dir = tmp_dir("tsc");
        let base = dir.join("findings.json");
        let store = FindingsStore::new(&base).await.unwrap();
        // Empty delta = no change.
        let r0 = store.apply_delta(&[], Some("t0"), None).await.unwrap();
        assert!(!r0.changed);
        assert_eq!(r0.tasks_since_change, 1);
        let r1 = store.apply_delta(&[], Some("t1"), None).await.unwrap();
        assert_eq!(r1.tasks_since_change, 2);
        let r2 = store
            .apply_delta(&[sample_finding("a")], Some("t2"), None)
            .await
            .unwrap();
        assert!(r2.changed);
        assert_eq!(r2.tasks_since_change, 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn relevant_subset_matches_on_id_mention() {
        let f = sample_finding("race_in_cq_ack");
        let sub = relevant_subset("This reinforces finding race_in_cq_ack — see more.", &[f]);
        assert_eq!(sub.len(), 1);
    }

    #[test]
    fn relevant_subset_matches_on_filename_basename() {
        let mut f = sample_finding("x");
        f.relevant_symbols.push(RelevantSymbol {
            name: "foo".into(),
            filename: "drivers/net/ethernet/intel/ice/ice_main.c".into(),
            line: 100,
            definition: "".into(),
        });
        let sub1 = relevant_subset("See ice_main.c:42 for details.", &[f.clone()]);
        assert_eq!(sub1.len(), 1);
        let sub2 = relevant_subset(
            "See drivers/net/ethernet/intel/ice/ice_main.c:42.",
            &[f.clone()],
        );
        assert_eq!(sub2.len(), 1);
        let sub3 = relevant_subset("Nothing relevant here.", &[f]);
        assert!(sub3.is_empty());
    }

    #[test]
    fn relevant_subset_matches_on_symbol_name_boundary() {
        let mut f = sample_finding("x");
        f.relevant_symbols.push(RelevantSymbol {
            name: "cpu_mask".into(),
            filename: "lib/cpumask.c".into(),
            line: 10,
            definition: "".into(),
        });
        // Whole-word match: `cpu_mask` in a sentence → hit.
        let sub1 = relevant_subset("The cpu_mask buffer is freed.", &[f.clone()]);
        assert_eq!(sub1.len(), 1);
        // Embedded inside `cpu_mask_var` → NOT a hit via identifier
        // match (identifier boundary enforced).
        let mut g = sample_finding("y");
        g.relevant_symbols.push(RelevantSymbol {
            name: "cpu_mask".into(),
            filename: "lib/other.c".into(),
            line: 20,
            definition: "".into(),
        });
        let sub2 = relevant_subset("Only cpu_mask_var mentioned.", &[g]);
        assert!(sub2.is_empty());
    }

    #[test]
    fn relevant_subset_includes_generously_on_any_signal() {
        // A finding should be included if ANY of id / filename /
        // symbol-name matches — not all of them.
        let mut f = sample_finding("race_x");
        f.relevant_symbols.push(RelevantSymbol {
            name: "completely_unrelated".into(),
            filename: "a/b/c.c".into(),
            line: 1,
            definition: "".into(),
        });
        let sub = relevant_subset("reinforces finding race_x — see details", &[f]);
        assert_eq!(sub.len(), 1);
    }

    #[test]
    fn relevant_subset_empty_inputs() {
        assert!(relevant_subset("", &[sample_finding("x")]).is_empty());
        assert!(relevant_subset("some prose", &[]).is_empty());
    }

    #[test]
    fn optional_fields_serialise_only_when_present() {
        let mut f = sample_finding("x");
        f.fix_sketch = None;
        f.mechanism_detail = None;
        let s = serde_json::to_string(&f).unwrap();
        assert!(!s.contains("fix_sketch"));
        assert!(!s.contains("mechanism_detail"));

        f.fix_sketch = Some("cache bool".to_string());
        let s = serde_json::to_string(&f).unwrap();
        assert!(s.contains("\"fix_sketch\":\"cache bool\""));
    }

    #[test]
    fn severity_and_status_serde() {
        let f = sample_finding("x");
        let s = serde_json::to_string(&f).unwrap();
        assert!(s.contains("\"severity\":\"high\""));
        assert!(s.contains("\"status\":\"active\""));
    }
}

#[cfg(test)]
mod schema_contract_tests {
    use super::*;

    /// The schema the model is shown must agree with the prompt: only
    /// `id` is unconditionally required, because an update is a legal
    /// record. Completeness of a NEW finding is enforced by
    /// `apply_delta_to_list`, which can see whether the id exists.
    #[test]
    fn only_id_is_required_on_the_wire() {
        let schema = serde_json::to_value(schemars::schema_for!(Finding)).unwrap();
        let required: Vec<String> = schema["required"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(required, vec!["id".to_string()], "got {required:?}");
        let props = schema["properties"].as_object().expect("properties");
        for field in ["id", "title", "severity", "summary", "status"] {
            assert!(
                props.contains_key(field),
                "`{field}` missing from the schema"
            );
        }
    }
}
