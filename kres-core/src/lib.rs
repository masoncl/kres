//! Core kres types: tasks, shutdown, findings.
//!
//! Closes bugs.md items:
//! - C1: all TaskManager shared state lives inside an RwLock; no caller
//!   can iterate without taking it.
//! - C2: every Task owns a CancellationToken; /stop / /clear / goal-met
//!   / --turns propagate cancel before dropping references.
//! - C3: abandoning a Task waits for its handle, never strands it.
//! - H1: the merge critical section is split — the extract lock holds
//!   only during disk write, not across API calls.
//! - H2/H3: findings_write_count is incremented under the same mutex
//!   that allocates N and writes the file — one atomic unit.
//! - H6: findings-N.json is written via tmp-file + fsync + rename.
//! - L1: no parallel "completed_ids" vector — done tasks are queried
//!   directly off the ordered task list.

pub mod consent;
pub mod cost;
pub mod findings;
pub mod io;
pub mod lens;
pub mod log;
pub mod mode;
pub mod shrink;
pub mod shutdown;
pub mod task;
pub mod todo;

pub use consent::ConsentStore;
pub use cost::{UsageEntry, UsageKey, UsageTracker};
pub use findings::{Finding, FindingsFile, FindingsStore, Severity};
pub use lens::LensSpec;
pub use log::{LoggedUsage, TurnLogger};
pub use mode::{CodeFile, TaskMode};
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
