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
//!   and `status: invalidated` on an existing id marks it.
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum Status {
    #[default]
    Active,
    Invalidated,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelevantSymbol {
    pub name: String,
    pub filename: String,
    pub line: u32,
    pub definition: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelevantFileSection {
    pub filename: String,
    pub line_start: u32,
    pub line_end: u32,
    pub content: String,
}

/// Per-task narrative detail captured on a Finding. Each entry is
/// the full analysis prose produced by one task that touched this
/// finding (either on its introductory add or on a subsequent
/// update). Consumed by `/summary` so the plain-text summary can
/// pull richer exposition than the short `summary` /
/// `reproducer_sketch` / `impact` fields carry.
///
/// These entries are NEVER forwarded to another LLM call. Every
/// site that hands findings to an agent strips the field first —
/// slow-agent `previous_findings`, consolidator lens outputs
/// (which come from freshly-deserialised agent replies and don't
/// carry it anyway), and the promoter's narrowed existing_findings
/// all run through [`Finding::redacted_for_agent`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindingDetail {
    /// Provenance stamp. Same format as `last_updated_task` —
    /// `"<uuid-simple>/<todo-tag>"` or bare uuid.
    pub task: String,
    /// The task's effective_analysis prose verbatim.
    pub analysis: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub id: String,
    pub title: String,
    pub severity: Severity,
    #[serde(default)]
    pub status: Status,
    #[serde(default)]
    pub relevant_symbols: Vec<RelevantSymbol>,
    #[serde(default)]
    pub relevant_file_sections: Vec<RelevantFileSection>,
    pub summary: String,
    pub reproducer_sketch: String,
    pub impact: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mechanism_detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix_sketch: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_questions: Vec<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_seen_task: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_updated_task: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related_finding_ids: Vec<String>,

    /// Per-task narrative captured from the task's effective_analysis
    /// at apply_delta time. Purely for `/summary` generation — NEVER
    /// forwarded to another LLM. Call [`Finding::redacted_for_agent`]
    /// before handing findings to any agent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<FindingDetail>,

    /// Wire-only signal: when `true` on an incoming delta AND the
    /// matching-id existing record is `Status::Invalidated`, the
    /// existing record flips back to `Status::Active`. Intended for
    /// slow-agent turns that discover new evidence reversing a
    /// prior invalidation (see slow-code-agent.system.md). Never
    /// serialized on stored records — `merge_into` consumes the
    /// signal and doesn't propagate it; on a new-id apply the flag
    /// is stripped before the entry enters the list.
    #[serde(default, skip_serializing_if = "is_false")]
    pub reactivate: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl Finding {
    /// Return a clone suitable for inclusion in an LLM prompt —
    /// `details` cleared so the agent doesn't see the per-task
    /// narrative captured for /summary. Keep every other field.
    pub fn redacted_for_agent(&self) -> Finding {
        let mut c = self.clone();
        c.details.clear();
        c
    }
}

/// Apply [`Finding::redacted_for_agent`] to every entry. Convenience
/// for the common case where a whole slice is about to be shipped
/// to an agent.
pub fn redact_findings_for_agent(findings: &[Finding]) -> Vec<Finding> {
    findings.iter().map(Finding::redacted_for_agent).collect()
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
}

impl SchemaV0 for FindingsFile {
    /// Legacy findings.json files were written by the pre-jsondb
    /// store and have no top-level `version` field. Treat those as V0.
    const VERSION_OPTIONAL: bool = true;
}

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
            changed: counts.changed,
            turn_n: next_turn,
            tasks_since_change,
        })
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
                let changed = merge_into(&mut current[idx], incoming, task_id);
                record_detail(&mut current[idx], task_id, task_analysis);
                if changed {
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
                let mut new_entry = incoming.clone();
                // `reactivate` is a transient wire signal; don't let
                // it persist on a newly-inserted record. Same for any
                // stray details an incoming delta tried to carry —
                // details is a store-local concept, not a wire
                // contract the agents know about.
                new_entry.reactivate = false;
                new_entry.details.clear();
                if let Some(t) = task_id {
                    if new_entry.first_seen_task.is_none() {
                        new_entry.first_seen_task = Some(t.to_string());
                    }
                    new_entry.last_updated_task = Some(t.to_string());
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
fn merge_into(existing: &mut Finding, incoming: &Finding, task_id: Option<&str>) -> bool {
    let mut changed = false;

    // Status transitions. `reactivate: true` is a specific, rarer
    // signal; when set it WINS regardless of what `status` carries.
    // Treating reactivate as authoritative avoids a contradictory
    // delta (both `status: "invalidated"` and `reactivate: true`)
    // producing a transient Active→Invalidated→Active flip with
    // `changed` set twice for what is really a single transition.
    if incoming.reactivate {
        if existing.status == Status::Invalidated {
            existing.status = Status::Active;
            changed = true;
        }
    } else if incoming.status == Status::Invalidated
        && existing.status != Status::Invalidated
    {
        existing.status = Status::Invalidated;
        changed = true;
    }

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

    // Union collections.
    changed |= union_symbols(&mut existing.relevant_symbols, &incoming.relevant_symbols);
    changed |= union_sections(
        &mut existing.relevant_file_sections,
        &incoming.relevant_file_sections,
    );
    changed |= union_strings(&mut existing.open_questions, &incoming.open_questions);
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

    changed
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
            details: vec![],
        }
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
    async fn reactivate_flag_flips_invalidated_back_to_active() {
        let dir = tmp_dir("reactivate");
        let base = dir.join("findings.json");
        let store = FindingsStore::new(&base).await.unwrap();
        store
            .apply_delta(&[sample_finding("a")], Some("t1"), None)
            .await
            .unwrap();
        let mut inv = sample_finding("a");
        inv.status = Status::Invalidated;
        let rep2 = store.apply_delta(&[inv], Some("t2"), None).await.unwrap();
        assert_eq!(rep2.invalidated, 1);
        assert_eq!(rep2.reactivated, 0);
        assert_eq!(store.snapshot().await[0].status, Status::Invalidated);
        let mut reactive = sample_finding("a");
        reactive.status = Status::Active;
        reactive.reactivate = true;
        reactive.summary = "new evidence reverses it".into();
        let rep3 = store.apply_delta(&[reactive], Some("t3"), None).await.unwrap();
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
        let mut inv = sample_finding("a");
        inv.status = Status::Invalidated;
        store.apply_delta(&[inv], Some("t2"), None).await.unwrap();
        assert_eq!(store.snapshot().await[0].status, Status::Invalidated);
        let mut both = sample_finding("a");
        both.status = Status::Invalidated;
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
        assert_eq!(snap[0].mechanism_detail.as_deref(), Some("rich mechanism context"));
        assert_eq!(snap[0].fix_sketch.as_deref(), Some("rich fix with file:line anchors"));
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
        assert_eq!(snap[0].summary, "much more detailed summary with concrete specifics");
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
        let mut inv = sample_finding("a");
        inv.status = Status::Invalidated;
        inv.summary = "".into(); // empty: don't overwrite
        let rep = store.apply_delta(&[inv], Some("t2"), None).await.unwrap();
        assert_eq!(rep.invalidated + rep.updated, 1);
        let snap = store.snapshot().await;
        assert_eq!(snap[0].status, Status::Invalidated);
        assert_eq!(snap[0].summary, "s");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn severity_only_escalates() {
        let dir = tmp_dir("severity");
        let base = dir.join("findings.json");
        let store = FindingsStore::new(&base).await.unwrap();
        let mut hi = sample_finding("a");
        hi.severity = Severity::Critical;
        store.apply_delta(&[hi], Some("t1"), None).await.unwrap();
        let mut lo = sample_finding("a");
        lo.severity = Severity::Low;
        store.apply_delta(&[lo], Some("t2"), None).await.unwrap();
        let snap = store.snapshot().await;
        assert_eq!(snap[0].severity, Severity::Critical);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn reload_preserves_findings() {
        let dir = tmp_dir("reload");
        let base = dir.join("findings.json");
        {
            let store = FindingsStore::new(&base).await.unwrap();
            store
                .apply_delta(&[sample_finding("a"), sample_finding("b")], Some("t1"), None)
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

    #[tokio::test]
    async fn legacy_unversioned_file_loads() {
        // A pre-jsondb findings.json has no `version` field. Because
        // FindingsFile: SchemaV0 with VERSION_OPTIONAL = true, jsondb
        // is supposed to accept it as V0.
        let dir = tmp_dir("legacy");
        let base = dir.join("findings.json");
        std::fs::write(
            &base,
            r#"{"findings":[{"id":"old","title":"t","severity":"high","summary":"s","reproducer_sketch":"r","impact":"i"}],"tasks_since_change":2,"turn_n":7}"#,
        )
        .unwrap();
        let store = FindingsStore::new(&base).await.unwrap();
        let snap = store.snapshot().await;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].id, "old");
        assert_eq!(store.tasks_since_change().await, 2);
        assert_eq!(store.last_turn().await, 7);
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
