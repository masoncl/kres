//! Persisted session state: plan + todo + deferred + counters,
//! written to `<results>/session.json` so an interrupted session
//! can be resumed on the next invocation pointed at the same
//! results directory.
//!
//! Two invariants:
//!
//! 1. **Atomic writes.** Every persist goes through a tmp-file +
//!    fsync + rename dance (mirrors [`crate::findings`] write
//!    discipline) so a crash mid-write cannot leave half a JSON
//!    blob behind for the next session to choke on.
//!
//! 2. **InProgress is not durable.** When loading a snapshot we
//!    flip every `TodoStatus::InProgress` todo back to `Pending`.
//!    An in-progress task belonged to a process that no longer
//!    exists; the only honest thing to do is re-queue it for the
//!    resumed session to pick up. The same rule applies to
//!    [`crate::PlanStepStatus::InProgress`].

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::plan::{Plan, PlanStepStatus};
use crate::todo::{TodoItem, TodoStatus};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewFileScanState {
    pub target: String,
    pub source_hash: String,
    pub baseline: String,
    pub head: String,
    pub scan: String,
}

#[derive(Debug, Error)]
pub enum SessionStateError {
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported session version {0}")]
    UnsupportedVersion(u32),
}

/// Versioned snapshot of everything needed to resume a session.
///
/// The `version` field is for forward compat: future schema
/// changes bump it and loaders decide how to migrate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionState {
    pub version: u32,
    /// Last operator prompt submitted. Useful for `--resume`
    /// reporting; not required for correctness.
    #[serde(default)]
    pub last_prompt: Option<String>,
    #[serde(default)]
    pub plan: Option<Plan>,
    /// Completed whole-file review bootstrap scan. Stored separately from
    /// `Plan::prompt` so task agents do not receive the scan twice and resume
    /// does not need to regenerate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_file_scan: Option<ReviewFileScanState>,
    #[serde(default)]
    pub todo: Vec<TodoItem>,
    /// Items parked by `/stop`, goal-met, or `--turns` drain.
    #[serde(default)]
    pub deferred: Vec<TodoItem>,
    /// Counter against `--turns N`. Persisted so a resumed session
    /// picks up the cap rather than starting over at 0.
    #[serde(default)]
    pub completed_run_count: u32,
}

fn default_version() -> u32 {
    3
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            version: default_version(),
            last_prompt: None,
            plan: None,
            review_file_scan: None,
            todo: Vec::new(),
            deferred: Vec::new(),
            completed_run_count: 0,
        }
    }
}

impl SessionState {
    /// Default filename inside a results dir.
    pub const FILENAME: &'static str = "session.json";

    pub fn path_in(dir: &Path) -> PathBuf {
        dir.join(Self::FILENAME)
    }

    /// Load a previously-persisted snapshot. Returns `Ok(None)`
    /// when the file does not exist (fresh session); `Err` only on
    /// real I/O or parse failures.
    ///
    /// Post-load hygiene: any `InProgress` todo / plan step is
    /// flipped to `Pending`, since its prior executor is gone.
    pub fn load(path: &Path) -> Result<Option<Self>, SessionStateError> {
        match std::fs::read_to_string(path) {
            Ok(s) => {
                let mut state: Self = serde_json::from_str(&s)?;
                if state.version != default_version() {
                    return Err(SessionStateError::UnsupportedVersion(state.version));
                }
                state.normalise_inprogress();
                Ok(Some(state))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Flip every in-progress todo and plan step back to pending.
    pub fn normalise_inprogress(&mut self) {
        for item in self.todo.iter_mut() {
            if item.status == TodoStatus::InProgress {
                item.status = TodoStatus::Pending;
            }
        }
        for item in self.deferred.iter_mut() {
            if item.status == TodoStatus::InProgress {
                item.status = TodoStatus::Pending;
            }
        }
        if let Some(p) = self.plan.as_mut() {
            for step in p.steps.iter_mut() {
                if step.status == PlanStepStatus::InProgress {
                    step.status = PlanStepStatus::Pending;
                }
            }
        }
    }

    /// Persist to `path` via tmp-file + fsync + rename + parent-dir
    /// fsync. The parent-dir fsync is what actually makes the rename
    /// durable on ext4/xfs after a power loss (mirrors the
    /// findings.rs H6 discipline). Creates the parent directory if
    /// missing and it is non-empty.
    pub fn save(&self, path: &Path) -> Result<(), SessionStateError> {
        // Only create the parent when it has a non-empty name:
        // `Path::new("foo.json").parent()` is `Some("")`, and
        // `create_dir_all("")` errors with NotFound on Unix.
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let body = serde_json::to_vec_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        {
            let mut f = File::create(&tmp)?;
            f.write_all(&body)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)?;
        // Fsync the containing directory so the rename itself is on
        // stable storage — without this a power loss right after
        // rename() can leave the directory entry pointing at nothing.
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                if let Ok(dir) = File::open(parent) {
                    let _ = dir.sync_all();
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode::TaskMode;
    use crate::plan::PlanStep;

    fn td(name: &str, status: TodoStatus) -> TodoItem {
        let mut t = TodoItem::new(name, "investigate");
        t.status = status;
        t
    }

    #[test]
    fn roundtrip_empty() {
        let dir = tempfile::tempdir().unwrap();
        let p = SessionState::path_in(dir.path());
        let s = SessionState::default();
        s.save(&p).unwrap();
        let loaded = SessionState::load(&p).unwrap().unwrap();
        assert_eq!(loaded.version, 3);
        assert!(loaded.todo.is_empty());
        assert!(loaded.plan.is_none());
        assert!(loaded.review_file_scan.is_none());
    }

    #[test]
    fn review_file_scan_roundtrips_outside_plan_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let p = SessionState::path_in(dir.path());
        let state = SessionState {
            plan: Some(Plan::new("review mm/vmscan.c", "goal", TaskMode::Audit)),
            review_file_scan: Some(ReviewFileScanState {
                target: "mm/vmscan.c".into(),
                source_hash: "source-hash".into(),
                baseline: "baseline".into(),
                head: "head".into(),
                scan: r#"{"functions":[{"name":"shrink_node","risk_rating":81}]}"#.into(),
            }),
            ..Default::default()
        };

        state.save(&p).unwrap();
        let loaded = SessionState::load(&p).unwrap().unwrap();

        assert_eq!(loaded.plan.unwrap().prompt, "review mm/vmscan.c");
        assert_eq!(
            loaded.review_file_scan,
            Some(ReviewFileScanState {
                target: "mm/vmscan.c".into(),
                source_hash: "source-hash".into(),
                baseline: "baseline".into(),
                head: "head".into(),
                scan: r#"{"functions":[{"name":"shrink_node","risk_rating":81}]}"#.into(),
            })
        );
    }

    #[test]
    fn load_missing_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("nope.json");
        assert!(SessionState::load(&p).unwrap().is_none());
    }

    #[test]
    fn inprogress_todos_flip_to_pending_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let p = SessionState::path_in(dir.path());
        let s = SessionState {
            todo: vec![
                td("a", TodoStatus::InProgress),
                td("b", TodoStatus::Pending),
                td("c", TodoStatus::Done),
            ],
            deferred: vec![td("d", TodoStatus::InProgress)],
            ..Default::default()
        };
        s.save(&p).unwrap();
        let loaded = SessionState::load(&p).unwrap().unwrap();
        assert_eq!(loaded.todo[0].status, TodoStatus::Pending);
        assert_eq!(loaded.todo[1].status, TodoStatus::Pending);
        assert_eq!(loaded.todo[2].status, TodoStatus::Done);
        assert_eq!(loaded.deferred[0].status, TodoStatus::Pending);
    }

    #[test]
    fn inprogress_plan_steps_flip_to_pending_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let p = SessionState::path_in(dir.path());
        let mut plan = Plan::new("prompt", "goal", TaskMode::Audit);
        let mut step = PlanStep::new("s1", "t");
        step.status = PlanStepStatus::InProgress;
        plan.steps.push(step);
        let s = SessionState {
            plan: Some(plan),
            ..Default::default()
        };
        s.save(&p).unwrap();
        let loaded = SessionState::load(&p).unwrap().unwrap();
        assert_eq!(
            loaded.plan.unwrap().steps[0].status,
            PlanStepStatus::Pending
        );
    }

    #[test]
    fn atomic_write_overwrites_prior() {
        let dir = tempfile::tempdir().unwrap();
        let p = SessionState::path_in(dir.path());
        SessionState {
            completed_run_count: 1,
            ..Default::default()
        }
        .save(&p)
        .unwrap();
        SessionState {
            completed_run_count: 7,
            ..Default::default()
        }
        .save(&p)
        .unwrap();
        let loaded = SessionState::load(&p).unwrap().unwrap();
        assert_eq!(loaded.completed_run_count, 7);
    }

    #[test]
    fn missing_or_unsupported_version_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let p = SessionState::path_in(dir.path());
        std::fs::write(&p, "{}").unwrap();
        assert!(matches!(
            SessionState::load(&p),
            Err(SessionStateError::Json(_))
        ));
        let state = SessionState {
            version: 99,
            ..SessionState::default()
        };
        state.save(&p).unwrap();
        assert!(matches!(
            SessionState::load(&p),
            Err(SessionStateError::UnsupportedVersion(99))
        ));
    }

    #[test]
    fn save_removes_tmp_file_on_success() {
        // Rename should consume the tmp file; nothing suffixed
        // `.json.tmp` should linger next to the canonical path.
        let dir = tempfile::tempdir().unwrap();
        let p = SessionState::path_in(dir.path());
        SessionState::default().save(&p).unwrap();
        let tmp = p.with_extension("json.tmp");
        assert!(!tmp.exists(), "tmp left behind at {}", tmp.display());
    }

    #[test]
    fn save_creates_missing_parent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/b/c");
        let p = nested.join("session.json");
        SessionState::default().save(&p).unwrap();
        assert!(p.exists());
    }

    #[test]
    fn populated_plan_survives_roundtrip() {
        // End-to-end guard: a plan produced by define_plan + stored
        // via set_plan must come back out of session.json with the
        // same id / title / description / status on every step, and
        // every non-InProgress step must keep its status (the
        // normalise_inprogress pass only touches InProgress).
        let dir = tempfile::tempdir().unwrap();
        let p = SessionState::path_in(dir.path());
        let mut plan = Plan::new(
            "review fs/btrfs for memory bugs",
            "identify every UAF / leak / double-free in fs/btrfs",
            TaskMode::Audit,
        );
        let mut s1 = PlanStep::new("s1", "audit accessors.c");
        s1.description = "walk each btrfs_set_*/btrfs_get_* helper".into();
        s1.status = PlanStepStatus::Done;
        s1.todo_ids = vec!["t-accessors".into()];
        plan.steps.push(s1);
        let s2 = PlanStep::new("s2", "audit ordered-data.c");
        plan.steps.push(s2);
        let s3 = PlanStep::new("s3", "audit free-space-cache.c");
        plan.steps.push(s3);
        let state = SessionState {
            plan: Some(plan),
            last_prompt: Some("review fs/btrfs for memory bugs".into()),
            ..Default::default()
        };
        state.save(&p).unwrap();
        let loaded = SessionState::load(&p).unwrap().unwrap();
        let lp = loaded.plan.expect("plan round-tripped");
        assert_eq!(lp.steps.len(), 3);
        assert_eq!(lp.steps[0].id, "s1");
        assert_eq!(lp.steps[0].status, PlanStepStatus::Done);
        assert_eq!(lp.steps[0].todo_ids, vec!["t-accessors".to_string()]);
        assert_eq!(
            lp.steps[0].description,
            "walk each btrfs_set_*/btrfs_get_* helper"
        );
        assert_eq!(lp.steps[1].id, "s2");
        assert_eq!(lp.steps[1].status, PlanStepStatus::Pending);
        assert_eq!(lp.mode, TaskMode::Audit);
        assert_eq!(
            loaded.last_prompt.as_deref(),
            Some("review fs/btrfs for memory bugs")
        );
    }
}
