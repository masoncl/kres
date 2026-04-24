//! Async-friendly printer channel.
//!
//! Background agents (fast, slow, main, reaper, lens fan-out) emit
//! progress messages while the REPL's `>` prompt is active. A raw
//! `eprintln!` bulldozes the prompt: the in-progress line buffer
//! gets overwritten and rustyline has to repaint on the next
//! keystroke. The REPL installs a sink that funnels every such line
//! through rustyline's `ExternalPrinter`, which knows how to clear
//! the current buffer, print, and repaint the prompt.
//!
//! This module lives in `kres-core` so every crate downstream can
//! write progress messages through the same sink without needing a
//! dep on `kres-repl`. Callers use [`async_println`] from anywhere;
//! the REPL installs the handler via [`install_printer`]. If no
//! handler is installed (non-REPL contexts like `kres turn` or
//! setup-phase logging), messages fall through to `eprintln!`.

use std::sync::OnceLock;

/// The sink receives each line as a String. The REPL wraps an
/// `ExternalPrinter` behind this. Non-REPL callers never install a
/// handler and their messages go to stderr via the fallback.
pub type PrinterFn = Box<dyn Fn(String) + Send + Sync + 'static>;

static PRINTER: OnceLock<PrinterFn> = OnceLock::new();

/// Install the global printer sink. Idempotent — subsequent calls
/// after the first are ignored (returns `Err` with the rejected
/// handler). Typically invoked once from the REPL startup.
pub fn install_printer(f: PrinterFn) -> Result<(), PrinterFn> {
    PRINTER.set(f)
}

/// Has a printer been installed? Useful for call sites that want to
/// skip work when there's no REPL listening.
pub fn has_printer() -> bool {
    PRINTER.get().is_some()
}

/// Route a single line through the installed printer, falling back
/// to `eprintln!` when none is set. Do not include a trailing
/// newline — the sink appends one.
///
/// Output is rendered in dimmed ("dark white") style so the running
/// progress chatter (fast/slow/main rounds, plan steps, fetch
/// summaries, banner metadata) settles below the eye level of the
/// task analysis the operator actually wants to read. Lines printed
/// via plain `println!` (notably the slow-agent analysis body in
/// `report_reaped`) stay at default brightness.
pub fn async_println(line: impl Into<String>) {
    use owo_colors::OwoColorize;
    let s = format!("{}", line.into().dimmed());
    match PRINTER.get() {
        Some(f) => f(s),
        None => eprintln!("{s}"),
    }
}

/// Convenience: `format!`-style with no-alloc when the printer is
/// absent. The `format_args!` form isn't capturable without
/// allocation here because `Box<dyn Fn>` requires an owned String;
/// keep callers using `async_println(format!(...))` for clarity.
#[macro_export]
macro_rules! async_eprintln {
    ($($arg:tt)*) => {
        $crate::io::async_println(format!($($arg)*))
    };
}

// ---------------------------------------------------------------
// Active-streams registry. The REPL status line reads this to show
// every in-flight Anthropic stream with its current token counts.
// ---------------------------------------------------------------

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

static NEXT_STREAM_ID: AtomicU32 = AtomicU32::new(1);

/// A single in-flight streaming call, identified by an auto-
/// incrementing id plus a caller-supplied `label` (e.g. "fast round
/// 2", "slow lens memory", "main turn 3"). Token counters update
/// live as SSE events arrive. `stop_reason` is None while the call
/// is running; Some once the stream delivered `message_delta`.
#[derive(Debug)]
pub struct StreamInfo {
    pub id: u32,
    pub label: String,
    pub model: String,
    pub input_tokens: AtomicU64,
    pub cache_read_tokens: AtomicU64,
    pub cache_creation_tokens: AtomicU64,
    pub output_tokens: AtomicU64,
    pub started_at: std::time::Instant,
}

/// Read-only snapshot of [`StreamInfo`] used by the status
/// renderer. Atomics are loaded once; the snapshot is a consistent
/// view of a single moment.
#[derive(Debug, Clone)]
pub struct StreamSnapshot {
    pub id: u32,
    pub label: String,
    pub model: String,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub output_tokens: u64,
    pub elapsed_ms: u128,
}

static ACTIVE: OnceLock<Mutex<Vec<Arc<StreamInfo>>>> = OnceLock::new();

fn registry() -> &'static Mutex<Vec<Arc<StreamInfo>>> {
    ACTIVE.get_or_init(|| Mutex::new(Vec::new()))
}

/// Register a new in-flight streaming call. The returned guard
/// updates counters via [`StreamGuard::on_message_start`] /
/// [`StreamGuard::on_text_delta`] / etc.; dropping the guard
/// removes the entry from the registry.
pub fn register_stream(label: impl Into<String>, model: impl Into<String>) -> StreamGuard {
    let id = NEXT_STREAM_ID.fetch_add(1, Ordering::Relaxed);
    let info = Arc::new(StreamInfo {
        id,
        label: label.into(),
        model: model.into(),
        input_tokens: AtomicU64::new(0),
        cache_read_tokens: AtomicU64::new(0),
        cache_creation_tokens: AtomicU64::new(0),
        output_tokens: AtomicU64::new(0),
        started_at: std::time::Instant::now(),
    });
    registry().lock().unwrap().push(info.clone());
    StreamGuard { info }
}

/// Drop-on-completion handle for a registered stream. Callers
/// update live token counts via its methods; when the guard drops,
/// the entry is removed from the registry so the status bar stops
/// showing it.
pub struct StreamGuard {
    info: Arc<StreamInfo>,
}

impl StreamGuard {
    pub fn on_message_start(&self, input: u64, cache_creation: u64, cache_read: u64) {
        self.info.input_tokens.store(input, Ordering::Relaxed);
        self.info
            .cache_creation_tokens
            .store(cache_creation, Ordering::Relaxed);
        self.info
            .cache_read_tokens
            .store(cache_read, Ordering::Relaxed);
    }
    /// Accumulate an output-token delta from a streamed block.
    pub fn add_output_tokens(&self, n: u64) {
        self.info.output_tokens.fetch_add(n, Ordering::Relaxed);
    }
    /// Set the terminal output_tokens from `message_delta`.
    pub fn set_output_tokens(&self, total: u64) {
        self.info.output_tokens.store(total, Ordering::Relaxed);
    }
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        let id = self.info.id;
        let mut g = registry().lock().unwrap();
        g.retain(|s| s.id != id);
    }
}

/// Snapshot of every currently-registered stream. Returned in
/// registration order (oldest first). The status renderer calls
/// this on every repaint tick.
pub fn active_streams() -> Vec<StreamSnapshot> {
    let g = registry().lock().unwrap();
    g.iter()
        .map(|s| StreamSnapshot {
            id: s.id,
            label: s.label.clone(),
            model: s.model.clone(),
            input_tokens: s.input_tokens.load(Ordering::Relaxed),
            cache_read_tokens: s.cache_read_tokens.load(Ordering::Relaxed),
            cache_creation_tokens: s.cache_creation_tokens.load(Ordering::Relaxed),
            output_tokens: s.output_tokens.load(Ordering::Relaxed),
            elapsed_ms: s.started_at.elapsed().as_millis(),
        })
        .collect()
}
