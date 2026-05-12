//! Persistent status line above the rustyline prompt.
//!
//! Reserves the bottom two rows of the terminal:
//! - row H-1: status line, repainted by [`paint`]
//! - row H:   rustyline's `> ` prompt (untouched)
//!
//! The mechanism is DECSTBM (`ESC[{top};{bottom}r`) to set a
//! scroll region that excludes those two rows. Every `async_println`
//! routed through the REPL's ExternalPrinter scrolls within the
//! region; the bottom two stay put.
//!
//! Not a full curses TUI — no raw mode, no keystroke handling, no
//! resize detection. Rustyline continues to own line editing.
//!
//! The terminal reset must be emitted on exit; [`restore`] does that.
//! If the kres process dies without calling restore, the scroll
//! region persists in the user's shell — a `reset` command fixes it.

use std::io::Write;
use std::sync::{Mutex, OnceLock};

fn terminal_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Current terminal size in rows/cols, via ioctl TIOCGWINSZ on a
/// tty fd (stderr). Returns None when stderr isn't a tty or the
/// ioctl fails.
pub fn term_size() -> Option<(u16, u16)> {
    use std::os::unix::io::AsRawFd;
    let fd = std::io::stderr().as_raw_fd();
    // SAFETY: libc::ioctl with TIOCGWINSZ writes into a
    // zero-initialised winsize; no Rust state leaks in or out.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: TIOCGWINSZ is a read-only ioctl; it only writes into
    // the winsize passed via a raw pointer. The pointer is valid
    // for the duration of the call.
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws as *mut _) };
    if rc != 0 {
        return None;
    }
    if ws.ws_row < 3 || ws.ws_col == 0 {
        return None;
    }
    Some((ws.ws_row, ws.ws_col))
}

/// Reserve the bottom two rows. Sets a scroll region of rows
/// 1..H-2 (inclusive, 1-indexed), moves the cursor inside it, and
/// leaves rows H-1 and H blank for the status + prompt.
///
/// Returns the `(rows, cols)` picked so the caller knows the geometry
/// for subsequent [`paint`] calls. Silently no-ops (returns None)
/// when stderr isn't a tty or the size lookup fails.
pub fn install() -> Option<(u16, u16)> {
    let (rows, cols) = term_size()?;
    let _guard = terminal_lock().lock().unwrap();
    let mut out = std::io::stderr();
    // Paint a blank initial status row so the layout is visible
    // even before the first poll fires.
    let _ = write!(
        out,
        // ESC[7h  enable autowrap (belt-and-braces)
        // ESC[?25h show cursor
        // ESC[1;{bottom}r  set scroll region top=1, bottom=H-2
        // ESC[{status_row};1H  move to status row, col 1
        // ESC[2K  clear entire line
        // ESC[{scroll_bot};1H  park cursor in scroll region bottom
        "\x1b[1;{bottom}r\x1b[{status_row};1H\x1b[2K\x1b[{scroll_bot};1H",
        bottom = rows - 2,
        status_row = rows - 1,
        scroll_bot = rows - 2,
    );
    let _ = out.flush();
    Some((rows, cols))
}

/// Repaint the status row with the given text. Called off the hot
/// path — a background poller runs every few hundred ms.
/// Truncates to `cols-1` chars so the terminal doesn't wrap.
pub fn paint(rows: u16, cols: u16, text: &str) {
    let _guard = terminal_lock().lock().unwrap();
    let mut out = std::io::stderr();
    let status_row = rows - 1;
    // Truncate; account for a trailing space and ensure we don't
    // cross a UTF-8 boundary — chars().take() is safe.
    let max = cols.saturating_sub(1) as usize;
    let trimmed: String = text.chars().take(max).collect();
    // ESC[s  save cursor + attrs
    // ESC[{row};1H  absolute-move to status row
    // ESC[2K  clear line
    // <text>  (plain — no reverse video so the bar reads as ambient info)
    // ESC[0m  reset any stray attrs just in case
    // ESC[u  restore cursor
    let _ = write!(
        out,
        "\x1b[s\x1b[{row};1H\x1b[2K{trim}\x1b[0m\x1b[u",
        row = status_row,
        trim = trimmed,
    );
    let _ = out.flush();
}

/// Move the cursor back into the scroll region reserved for normal
/// output. Foreground slash commands run between two rustyline
/// prompts, so rustyline's ExternalPrinter writes directly at the
/// current cursor position instead of going through its raw-mode
/// prompt-aware path. After the operator presses Enter, that cursor
/// can still be on the prompt row, outside the DECSTBM scroll region;
/// park it at the scroll bottom before command output so newlines
/// visibly scroll the transcript.
pub fn park_scroll_region_bottom() {
    let Some((rows, _)) = term_size() else {
        return;
    };
    if rows < 3 {
        return;
    }
    let _guard = terminal_lock().lock().unwrap();
    let mut out = std::io::stderr();
    let _ = write!(out, "\x1b[{};1H", rows - 2);
    let _ = out.flush();
}

/// Print foreground command output synchronously above the reserved
/// status/prompt rows.
///
/// Do not route this through the async printer: foreground slash
/// commands return to the readline loop immediately after printing,
/// so an async ExternalPrinter send can race the next prompt repaint
/// and the status-row painter. This function writes the complete
/// block while holding the same terminal lock as [`paint`].
pub fn print_command_block(block: &str) -> bool {
    let Some((rows, _)) = term_size() else {
        return false;
    };
    if rows < 3 {
        return false;
    }
    let _guard = terminal_lock().lock().unwrap();
    let mut out = std::io::stderr();
    let _ = write!(out, "\x1b[{};1H", rows - 2);
    for line in block.lines() {
        let _ = write!(out, "\r\n\x1b[2K{line}");
    }
    if block.ends_with('\n') {
        let _ = write!(out, "\r\n\x1b[2K");
    }
    let _ = out.flush();
    true
}

/// Clear a specific terminal row (absolute row number, 1-indexed)
/// and reset the scroll region to full-screen. Used on resize to
/// wipe the previous status row at its old location before
/// install() sets up the new scroll region. Preserves scrollback
/// content unlike a full ESC[2J clear.
pub fn clear_row_and_reset_region(row: u16) {
    let _guard = terminal_lock().lock().unwrap();
    let mut out = std::io::stderr();
    // ESC[r        reset scroll region to full screen so our
    //              absolute-cursor-move can reach any row
    // ESC[{row};1H absolute-move to target row
    // ESC[2K       clear just that row
    let _ = write!(out, "\x1b[r\x1b[{row};1H\x1b[2K", row = row);
    let _ = out.flush();
}

/// Restore the terminal: reset the scroll region and clear the
/// status row. Call on REPL shutdown.
pub fn restore() {
    let _guard = terminal_lock().lock().unwrap();
    let mut out = std::io::stderr();
    // ESC[r  reset scroll region to full screen
    if let Some((rows, _)) = term_size() {
        let _ = write!(out, "\x1b[r\x1b[{row};1H\x1b[2K", row = rows - 1,);
    } else {
        let _ = write!(out, "\x1b[r");
    }
    let _ = out.flush();
}
