//! Core kres types: tasks, shutdown, findings.
//!
//! Closes bugs.md items:
//! - C1: all TaskManager shared state lives inside an RwLock; no caller
//!   can iterate without taking it.
//! - C2: every Task owns a CancellationToken; /stop / /clear / goal-met
//!   / --turns propagate cancel before dropping references.
//! - C3: abandoning a Task waits for its handle, never strands it.
//! - H1: no LLM call runs inside the findings-extract critical
//!   section; `FindingsStore::apply_delta` does a pure Rust merge
//!   and the jsondb-owned RwLock serialises disk writes.
//! - H6: the canonical findings.json is written via jsondb's
//!   tmp-file + fsync + rename pipeline (no history snapshots).
//! - L1: no parallel "completed_ids" vector — done tasks are queried
//!   directly off the ordered task list.

pub mod artifact;
pub mod consent;
pub mod cost;
pub mod findings;
pub mod io;
pub mod lens;
pub mod log;
pub mod mode;
pub mod plan;
pub mod preview;
pub mod session_state;
pub mod shrink;
pub mod shutdown;
pub mod task;
pub mod todo;

pub use artifact::{
    auto_generated_fix_link, auto_generated_fix_name, ensure_artifact_dir_files,
    patch_file_matches_head, patch_file_matches_head_named, record_auto_generated_fix,
    record_auto_generated_fix_named, set_finding_status_files, AUTO_GENERATED_FIX_LINK,
    AUTO_GENERATED_FIX_NAME, SUMMARY_CROSS_LINK,
};
pub use consent::ConsentStore;
pub use cost::{UsageEntry, UsageKey, UsageTracker};
pub use findings::{
    apply_delta_to_list, redact_findings_for_agent, relevant_subset, ApplyReport, DeltaCounts,
    Finding, FindingDetail, FindingsFile, FindingsStore, Severity, Status,
};
pub use lens::LensSpec;
pub use log::{LoggedUsage, TurnLogger};
pub use mode::{CodeEdit, CodeFile, TaskMode};
pub use plan::{extract_embedded_plan, Plan, PlanRewrite, PlanStep, PlanStepStatus};
pub use session_state::{SessionState, SessionStateError};
pub use shrink::{
    estimate_tokens, finding_char_size, fit_payload, shrink_findings_to_budget,
    shrink_last_user_message, total_char_size,
};
pub use shutdown::Shutdown;
pub use task::{ReapedTask, Task, TaskId, TaskManager, TaskState};
pub use todo::{TodoItem, TodoStatus};

pub mod version {
    pub const VERSION: &str = env!("CARGO_PKG_VERSION");
}
