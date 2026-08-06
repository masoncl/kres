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
pub mod brace;
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
pub mod shutdown;
pub mod task;
pub mod todo;

pub use artifact::{
    auto_generated_fix_link, auto_generated_fix_name, clear_invalidation_artifacts,
    ensure_artifact_dir_files, mark_fixes_invalidated, patch_file_matches_head,
    patch_file_matches_head_named, read_finding_bugs, record_auto_generated_fix,
    record_auto_generated_fix_named, set_finding_bugs, set_finding_results,
    set_finding_status_files, validate_metadata_yaml_content, write_invalidation_artifact,
    write_partial_invalidation_artifact, FindingBug, FindingResult, AUTO_GENERATED_FIX_LINK,
    AUTO_GENERATED_FIX_NAME, INVALIDATED_FIX_PREFIX, INVALIDATION_NAME, PARTIAL_INVALIDATION_NAME,
    SUMMARY_CROSS_LINK,
};
pub use consent::ConsentStore;
pub use cost::{format_token_count, format_usage_summary, UsageEntry, UsageKey, UsageTracker};
pub use findings::{
    apply_delta_to_list, findings_for_prompt_history, redact_findings_for_agent, relevant_subset,
    ApplyReport, DeltaCounts, Finding, FindingDetail, FindingsFile, FindingsStore, Severity,
    Status,
};
pub use lens::LensSpec;
pub use log::{
    duplicate_symbol_bodies_in_context, ContextStats, LoggedUsage, RequestMeta, TurnLogger,
};
pub use mode::{CodeEdit, CodeFile, TaskMode};
pub use plan::{
    extract_embedded_plan, Plan, PlanPromptView, PlanRewrite, PlanStep, PlanStepPromptView,
    PlanStepStatus,
};
pub use session_state::{ReviewFileScanState, SessionState, SessionStateError};
pub use shutdown::Shutdown;
pub use task::{ReadyTodos, ReapedTask, Task, TaskId, TaskManager, TaskState, TodoClaims};
pub use todo::{TodoItem, TodoStatus};

pub mod version {
    pub const VERSION: &str = env!("CARGO_PKG_VERSION");
}
