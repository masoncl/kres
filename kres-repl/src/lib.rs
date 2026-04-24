//! REPL for kres.
//!
//! Not trying to match rustyline feature-by-feature with
//! readline yet — the initial REPL is line-buffered stdin with a
//! shutdown signal and the handful of commands needed to exercise the
//! pipeline end-to-end:
//!
//! - `/help`
//! - `/tasks`
//! - `/stop` — cancel every running task (bugs.md#C2, #C3)
//! - `/quit` / `/exit`
//! - anything else → submitted as a prompt
//!
//! Bug-list invariants upheld:
//! - Ctrl-C and `/stop` both propagate via TaskManager::root_shutdown.
//!   Running tasks get cancelled cooperatively; detached threads are
//!   never left running (bugs.md#C2, #C3).
//! - The command table is explicit so an unknown `/slash` prints
//!   "unknown command" instead of being interpreted as a prompt
//!   (bugs.md#M8 UX, adapted).

pub mod commands;
pub mod export;
pub mod report;
pub mod session;
pub mod settings;
pub mod status;
pub mod summary;
pub mod tui;

pub use commands::{parse_command, Command};
pub use export::{run_export, run_export_index, ExportInputs};
pub use report::{append_task_section, render_findings_markdown, write_findings_to_file};
pub use session::{build_orchestrator, ReplConfig, Session};
pub use settings::{pick_model, ModelRole, Settings};
pub use summary::{default_output_path, run_summary, SummaryInputs};
