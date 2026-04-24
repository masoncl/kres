//! Ratatui-based interactive loop — stage 1 of the TUI migration.
//!
//! This module is a drop-in replacement for [`session::read_stdin`]
//! when the session is started with `--tui`. It owns the terminal
//! (raw mode + alternate screen) for the lifetime of the loop, runs
//! a crossterm event poll, and feeds submitted lines into the same
//! `mpsc::UnboundedSender<String>` the rustyline path uses — so the
//! rest of `Session::run` doesn't care which input driver produced
//! the line.
//!
//! What stage 1 ships:
//! - Scrollback pane (top) fed by [`kres_core::io::install_printer`].
//!   Every `async_println` / `async_eprintln` call site in the crates
//!   becomes a line in the TUI buffer; no console paint races.
//! - Status row (one line above the input) driven by the same
//!   [`render_status_line`](crate::session::render_status_line) the
//!   DECSTBM path already uses.
//! - Single-line input bar with insert / backspace / delete /
//!   Home/End / Left-Right / Enter-submit / Ctrl-C-cancel / Ctrl-D-EOF.
//!
//! What stage 1 deliberately does NOT ship (follow-up stages):
//! - History, Ctrl-R incremental search (rustyline still owns these
//!   in the default path).
//! - Ctrl-G `/edit` handoff to $EDITOR. In TUI mode the TUI owns the
//!   terminal; a follow-up will suspend ratatui before spawning vim
//!   and resume after. Until then `/edit` still submits as a command
//!   and the editor output will fight the frame.
//! - Multi-line prompt editing (Shift-Enter etc.).
//! - Mouse scrollback, search, panes, findings sidebar.
//!
//! Teardown: [`run_tui`] always restores the terminal (leave raw
//! mode, leave alt screen, show cursor) before returning, even on
//! panic, via [`TuiGuard`]. If kres is killed uncleanly the user may
//! need `reset` — same caveat as the existing DECSTBM path.
//!
//! `--stdio` takes precedence: when both `--stdio` and `--tui` are
//! set, `--stdio` wins (the plain line-buffered path stays in
//! charge) so output redirection keeps working unchanged.
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Terminal,
};
use tokio::sync::mpsc;

/// Bounded ring of lines rendered in the scrollback pane. A growing
/// `Vec<String>` is fine because `async_println` volume is low
/// (agent-emitted status, not per-token streaming), but an upper
/// bound stops a pathological agent from leaking unbounded memory
/// over a long session.
pub const SCROLLBACK_CAP: usize = 10_000;

/// Shared state between the crossterm event loop (which owns input)
/// and background writers (which push lines via the installed
/// printer). The mutex is held only for the time it takes to
/// push/pop a line, so contention with the event loop's per-frame
/// snapshot is negligible.
///
/// Each line gets a monotonically increasing **logical id**. The
/// `Vec<String>` only ever stores the most recent `SCROLLBACK_CAP`
/// lines, but the logical id survives eviction: the view anchor in
/// `run_tui` is a logical id, so a pinned scrollback position
/// doesn't drift when the ring evicts oldest entries. Without the
/// id mapping, the anchor would be an absolute `Vec` index that
/// points at different content after every drain — visually, the
/// pinned view would jump forward by the drain count.
#[derive(Clone, Default)]
pub struct Scrollback {
    inner: Arc<Mutex<ScrollbackInner>>,
}

#[derive(Default)]
struct ScrollbackInner {
    lines: Vec<String>,
    /// Logical id of `lines[0]`. Grows by the drain count on each
    /// push that exceeds `SCROLLBACK_CAP`. `first_id == 0` while the
    /// ring has never evicted.
    first_id: usize,
}

impl Scrollback {
    pub fn new() -> Self {
        Self::default()
    }
    /// Append a line, trimming the oldest entries if we're over cap.
    /// Called from every async_println site via the installed
    /// printer closure. A line may contain embedded newlines; we
    /// split so the renderer sees one entry per visual line.
    pub fn push(&self, s: &str) {
        let mut g = self.inner.lock().unwrap();
        for chunk in s.split('\n') {
            g.lines.push(chunk.to_string());
        }
        let len = g.lines.len();
        if len > SCROLLBACK_CAP {
            let drop = len - SCROLLBACK_CAP;
            g.lines.drain(0..drop);
            g.first_id += drop;
        }
    }
    /// Snapshot the last `max_rows` lines for a draw tick. Cheap —
    /// clones at most `max_rows` strings (on a 50-row terminal that's
    /// ~50 allocations per 100ms tick).
    pub fn tail(&self, max_rows: usize) -> Vec<String> {
        let g = self.inner.lock().unwrap();
        let start = g.lines.len().saturating_sub(max_rows);
        g.lines[start..].to_vec()
    }
    /// Snapshot a window ending `offset` lines above the newest
    /// entry. `offset = 0` is equivalent to [`tail`]; `offset = N`
    /// walks back N lines and returns the `max_rows` window ending
    /// at `len - offset`. Returns an empty Vec when offset walks past
    /// the oldest entry.
    pub fn window(&self, max_rows: usize, offset: usize) -> Vec<String> {
        let g = self.inner.lock().unwrap();
        let end = g.lines.len().saturating_sub(offset);
        let start = end.saturating_sub(max_rows);
        g.lines[start..end].to_vec()
    }
    /// Snapshot `max_rows` starting at logical line id
    /// `anchor_id`. If `anchor_id` has already been evicted (i.e.
    /// it's older than `first_id`), snap to the oldest retained
    /// line instead of returning empty — matches `less`'s behaviour
    /// of following the top when the tail of a rotating log moves
    /// past your position.
    pub fn window_from(&self, anchor_id: usize, max_rows: usize) -> Vec<String> {
        let g = self.inner.lock().unwrap();
        let vec_start = anchor_id.saturating_sub(g.first_id).min(g.lines.len());
        let vec_end = (vec_start + max_rows).min(g.lines.len());
        g.lines[vec_start..vec_end].to_vec()
    }
    /// Number of currently-retained lines. Doesn't count evicted
    /// entries — use `total_logical_lines` when you need the
    /// ever-pushed count (for clamping an anchor id, say).
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().lines.len()
    }
    /// Ever-pushed line count (= `first_id` + retained). Used by
    /// the view anchor logic to clamp PgUp against the newest line.
    pub fn total_logical_lines(&self) -> usize {
        let g = self.inner.lock().unwrap();
        g.first_id + g.lines.len()
    }
    /// Logical id of the oldest retained line. A view anchor less
    /// than this has been evicted — `window_from` snaps forward,
    /// but the `run_tui` clamp can also observe it and nudge the
    /// anchor forward itself so the `[PIN @N]` marker stays honest.
    pub fn first_id(&self) -> usize {
        self.inner.lock().unwrap().first_id
    }
    /// Convenience for clippy; not used by the TUI.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Install a `kres_core::io` printer that routes every
/// `async_println` through the TUI scrollback. Returns Ok even if
/// the printer slot is already filled — the first caller wins and we
/// don't want to abort TUI startup over a printer already installed
/// by an earlier rustyline attempt in the same process.
pub fn install_tui_printer(scrollback: Scrollback) {
    // Replace unconditionally: the session may have installed a
    // stdout-bootstrap printer earlier to serve `print_banner` and
    // other pre-TUI messages; once alt screen is about to take over
    // those stdout writes would blow up the frame, so the TUI
    // scrollback takes ownership here.
    let sb_print = scrollback.clone();
    kres_core::io::replace_printer(Box::new(move |s| {
        sb_print.push(&s);
    }));
    // Second sink: markdown blocks (slow-agent analysis body). We
    // bracket the block with sentinel lines the render path
    // recognises and hands to `render_markdown_block`. Non-TUI
    // contexts don't install this sink, so `async_println_markdown`
    // folds into `async_println` and `--stdio` stays byte-identical.
    let sb_md = scrollback.clone();
    kres_core::io::replace_markdown_sink(Box::new(move |body| {
        sb_md.push(MD_BLOCK_START);
        sb_md.push(body);
        sb_md.push(MD_BLOCK_END);
    }));
}

/// Sentinels bracketing a markdown region in the scrollback. The
/// render loop strips them and hands the enclosed body to
/// `render_markdown_block`. Plain stdout never sees these — they
/// only enter the scrollback via the TUI-only sink in
/// `install_tui_printer`.
pub const MD_BLOCK_START: &str = "\x01kres-md-block-start\x01";
pub const MD_BLOCK_END: &str = "\x01kres-md-block-end\x01";

/// Convert a markdown body into styled ratatui `Line`s. Small
/// in-house renderer — no deps — recognising the three shapes the
/// slow-agent actually uses: fenced code blocks (```…```, fence
/// markers in dim cyan, enclosed lines in cyan), 4-space-indented
/// code blocks (cyan), and inline `code` spans inside prose (cyan).
/// Everything else renders as a plain `Span`. The slow-agent body
/// is the only current caller, so scope is deliberately narrow —
/// no headings, lists, emphasis, or links.
pub fn render_markdown_block(body: &str) -> Vec<Line<'static>> {
    let code_style = Style::default().fg(Color::Cyan);
    let fence_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::DIM);
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut in_fence = false;
    for line in body.split('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            // Fence delimiter (open or close). Dim-cyan the marker
            // itself and flip fence state.
            out.push(Line::from(Span::styled(line.to_string(), fence_style)));
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            out.push(Line::from(Span::styled(line.to_string(), code_style)));
            continue;
        }
        // 4-space-indented code block (outside a fence).
        if line.starts_with("    ") && !line.trim().is_empty() {
            out.push(Line::from(Span::styled(line.to_string(), code_style)));
            continue;
        }
        // Prose line: scan for inline `backtick` spans.
        out.push(Line::from(split_inline_code(line, code_style)));
    }
    out
}

/// Turn a prose line into a vector of spans, styling anything
/// between matching backticks as code. Unmatched backticks fall
/// through as plain text.
fn split_inline_code(line: &str, code_style: Style) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cursor = 0usize;
    while cursor < line.len() {
        let Some(open_rel) = line[cursor..].find('`') else {
            spans.push(Span::raw(line[cursor..].to_string()));
            break;
        };
        let open = cursor + open_rel;
        // Plain text before the opening backtick.
        if open > cursor {
            spans.push(Span::raw(line[cursor..open].to_string()));
        }
        let Some(close_rel) = line[open + 1..].find('`') else {
            // Unmatched: emit the rest as plain text, backtick and
            // all, so the operator sees literally what the agent
            // produced rather than silently losing characters.
            spans.push(Span::raw(line[open..].to_string()));
            break;
        };
        let close = open + 1 + close_rel;
        spans.push(Span::styled(
            line[open + 1..close].to_string(),
            code_style,
        ));
        cursor = close + 1;
    }
    spans
}

/// Simple stdout writer used by the default / --stdio paths as a
/// bootstrap printer. Installed before `print_banner` so every
/// `async_println` / migrated `println!` call reaches a real sink
/// from the first line. The rustyline path replaces this with its
/// `ExternalPrinter` once the editor finishes booting; --stdio
/// keeps this printer for the whole session so redirected output
/// (`kres --stdio … > out.txt`) captures everything.
pub fn install_stdout_printer() {
    let _ = kres_core::io::install_printer(Box::new(|s| {
        use std::io::Write as _;
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{s}");
        let _ = out.flush();
    }));
}

/// Input-buffer state plus a history ring. Stage 2 adds Up/Down
/// recall and persistence to `~/.kres/history` so the TUI matches
/// rustyline's main QoL win. Stage 8 adds Ctrl-R reverse-i-search.
#[derive(Default)]
struct Input {
    buf: String,
    /// Cursor position in char (not byte) units so arrow-key moves
    /// don't split UTF-8. Converted to a byte offset only at
    /// insert/delete time.
    cursor: usize,
    /// Submitted lines, oldest first. Capped at [`HISTORY_CAP`] so a
    /// long-running session doesn't grow unbounded.
    history: Vec<String>,
    /// `None` = editing a fresh line; `Some(i)` = cursoring through
    /// history, with `i` as the index into `history`. When the
    /// operator types after a history move we drop back to `None`
    /// (their edits live on a draft that isn't saved until submit).
    hist_idx: Option<usize>,
    /// Stashed draft while browsing history. Restored when the
    /// operator walks past the newest entry (Down one past the end)
    /// or hits Escape. Kept separate from `buf` so the line they
    /// were typing doesn't get clobbered by an accidental Up press.
    draft: String,
    /// `Some` while Ctrl-R is active. Contains the in-progress
    /// query and the index of the matching history entry (if any).
    /// Typing appends to the query; Ctrl-R steps to the next older
    /// match; Enter accepts; Esc / Ctrl-C cancels.
    search: Option<SearchState>,
    /// Most recently killed text (Ctrl-W / Ctrl-U / Ctrl-K).
    /// Ctrl-Y pastes it at the cursor. Not history-persisted —
    /// matches rustyline, which keeps kill state per-session.
    kill_buffer: String,
}

/// Ctrl-R reverse-i-search state. `query` is the substring the
/// operator has typed; `match_idx` is the history-ring position of
/// the most-recent entry containing it, or None when nothing
/// matches. Recomputed on every query change; stepped (older)
/// on every additional Ctrl-R press.
#[derive(Default)]
struct SearchState {
    query: String,
    match_idx: Option<usize>,
}

/// Cap the in-memory history ring. Matches rustyline's default
/// max_history_size. Anything older is dropped on push so long
/// sessions stay bounded.
const HISTORY_CAP: usize = 1_000;

impl Input {
    fn byte_pos(&self) -> usize {
        self.buf
            .char_indices()
            .nth(self.cursor)
            .map(|(i, _)| i)
            .unwrap_or(self.buf.len())
    }
    fn char_len(&self) -> usize {
        self.buf.chars().count()
    }
    /// Called whenever the operator performs an edit (insert, delete,
    /// backspace): abandon history-browse mode so subsequent edits
    /// live on the current line instead of on the historical entry.
    fn leave_history(&mut self) {
        self.hist_idx = None;
    }
    fn insert(&mut self, c: char) {
        self.leave_history();
        let p = self.byte_pos();
        self.buf.insert(p, c);
        self.cursor += 1;
    }
    /// Insert a literal newline. Called from the Shift/Alt-Enter
    /// path so the operator can compose multi-line prompts inline
    /// without going through /edit. Storage is just `'\n'` in
    /// `buf`; the renderer splits on newline at draw time.
    fn newline(&mut self) {
        self.insert('\n');
    }

    /// Insert a whole string at the cursor. Used by bracketed
    /// paste — terminals send the pasted payload as one event so
    /// embedded newlines don't fire the Enter handler partway
    /// through. Normalises `\r\n` and bare `\r` to `\n` so pastes
    /// from clipboards that carry DOS line endings land with a
    /// sane shape in `buf`.
    fn insert_str(&mut self, s: &str) {
        self.leave_history();
        if s.is_empty() {
            return;
        }
        let normalised = s.replace("\r\n", "\n").replace('\r', "\n");
        let n = normalised.chars().count();
        let p = self.byte_pos();
        self.buf.insert_str(p, &normalised);
        self.cursor += n;
    }

    /// Convert a char-index into a byte-index for `self.buf`. Past
    /// the end returns `buf.len()` — matches `byte_pos` semantics.
    fn char_to_byte(&self, char_idx: usize) -> usize {
        self.buf
            .char_indices()
            .nth(char_idx)
            .map(|(b, _)| b)
            .unwrap_or(self.buf.len())
    }

    /// Kill the word behind the cursor (Ctrl-W). Word = run of
    /// non-whitespace after any trailing whitespace. Killed text is
    /// stored in `kill_buffer` so a subsequent Ctrl-Y yanks it.
    fn kill_prev_word(&mut self) {
        self.leave_history();
        if self.cursor == 0 {
            return;
        }
        let chars: Vec<char> = self.buf.chars().collect();
        let mut i = self.cursor;
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        let killed: String = chars[i..self.cursor].iter().collect();
        let start_byte = self.char_to_byte(i);
        let end_byte = self.byte_pos();
        self.buf.drain(start_byte..end_byte);
        self.cursor = i;
        self.kill_buffer = killed;
    }

    /// Kill from the cursor back to the start of the current
    /// logical line (Ctrl-U). In a single-line buffer that's the
    /// start of the buffer; when there are embedded newlines the
    /// kill stops at the previous `\n` so lines above aren't
    /// touched — matches rustyline / bash convention.
    fn kill_to_line_start(&mut self) {
        self.leave_history();
        if self.cursor == 0 {
            return;
        }
        let chars: Vec<char> = self.buf.chars().collect();
        let mut start = self.cursor;
        while start > 0 && chars[start - 1] != '\n' {
            start -= 1;
        }
        let killed: String = chars[start..self.cursor].iter().collect();
        let start_byte = self.char_to_byte(start);
        let end_byte = self.byte_pos();
        self.buf.drain(start_byte..end_byte);
        self.cursor = start;
        self.kill_buffer = killed;
    }

    /// Kill from the cursor to the end of the current logical line
    /// (Ctrl-K). Stops at the next `\n`; does not remove the
    /// newline itself so a Ctrl-K on a blank inner line leaves the
    /// empty line intact.
    fn kill_to_line_end(&mut self) {
        self.leave_history();
        let chars: Vec<char> = self.buf.chars().collect();
        if self.cursor >= chars.len() {
            return;
        }
        let mut end = self.cursor;
        while end < chars.len() && chars[end] != '\n' {
            end += 1;
        }
        if end == self.cursor {
            return;
        }
        let killed: String = chars[self.cursor..end].iter().collect();
        let start_byte = self.byte_pos();
        let end_byte = self.char_to_byte(end);
        self.buf.drain(start_byte..end_byte);
        self.kill_buffer = killed;
    }

    /// Yank the kill buffer at the cursor (Ctrl-Y). No-op when the
    /// kill buffer is empty so a blind Ctrl-Y on session start
    /// doesn't insert a stale paste.
    fn yank(&mut self) {
        self.leave_history();
        if self.kill_buffer.is_empty() {
            return;
        }
        let yanked = self.kill_buffer.clone();
        let n = yanked.chars().count();
        let p = self.byte_pos();
        self.buf.insert_str(p, &yanked);
        self.cursor += n;
    }

    /// Transpose the two characters around the cursor (Ctrl-T).
    /// At end-of-buffer, swap the last two chars without advancing —
    /// matches readline's "fix-the-typo-you-just-made" convention.
    fn transpose_chars(&mut self) {
        self.leave_history();
        let n = self.char_len();
        if n < 2 || self.cursor == 0 {
            return;
        }
        let (left, right) = if self.cursor >= n {
            (self.cursor - 2, self.cursor - 1)
        } else {
            (self.cursor - 1, self.cursor)
        };
        let mut chars: Vec<char> = self.buf.chars().collect();
        chars.swap(left, right);
        self.buf = chars.into_iter().collect();
        if self.cursor < n {
            self.cursor += 1;
        }
    }
    fn backspace(&mut self) {
        self.leave_history();
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        let p = self.byte_pos();
        self.buf.remove(p);
    }
    fn delete(&mut self) {
        self.leave_history();
        if self.cursor >= self.char_len() {
            return;
        }
        let p = self.byte_pos();
        self.buf.remove(p);
    }
    fn take(&mut self) -> String {
        self.cursor = 0;
        self.hist_idx = None;
        self.draft.clear();
        std::mem::take(&mut self.buf)
    }
    /// Record a submitted line. No-op for empty lines (parity with
    /// rustyline's add_history_entry when the trimmed form is
    /// empty); also dedupes against the most-recent entry so
    /// repeated Enter on the same prompt doesn't clutter the ring.
    fn record(&mut self, line: &str) {
        if line.trim().is_empty() {
            return;
        }
        if self.history.last().is_some_and(|prev| prev == line) {
            return;
        }
        self.history.push(line.to_string());
        if self.history.len() > HISTORY_CAP {
            let drop = self.history.len() - HISTORY_CAP;
            self.history.drain(0..drop);
        }
    }
    /// Ctrl-R: either enter search mode (first press) or step to
    /// the next-older match (subsequent presses). Does nothing when
    /// history is empty.
    fn search_start_or_step(&mut self) {
        if self.history.is_empty() {
            return;
        }
        if self.search.is_none() {
            self.search = Some(SearchState::default());
            self.recompute_search_match();
            return;
        }
        // Step: find the next older entry that also matches the
        // current query.
        let (query, cur_match) = {
            let Some(ref s) = self.search else { return };
            (s.query.clone(), s.match_idx)
        };
        let next_idx =
            cur_match.and_then(|cur| (0..cur).rev().find(|&i| self.history[i].contains(&query)));
        if let Some(ref mut s) = self.search {
            if next_idx.is_some() {
                s.match_idx = next_idx;
            }
            // Leave the old match visible when there's nothing
            // older — matches bash/rustyline's behaviour of
            // "stuck at oldest match" rather than silently clearing.
        }
    }
    /// Append `c` to the current search query and re-search for the
    /// newest matching history entry.
    fn search_push(&mut self, c: char) {
        if let Some(ref mut s) = self.search {
            s.query.push(c);
        }
        self.recompute_search_match();
    }
    fn search_pop(&mut self) {
        if let Some(ref mut s) = self.search {
            s.query.pop();
        }
        self.recompute_search_match();
    }
    /// Recompute `match_idx` from scratch, scanning newest-to-oldest
    /// for the first history entry containing the query.
    fn recompute_search_match(&mut self) {
        let query = match self.search {
            Some(ref s) => s.query.clone(),
            None => return,
        };
        let idx = self.history.iter().rposition(|h| h.contains(&query));
        if let Some(ref mut s) = self.search {
            s.match_idx = idx;
        }
    }
    /// Enter while in search mode — copy the matched entry into the
    /// input buffer so a subsequent Enter submits it, and exit
    /// search mode. When no match is current the query is dropped
    /// silently and the buffer is left as-is.
    fn search_accept(&mut self) {
        if let Some(s) = self.search.take() {
            if let Some(idx) = s.match_idx {
                self.buf = self.history[idx].clone();
                self.cursor = self.char_len();
            }
        }
    }
    /// Esc / Ctrl-C while searching — abandon the query and return
    /// to whatever the operator had in the buffer before.
    fn search_cancel(&mut self) {
        self.search = None;
    }

    /// Up: move the cursor up one line when the buffer has
    /// embedded newlines, falling through to `history_prev` only
    /// when the cursor is on the first source line (nothing to
    /// move up to). Column is preserved and clamped to the target
    /// line's length, matching vim / most editors.
    fn move_up(&mut self) {
        let chars: Vec<char> = self.buf.chars().collect();
        // Start of current source line.
        let mut line_start = self.cursor;
        while line_start > 0 && chars[line_start - 1] != '\n' {
            line_start -= 1;
        }
        if line_start == 0 {
            self.history_prev();
            return;
        }
        let col = self.cursor - line_start;
        let prev_line_end = line_start - 1; // the '\n'
        let mut prev_line_start = prev_line_end;
        while prev_line_start > 0 && chars[prev_line_start - 1] != '\n' {
            prev_line_start -= 1;
        }
        let prev_line_len = prev_line_end - prev_line_start;
        self.cursor = prev_line_start + col.min(prev_line_len);
    }

    /// Down: move the cursor down one line when there's a source
    /// line below, else fall through to `history_next`.
    fn move_down(&mut self) {
        let chars: Vec<char> = self.buf.chars().collect();
        let mut line_end = self.cursor;
        while line_end < chars.len() && chars[line_end] != '\n' {
            line_end += 1;
        }
        if line_end >= chars.len() {
            self.history_next();
            return;
        }
        let mut line_start = self.cursor;
        while line_start > 0 && chars[line_start - 1] != '\n' {
            line_start -= 1;
        }
        let col = self.cursor - line_start;
        let next_line_start = line_end + 1;
        let mut next_line_end = next_line_start;
        while next_line_end < chars.len() && chars[next_line_end] != '\n' {
            next_line_end += 1;
        }
        let next_line_len = next_line_end - next_line_start;
        self.cursor = next_line_start + col.min(next_line_len);
    }

    /// Up-arrow: step one entry backwards in history. Stashes the
    /// draft on the first press so a later Down can restore it.
    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let new_idx = match self.hist_idx {
            None => {
                // Leaving draft mode — stash whatever we were typing.
                self.draft = self.buf.clone();
                self.history.len() - 1
            }
            Some(0) => 0, // already at oldest, clamp
            Some(i) => i - 1,
        };
        self.hist_idx = Some(new_idx);
        self.buf = self.history[new_idx].clone();
        self.cursor = self.char_len();
    }
    /// Down-arrow: step one forward. Past the newest entry restores
    /// the stashed draft and drops out of browse mode.
    fn history_next(&mut self) {
        let Some(i) = self.hist_idx else { return };
        if i + 1 >= self.history.len() {
            // Walked off the end; restore draft.
            self.hist_idx = None;
            self.buf = std::mem::take(&mut self.draft);
            self.cursor = self.char_len();
            return;
        }
        self.hist_idx = Some(i + 1);
        self.buf = self.history[i + 1].clone();
        self.cursor = self.char_len();
    }
}

/// Load the persisted history file (one entry per line). Missing or
/// unreadable files are not an error — a first-run session starts
/// with an empty ring.
pub fn load_history(path: &std::path::Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect()
}

/// Persist the history ring to `path`, creating the parent dir if
/// missing. Failures are swallowed — losing history on shutdown is
/// not important enough to abort teardown and leave the terminal in
/// raw mode.
pub fn save_history(path: &std::path::Path, history: &[String]) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let body: String = history
        .iter()
        .flat_map(|line| [line.as_str(), "\n"])
        .collect();
    let _ = std::fs::write(path, body);
}

/// RAII guard: leaves raw mode and the alternate screen on drop.
/// Constructed *before* the event loop so a panic unwinding through
/// the loop restores the user's terminal instead of leaving it in
/// raw mode.
struct TuiGuard;

impl TuiGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut out = io::stdout();
        // BracketedPaste makes a terminal deliver pastes as a
        // single `Event::Paste(String)` rather than a barrage of
        // KeyPress events — crucial because a multi-line paste
        // would otherwise submit at the first Enter. Capture is
        // best-effort; terminals without support leave it off and
        // the old one-key-per-char behaviour keeps working.
        execute!(
            out,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )?;
        Ok(Self)
    }
}

impl Drop for TuiGuard {
    fn drop(&mut self) {
        let mut out = io::stdout();
        let _ = execute!(
            out,
            DisableBracketedPaste,
            LeaveAlternateScreen,
            DisableMouseCapture
        );
        let _ = disable_raw_mode();
        let _ = out.flush();
    }
}

/// Suspend the TUI (leave alt screen + raw mode), run `$EDITOR` on
/// a tempfile, then restore the TUI so the event loop can resume
/// drawing. Mirrors [`Session::cmd_edit`] at session.rs:2515 but
/// lives on the TUI thread because that's the thread that owns the
/// terminal. Doing the handoff from the main loop would need a
/// cross-thread choreography (tell the TUI to suspend, wait for it,
/// run the editor, signal resume); handling it here keeps the
/// terminal-ownership boundary a single thread's concern.
///
/// Returns the non-empty trimmed tempfile contents on success, or
/// None when the editor errored, the file was empty, or the user
/// aborted. Any resume error is logged via `async_eprintln!` and
/// the caller is expected to bail — at that point the terminal is
/// wedged and the session has to tear down.
fn run_editor_handoff() -> Option<String> {
    // Leave the TUI before the child runs so the editor paints on a
    // normal-screen terminal with raw mode off. The subsequent
    // re-entry rebuilds the TUI frame from scratch via
    // `terminal.clear()` in the caller.
    let mut out = io::stdout();
    let _ = execute!(out, LeaveAlternateScreen, DisableMouseCapture);
    let _ = disable_raw_mode();
    let _ = out.flush();

    let editor_cmd = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    let tmp = std::env::temp_dir().join(format!(
        "kres-edit-{}-{}.md",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    let _ = std::fs::write(&tmp, "");
    let status = std::process::Command::new(&editor_cmd).arg(&tmp).status();
    // Trust the tempfile contents regardless of editor exit code —
    // `:wq!` forced-quit and Esc-save-without-quit shouldn't drop
    // the typed prompt. Only a spawn failure skips the read.
    let content = match status {
        Ok(_) => std::fs::read_to_string(&tmp).ok(),
        Err(e) => {
            kres_core::async_eprintln!("/edit: editor spawn failed: {e}");
            None
        }
    };
    let _ = std::fs::remove_file(&tmp);

    // Re-enter the TUI. Raw mode + alt screen are restored so the
    // next draw() fires into a clean frame.
    if enable_raw_mode().is_err() {
        kres_core::async_eprintln!("/edit: re-entering raw mode failed");
        return None;
    }
    if execute!(out, EnterAlternateScreen, EnableMouseCapture).is_err() {
        kres_core::async_eprintln!("/edit: re-entering alt screen failed");
        return None;
    }
    let trimmed = content
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    if trimmed.is_none() {
        kres_core::async_eprintln!("/edit: empty, nothing submitted");
    }
    trimmed
}

/// Status-line callback: given the current terminal width, produce
/// the single-line summary to paint above the input bar. Kept
/// generic so the TUI doesn't import the TaskManager type directly —
/// `Session::run` closes over a shared snapshot cell (populated by a
/// tokio background task) and passes a capturing closure that reads
/// it synchronously. No `block_on` from the crossterm thread.
pub type StatusFn = Box<dyn Fn(usize) -> String + Send>;

/// Width of the "> " prompt prefix painted on the very first visual
/// row of the input widget. Subsequent visual rows (either from
/// `\n` or from wrap) start at column 0.
const PROMPT_PREFIX: usize = 2;

/// Compute `(total_visual_rows, cursor_row, cursor_col)` for the
/// input prompt given `width` (the paragraph text-area width, i.e.
/// box width minus borders). The cursor column already includes
/// the `"> "` prefix when the cursor sits on the first visual row.
///
/// Pulled out as a pure function so (a) the draw closure can size
/// the input box and park the cursor from one walk of the buffer,
/// and (b) a unit test can exercise every branch without spinning
/// up a ratatui backend.
fn compute_input_layout(buf: &str, cursor: usize, width: u16) -> (u16, u16, u16) {
    // Degenerate terminals (1-wide) park the cursor at origin and
    // claim one row; the frame will look bad but nothing panics.
    let width = (width as usize).max(PROMPT_PREFIX + 1);
    let mut row: u16 = 0;
    let mut chars_on_row: usize = 0;
    let mut on_first_src_line = true;
    let mut cur_row: u16 = 0;
    let mut cur_col: u16 = PROMPT_PREFIX as u16;
    let mut passed_cursor = false;
    let effective_cap = |r: u16, first: bool| -> usize {
        if r == 0 && first {
            width.saturating_sub(PROMPT_PREFIX)
        } else {
            width
        }
    };
    for (i, c) in buf.chars().enumerate() {
        if i == cursor {
            let prefix = if row == 0 && on_first_src_line {
                PROMPT_PREFIX
            } else {
                0
            };
            cur_row = row;
            cur_col = (prefix + chars_on_row) as u16;
            passed_cursor = true;
        }
        if c == '\n' {
            row += 1;
            chars_on_row = 0;
            on_first_src_line = false;
            continue;
        }
        let cap = effective_cap(row, on_first_src_line);
        if chars_on_row + 1 > cap {
            row += 1;
            chars_on_row = 1;
        } else {
            chars_on_row += 1;
        }
    }
    if !passed_cursor {
        let prefix = if row == 0 && on_first_src_line {
            PROMPT_PREFIX
        } else {
            0
        };
        cur_row = row;
        cur_col = (prefix + chars_on_row) as u16;
    }
    (row + 1, cur_row, cur_col)
}

/// Entry point used by `Session::run`. Owns the terminal and the
/// event pump; blocks until Ctrl-D / channel-close. Runs on a
/// `spawn_blocking` thread exactly like the rustyline path.
///
/// `tx` — where submitted lines go (commands + prompts, same format
/// as the plain path emits).
///
/// `ack_rx` — currently ignored. Kept in the signature so call sites
/// match the rustyline path and a future stage can coordinate
/// $EDITOR handoff.
pub fn run_tui(
    tx: mpsc::UnboundedSender<String>,
    _ack_rx: mpsc::UnboundedReceiver<()>,
    scrollback: Scrollback,
    status_fn: StatusFn,
    history_path: Option<std::path::PathBuf>,
) -> io::Result<()> {
    let _guard = TuiGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut input = Input::default();
    if let Some(ref p) = history_path {
        input.history = load_history(p);
    }
    // Scrollback view state. `None` = follow mode (always show the
    // tail). `Some(i)` = pinned at absolute line index `i` — new
    // lines pushed at the tail don't shift what the operator is
    // looking at, because the window is `[anchor..anchor+rows]`
    // regardless of total length. PgDn / End restore follow by
    // clearing back to None.
    let mut view_anchor: Option<usize> = None;
    // Track what we showed last draw so PgUp / PgDn can step by one
    // page (= visible rows - 1 for continuity).  Initialise to a
    // sensible default in case the first key press fires before the
    // first draw tick.
    let mut last_scrollback_rows: usize = 20;

    // Cap on the input box height — past this, the input scrolls
    // rather than pushing the scrollback pane off-screen. Rustyline
    // does the same when multi-line composition outgrows the
    // terminal.
    const INPUT_MAX_ROWS: u16 = 10;
    loop {
        terminal.draw(|f| {
            let size = f.area();
            // Grow the input box to fit both explicit `\n` newlines
            // AND soft-wrapped long lines. Two-pass layout: compute
            // with an assumed width, then use the real one. Since
            // the box width is (terminal width - 0 borders padding),
            // a single pass with `size.width - 2` (the inner width
            // of the bordered box) is enough — the visual row count
            // doesn't depend on box height, only on width, and the
            // layout split only depends on box height.
            let inner_width = size.width.saturating_sub(2);
            let (buf_rows, cur_row, cur_col) =
                compute_input_layout(&input.buf, input.cursor, inner_width);
            // +2 for borders. Capped so a monster paste doesn't
            // push the scrollback off-screen; if the buffer needs
            // more rows than INPUT_MAX_ROWS the overflow scrolls
            // within the Paragraph and the cursor clamps to the
            // bottom edge.
            let input_rows = (buf_rows + 2).min(INPUT_MAX_ROWS);
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(1),             // scrollback
                    Constraint::Length(1),          // status line
                    Constraint::Length(input_rows), // input (borders + text)
                ])
                .split(size);

            let scrollback_rows = chunks[0].height as usize;
            last_scrollback_rows = scrollback_rows;
            let total = scrollback.total_logical_lines();
            let first = scrollback.first_id();
            // Clamp anchor into the currently-valid logical range.
            // Three nudges:
            //   1. anchor < first_id → line has been evicted. Snap
            //      to `first_id` so the view sits on the oldest
            //      retained line instead of silently following.
            //   2. anchor + rows >= total → the tail would already
            //      be on-screen from this anchor; drop the pin so
            //      we follow again (and the [PIN] marker clears).
            //   3. anchor >= total → buffer was cleared under us;
            //      follow.
            if let Some(a) = view_anchor {
                // Cases 2 and 3 both drop the pin — the tail would
                // already be on-screen (so no point pinning) or the
                // buffer was cleared under us (so there's nothing
                // left to pin to). Case 1 snaps up to the oldest
                // retained line instead of silently following.
                if a >= total || a + scrollback_rows >= total {
                    view_anchor = None;
                } else if a < first {
                    view_anchor = Some(first);
                }
            }
            let window = match view_anchor {
                Some(anchor_id) => scrollback.window_from(anchor_id, scrollback_rows),
                None => scrollback.window(scrollback_rows, 0),
            };
            // Pad the top with blank lines so content is anchored
            // to the bottom of the pane (against the status line),
            // matching terminal scrollback convention. Without this,
            // a short buffer renders at the top of the pane and
            // leaves an empty gap between the latest line and the
            // status row — the operator submits /followup, gets a
            // one-line response, and sees it isolated high in the
            // pane with nothing near the prompt, reading as if the
            // command produced nothing.
            let pad = scrollback_rows.saturating_sub(window.len());
            // Expand markdown regions: when we hit MD_BLOCK_START,
            // gather lines until the matching MD_BLOCK_END, feed the
            // joined body through `render_markdown_block`, and
            // splice its styled Lines in place of the raw lines.
            // Marker lines are dropped. A window slice that cuts
            // through a bracketed region (MD_START before the
            // window, or MD_END after) falls back to plain rendering
            // for the visible half — acceptable for a first pass.
            let mut body: Vec<Line> = (0..pad).map(|_| Line::from("")).collect();
            let mut i = 0;
            while i < window.len() {
                if window[i] == MD_BLOCK_START {
                    let start = i + 1;
                    let end = window[start..]
                        .iter()
                        .position(|l| l == MD_BLOCK_END)
                        .map(|p| start + p)
                        .unwrap_or(window.len());
                    let block = window[start..end].join("\n");
                    body.extend(render_markdown_block(&block));
                    // Skip past MD_END when we found one; otherwise
                    // we already consumed to the window tail.
                    i = if end < window.len() { end + 1 } else { end };
                } else if window[i] == MD_BLOCK_END {
                    // Dangling close marker (window started
                    // mid-block); swallow it.
                    i += 1;
                } else {
                    body.push(Line::from(window[i].clone()));
                    i += 1;
                }
            }
            let output = Paragraph::new(body).wrap(Wrap { trim: false });
            f.render_widget(output, chunks[0]);

            // When scrolled away from the bottom, prefix the
            // status with a short marker so the operator isn't
            // confused by new lines appearing off-screen. Shows
            // the anchored top line's index so they can tell how
            // far back they've walked.
            let status_text = status_fn(chunks[1].width as usize);
            let status_text = if let Some(a) = view_anchor {
                format!("[PIN @{a}] {status_text}")
            } else {
                status_text
            };
            let status = Paragraph::new(Line::from(Span::styled(
                status_text,
                Style::default().add_modifier(Modifier::REVERSED),
            )));
            f.render_widget(status, chunks[1]);

            // Search mode: render `(reverse-i-search)'query': match`
            // in place of the normal prompt. The regular buffer is
            // untouched — a search-accept moves the match into the
            // buffer and exits search mode before the next draw.
            if let Some(ref s) = input.search {
                let match_line = s
                    .match_idx
                    .map(|i| input.history[i].clone())
                    .unwrap_or_else(|| "(no match)".to_string());
                let search_prompt = format!("(reverse-i-search)`{}': {match_line}", s.query);
                let widget = Paragraph::new(Line::from(Span::raw(search_prompt)))
                    .block(Block::default().borders(Borders::ALL));
                f.render_widget(widget, chunks[2]);
                return;
            }
            // Render each \n-separated segment as a visual line,
            // with "> " bolted onto the first one so the prompt
            // marker stays visible regardless of how tall the box
            // has grown. A completely empty buffer still renders
            // one visible row so the cursor has somewhere to sit.
            let segments: Vec<&str> = if input.buf.is_empty() {
                vec![""]
            } else {
                input.buf.split('\n').collect()
            };
            let prompt_lines: Vec<Line> = segments
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    if i == 0 {
                        Line::from(vec![
                            Span::styled("> ", Style::default().add_modifier(Modifier::BOLD)),
                            Span::raw(s.to_string()),
                        ])
                    } else {
                        Line::from(Span::raw(s.to_string()))
                    }
                })
                .collect();
            let prompt = Paragraph::new(prompt_lines)
                .wrap(Wrap { trim: false })
                .block(Block::default().borders(Borders::ALL));
            f.render_widget(prompt, chunks[2]);

            // Cursor row/col came from compute_input_layout above;
            // translate to absolute terminal coords. `+1` on each
            // axis to step inside the border. Clamp to the
            // box's bottom-right so an oversized paste doesn't
            // park the cursor in the status area.
            let cx = chunks[2].x + 1 + cur_col;
            let cy = chunks[2].y + 1 + cur_row;
            let right_edge = chunks[2].x + chunks[2].width - 1;
            let bottom_edge = chunks[2].y + chunks[2].height - 1;
            f.set_cursor_position((cx.min(right_edge), cy.min(bottom_edge)));
        })?;

        // Event poll: 100ms matches the REPL's ambient poll cadence.
        // A shorter poll would waste CPU; longer lags status-bar and
        // scrollback updates the operator can see.
        //
        // Keep the pump responsive to async_println traffic — drive
        // a redraw on every tick even without a key event so
        // background writers show up promptly.
        if !event::poll(Duration::from_millis(100))? {
            // Silent tick — status_fn is driven by a shared cell
            // refreshed on a tokio task, so the next draw() already
            // has fresh data without blocking here.
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                // Search mode swallows most input: typable chars
                // extend the query, Backspace trims it, Ctrl-R
                // steps to the next match, Enter accepts, Esc /
                // Ctrl-C cancels. Any other key exits search mode
                // without accepting so the main handler sees it.
                if input.search.is_some() {
                    match (key.code, key.modifiers) {
                        (KeyCode::Char('r'), KeyModifiers::CONTROL) => {
                            input.search_start_or_step();
                        }
                        (KeyCode::Char('c'), KeyModifiers::CONTROL) | (KeyCode::Esc, _) => {
                            input.search_cancel();
                        }
                        (KeyCode::Enter, _) => {
                            input.search_accept();
                        }
                        (KeyCode::Backspace, _) => {
                            input.search_pop();
                        }
                        (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => {
                            input.search_push(c);
                        }
                        _ => {
                            // Arrow keys, PgUp/Dn, etc. cancel
                            // search rather than mysteriously
                            // doing nothing.
                            input.search_cancel();
                        }
                    }
                    continue;
                }
                match (key.code, key.modifiers) {
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                        // Ctrl-C: when the input has content, clear
                        // it (matches the rustyline behaviour of
                        // discarding the line). When the buffer is
                        // empty, the operator means "cancel the
                        // running tasks" — but crossterm's raw mode
                        // clears ISIG, so the kernel won't generate
                        // SIGINT for us. Raise it manually so the
                        // tokio `signal::ctrl_c` handler in
                        // Session::run wakes up and runs its drain +
                        // cancel + persist sequence.
                        if input.buf.is_empty() {
                            // SAFETY: kill(pid, SIGINT) is a thread-
                            // safe POSIX call. Target our own pid
                            // (not the process group): the group
                            // includes child processes like
                            // semcode-mcp, which would catch the
                            // SIGINT and exit, racing the in-process
                            // tokio handler and tearing down the
                            // session before it could drain. Errors
                            // here would mean the kernel can't
                            // deliver — unrecoverable for the cancel
                            // path, so swallow.
                            unsafe {
                                libc::kill(libc::getpid(), libc::SIGINT);
                            }
                        } else {
                            input.buf.clear();
                            input.cursor = 0;
                        }
                    }
                    (KeyCode::Char('d'), KeyModifiers::CONTROL) if input.buf.is_empty() => {
                        // Ctrl-D on empty buffer = EOF. Persist the
                        // history ring before returning so the next
                        // session sees this run's entries. Dropping
                        // `tx` would do the same exit-wise, but we
                        // return explicitly so a later stage can
                        // distinguish operator-driven shutdown from
                        // an internal error.
                        if let Some(ref p) = history_path {
                            save_history(p, &input.history);
                        }
                        return Ok(());
                    }
                    (KeyCode::Enter, m)
                        if m.contains(KeyModifiers::SHIFT) || m.contains(KeyModifiers::ALT) =>
                    {
                        // Shift-Enter / Alt-Enter insert a literal
                        // newline so the operator can compose a
                        // multi-line prompt without going through
                        // $EDITOR. Matches the rustyline bindings
                        // at session.rs:3797-3802.
                        input.newline();
                    }
                    (KeyCode::Enter, _)
                        if input.buf.ends_with('\\') && !input.buf.ends_with("\\\\") =>
                    {
                        // Backslash-Enter: shell-style line
                        // continuation. Eat the trailing `\` and
                        // insert a newline. `\\` (escaped
                        // backslash) is respected so an operator
                        // who genuinely wants a prompt ending in
                        // `\` can type it by doubling.
                        //
                        // The check is against the whole buffer,
                        // not the cursor position, so hitting
                        // Enter after a mid-buffer edit still
                        // submits as normal unless the very last
                        // char is a lone `\`.
                        input.buf.pop();
                        input.cursor = input.cursor.saturating_sub(1);
                        input.newline();
                    }
                    (KeyCode::Enter, _) => {
                        // Record BEFORE take() so `input.history`
                        // has the line appended, then ship it. The
                        // submit channel and history are decoupled:
                        // tx.send failure still leaves the entry in
                        // memory so a later save_history call picks
                        // it up.
                        let line = input.buf.clone();
                        input.record(&line);
                        let _ = input.take();
                        // Echo the submitted line into the scrollback
                        // *before* the tx.send. The main REPL loop
                        // might be blocked on a long-running command
                        // (slow agent turn, /summary streaming, etc.),
                        // so without the echo the operator has no
                        // confirmation that their slash command was
                        // accepted — the input just disappears and
                        // stays silent until the main loop drains
                        // the current work. Empty submissions
                        // (blank Enter) skip the echo to avoid a
                        // blank "> " line in scrollback.
                        if !line.is_empty() {
                            scrollback.push(&format!("> {line}"));
                        }
                        // Any submission drops the scroll pin and
                        // snaps to follow mode. Otherwise a
                        // scrolled-back operator who fires /todo
                        // or /followup sees nothing: the command
                        // output lands at the tail, the pinned
                        // view stays on older content, and the
                        // response is invisible until they also
                        // press End/Ctrl-End. Operators who meant
                        // to keep reading the old page wouldn't
                        // be submitting in the first place.
                        view_anchor = None;
                        // `/edit` on its own submits a prompt via
                        // $EDITOR. In rustyline mode this is the
                        // cmd_edit path; here we do the same work
                        // on the TUI thread because the main loop
                        // doesn't own the terminal.
                        if line.trim() == "/edit" {
                            if let Some(text) = run_editor_handoff() {
                                input.record(&text);
                                if tx.send(text).is_err() {
                                    if let Some(ref p) = history_path {
                                        save_history(p, &input.history);
                                    }
                                    return Ok(());
                                }
                            }
                            terminal.clear()?;
                            continue;
                        }
                        if tx.send(line).is_err() {
                            if let Some(ref p) = history_path {
                                save_history(p, &input.history);
                            }
                            return Ok(());
                        }
                    }
                    (KeyCode::Char('r'), KeyModifiers::CONTROL) => {
                        // Ctrl-R: start reverse history search.
                        // The search-mode branch at the top of the
                        // match handles subsequent Ctrl-R presses
                        // by stepping to the next-older match.
                        input.search_start_or_step();
                    }
                    // ── Emacs-style cursor / history aliases ──
                    (KeyCode::Char('a'), KeyModifiers::CONTROL) => input.cursor = 0,
                    (KeyCode::Char('e'), KeyModifiers::CONTROL) => {
                        input.cursor = input.char_len();
                    }
                    (KeyCode::Char('b'), KeyModifiers::CONTROL) => {
                        input.cursor = input.cursor.saturating_sub(1);
                    }
                    (KeyCode::Char('f'), KeyModifiers::CONTROL) => {
                        let n = input.char_len();
                        if input.cursor < n {
                            input.cursor += 1;
                        }
                    }
                    (KeyCode::Char('p'), KeyModifiers::CONTROL) => input.move_up(),
                    (KeyCode::Char('n'), KeyModifiers::CONTROL) => input.move_down(),
                    // ── Kill / yank / transpose ──
                    (KeyCode::Char('w'), KeyModifiers::CONTROL) => input.kill_prev_word(),
                    (KeyCode::Char('u'), KeyModifiers::CONTROL) => input.kill_to_line_start(),
                    (KeyCode::Char('k'), KeyModifiers::CONTROL) => input.kill_to_line_end(),
                    (KeyCode::Char('y'), KeyModifiers::CONTROL) => input.yank(),
                    (KeyCode::Char('t'), KeyModifiers::CONTROL) => input.transpose_chars(),
                    // ── Clear scrollback view + redraw ──
                    (KeyCode::Char('l'), KeyModifiers::CONTROL) => {
                        // Ctrl-L: drop any pin and force a full
                        // repaint. Doesn't drop the buffer —
                        // operators who want to purge scrollback
                        // can /clear.
                        view_anchor = None;
                        terminal.clear()?;
                    }
                    (KeyCode::Char('g'), KeyModifiers::CONTROL) => {
                        // Ctrl-G: open $EDITOR on a scratch
                        // file — matches the rustyline binding at
                        // session.rs:3792. Stashes whatever is in
                        // the input buffer so the draft isn't lost
                        // if the operator aborts the editor.
                        if let Some(text) = run_editor_handoff() {
                            input.record(&text);
                            if tx.send(text).is_err() {
                                if let Some(ref p) = history_path {
                                    save_history(p, &input.history);
                                }
                                return Ok(());
                            }
                            // Any submission drops the scroll pin,
                            // same as the Enter path — the editor
                            // handoff is just a more elaborate
                            // Enter and the operator expects to
                            // see the response.
                            view_anchor = None;
                        }
                        terminal.clear()?;
                    }
                    (KeyCode::Up, _) => input.move_up(),
                    (KeyCode::Down, _) => input.move_down(),
                    (KeyCode::PageUp, _) => {
                        // Step by (rows - 1) so one line of
                        // context carries over between pages,
                        // matching the convention `less` uses.
                        // First PgUp converts follow → pin at
                        // (total - rows - step); further PgUps
                        // decrement the anchor. Anchor values are
                        // logical line ids; clamp at `first_id` so
                        // we can't page past the oldest retained
                        // line into evicted-id territory.
                        let step = last_scrollback_rows.saturating_sub(1).max(1);
                        let total = scrollback.total_logical_lines();
                        let first = scrollback.first_id();
                        let raw = match view_anchor {
                            Some(a) => a.saturating_sub(step),
                            None => total
                                .saturating_sub(last_scrollback_rows)
                                .saturating_sub(step),
                        };
                        view_anchor = Some(raw.max(first));
                    }
                    (KeyCode::PageDown, _) => {
                        let step = last_scrollback_rows.saturating_sub(1).max(1);
                        view_anchor = match view_anchor {
                            Some(a) => {
                                let next = a.saturating_add(step);
                                let total = scrollback.total_logical_lines();
                                if next + last_scrollback_rows >= total {
                                    None
                                } else {
                                    Some(next)
                                }
                            }
                            None => None,
                        };
                    }
                    // Ctrl+Home / Ctrl+End jump to the top and
                    // bottom of scrollback. Bare Home/End are
                    // reserved for input-line cursor moves (below)
                    // so the common cursor-to-start / cursor-to-end
                    // gestures keep working. Top = first retained
                    // logical id so the PIN marker shows a real,
                    // visible line.
                    (KeyCode::Home, m) if m.contains(KeyModifiers::CONTROL) => {
                        view_anchor = Some(scrollback.first_id());
                    }
                    (KeyCode::End, m) if m.contains(KeyModifiers::CONTROL) => {
                        view_anchor = None;
                    }
                    (KeyCode::Backspace, _) => input.backspace(),
                    (KeyCode::Delete, _) => input.delete(),
                    (KeyCode::Left, _) => {
                        input.cursor = input.cursor.saturating_sub(1);
                    }
                    (KeyCode::Right, _) => {
                        let n = input.char_len();
                        if input.cursor < n {
                            input.cursor += 1;
                        }
                    }
                    (KeyCode::Home, _) => input.cursor = 0,
                    (KeyCode::End, _) => input.cursor = input.char_len(),
                    (KeyCode::Esc, _) => {
                        // Matches the convention "Esc drops out of
                        // history browse" — restore the stashed
                        // draft. A no-op when not browsing.
                        if input.hist_idx.is_some() {
                            input.hist_idx = None;
                            input.buf = std::mem::take(&mut input.draft);
                            input.cursor = input.char_len();
                        }
                    }
                    (KeyCode::Char(c), m) if !m.contains(KeyModifiers::CONTROL) => {
                        input.insert(c);
                    }
                    _ => {}
                }
            }
            Event::Paste(text) => {
                // BracketedPaste: dump the whole chunk into the
                // buffer as one operation. If search mode is active,
                // the query extends by the pasted content too —
                // that's handy for pasting a partial command to
                // find it in history.
                if let Some(ref mut s) = input.search {
                    s.query.push_str(&text);
                    input.recompute_search_match();
                } else {
                    input.insert_str(&text);
                }
            }
            Event::Mouse(me) => {
                // Wheel scroll walks the scrollback view the same
                // way PgUp/PgDn do, just three lines per tick so a
                // single flick of the wheel moves a comfortable
                // amount without overshooting short output. Mouse
                // position is ignored — the wheel always targets
                // the scrollback pane, which is the only pane
                // that's scrollable. Shift+select still works in
                // most terminals to bypass mouse capture for text
                // copy.
                match me.kind {
                    MouseEventKind::ScrollUp => {
                        let total = scrollback.total_logical_lines();
                        let first = scrollback.first_id();
                        let raw = match view_anchor {
                            Some(a) => a.saturating_sub(3),
                            None => total.saturating_sub(last_scrollback_rows).saturating_sub(3),
                        };
                        view_anchor = Some(raw.max(first));
                    }
                    MouseEventKind::ScrollDown => {
                        view_anchor = match view_anchor {
                            Some(a) => {
                                let next = a.saturating_add(3);
                                let total = scrollback.total_logical_lines();
                                if next + last_scrollback_rows >= total {
                                    None
                                } else {
                                    Some(next)
                                }
                            }
                            None => None,
                        };
                    }
                    _ => {}
                }
            }
            Event::Resize(_, _) => {
                // Next draw() picks up the new size automatically.
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_plain_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn line_styled_text(line: &Line<'_>, style: Style) -> String {
        line.spans
            .iter()
            .filter(|s| s.style == style)
            .map(|s| s.content.as_ref())
            .collect()
    }

    #[test]
    fn render_markdown_fenced_block_styles_every_line_including_markers() {
        let body = "before\n```\ncode a\ncode b\n```\nafter";
        let lines = render_markdown_block(body);
        let code_style = Style::default().fg(Color::Cyan);
        let fence_style = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::DIM);
        let texts: Vec<String> = lines.iter().map(line_plain_text).collect();
        assert_eq!(texts, vec!["before", "```", "code a", "code b", "```", "after"]);
        // Fence markers are dim-cyan, enclosed lines are plain cyan,
        // prose lines carry no Cyan styling.
        assert_eq!(lines[0].spans[0].style, Style::default(), "prose unstyled");
        assert_eq!(lines[1].spans[0].style, fence_style, "open fence dim");
        assert_eq!(lines[2].spans[0].style, code_style, "code a");
        assert_eq!(lines[3].spans[0].style, code_style, "code b");
        assert_eq!(lines[4].spans[0].style, fence_style, "close fence dim");
        assert_eq!(lines[5].spans[0].style, Style::default(), "prose after");
    }

    #[test]
    fn render_markdown_inline_backticks_emit_mixed_spans() {
        let body = "see `foo_bar()` and `x` for details";
        let lines = render_markdown_block(body);
        assert_eq!(lines.len(), 1);
        let code_style = Style::default().fg(Color::Cyan);
        let code_text: String = line_styled_text(&lines[0], code_style);
        assert_eq!(code_text, "foo_bar()x", "both backticked spans cyan");
        let full: String = line_plain_text(&lines[0]);
        assert_eq!(full, "see foo_bar() and x for details");
    }

    #[test]
    fn render_markdown_unmatched_backtick_passes_through() {
        // Don't silently drop characters when the agent emits a
        // stray backtick — surface what it produced.
        let body = "backtick: ` alone";
        let lines = render_markdown_block(body);
        let full: String = line_plain_text(&lines[0]);
        assert_eq!(full, "backtick: ` alone");
    }

    #[test]
    fn render_markdown_indented_code_outside_fence() {
        let body = "prose\n    fn foo() {}\nprose";
        let lines = render_markdown_block(body);
        let code_style = Style::default().fg(Color::Cyan);
        assert_eq!(lines[1].spans[0].style, code_style);
        assert_eq!(lines[0].spans[0].style, Style::default());
        assert_eq!(lines[2].spans[0].style, Style::default());
    }

    #[test]
    fn scrollback_respects_cap() {
        let sb = Scrollback::new();
        for i in 0..(SCROLLBACK_CAP + 50) {
            sb.push(&format!("line {i}"));
        }
        let tail = sb.tail(10);
        assert_eq!(tail.len(), 10);
        // The last line pushed must survive; the first must not.
        assert_eq!(
            tail.last().unwrap(),
            &format!("line {}", SCROLLBACK_CAP + 49)
        );
        assert_eq!(sb.len(), SCROLLBACK_CAP);
        // Oldest retained logical id is 50 (we pushed 10050 total).
        assert_eq!(sb.first_id(), 50);
        // Pull the oldest retained via a wide-enough tail call.
        let full = sb.tail(SCROLLBACK_CAP);
        assert_eq!(full.first().unwrap(), "line 50");
    }

    #[test]
    fn scrollback_window_offset_walks_back() {
        let sb = Scrollback::new();
        for i in 0..50 {
            sb.push(&format!("l{i}"));
        }
        // Offset 0 → last 10 lines (l40..l49).
        let tail = sb.window(10, 0);
        assert_eq!(tail.first().unwrap(), "l40");
        assert_eq!(tail.last().unwrap(), "l49");
        // Offset 5 → window ends at l44, so last 10 ending there
        // = l35..l44.
        let mid = sb.window(10, 5);
        assert_eq!(mid.first().unwrap(), "l35");
        assert_eq!(mid.last().unwrap(), "l44");
        // Offset past the oldest entry returns an empty window
        // rather than panicking.
        let over = sb.window(10, 500);
        assert!(over.is_empty());
    }

    #[test]
    fn scrollback_window_from_anchors_to_logical_id() {
        let sb = Scrollback::new();
        for i in 0..20 {
            sb.push(&format!("l{i}"));
        }
        // Pin at logical id 5, show 6 rows → l5..l10.
        let pinned = sb.window_from(5, 6);
        assert_eq!(pinned.len(), 6);
        assert_eq!(pinned.first().unwrap(), "l5");
        assert_eq!(pinned.last().unwrap(), "l10");
        // Push more lines — the pinned view shouldn't shift.
        for i in 20..30 {
            sb.push(&format!("l{i}"));
        }
        let still_pinned = sb.window_from(5, 6);
        assert_eq!(still_pinned, pinned);
        // Anchor past newest → empty (not a panic).
        let past = sb.window_from(9999, 5);
        assert!(past.is_empty());
    }

    #[test]
    fn scrollback_pin_survives_eviction() {
        // Fill past the cap so the ring drains and `first_id`
        // advances; the pinned window's contents must stay the
        // same before and after push-past-cap traffic continues.
        let sb = Scrollback::new();
        for i in 0..SCROLLBACK_CAP {
            sb.push(&format!("l{i}"));
        }
        // Pin mid-buffer (anchor 500), page size 6 → l500..l505.
        let before = sb.window_from(500, 6);
        assert_eq!(before.first().unwrap(), "l500");
        assert_eq!(before.last().unwrap(), "l505");
        assert_eq!(sb.first_id(), 0);
        // Now push another 300 lines — triggers eviction of the
        // oldest 300. first_id advances; anchor id 500 still maps
        // to the same content (just at a different vec index).
        for i in SCROLLBACK_CAP..(SCROLLBACK_CAP + 300) {
            sb.push(&format!("l{i}"));
        }
        assert_eq!(sb.first_id(), 300);
        let after = sb.window_from(500, 6);
        assert_eq!(after, before);
        // An anchor that's been evicted snaps forward to the
        // oldest retained line rather than returning garbage from
        // an off-by-first_id vec index.
        let evicted = sb.window_from(100, 6);
        assert_eq!(evicted.first().unwrap(), "l300");
    }

    #[test]
    fn total_logical_lines_includes_evicted() {
        let sb = Scrollback::new();
        for i in 0..(SCROLLBACK_CAP + 50) {
            sb.push(&format!("l{i}"));
        }
        assert_eq!(sb.first_id(), 50);
        assert_eq!(sb.len(), SCROLLBACK_CAP);
        assert_eq!(sb.total_logical_lines(), SCROLLBACK_CAP + 50);
    }

    #[test]
    fn scrollback_splits_embedded_newlines() {
        let sb = Scrollback::new();
        sb.push("alpha\nbeta\ngamma");
        assert_eq!(sb.tail(10), vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn input_insert_and_backspace() {
        let mut i = Input::default();
        i.insert('h');
        i.insert('i');
        assert_eq!(i.buf, "hi");
        assert_eq!(i.cursor, 2);
        i.backspace();
        assert_eq!(i.buf, "h");
        assert_eq!(i.cursor, 1);
        i.backspace();
        i.backspace(); // underflow-safe
        assert_eq!(i.buf, "");
        assert_eq!(i.cursor, 0);
    }

    #[test]
    fn input_preserves_utf8_on_cursor_moves() {
        let mut i = Input::default();
        for c in "héllo".chars() {
            i.insert(c);
        }
        assert_eq!(i.char_len(), 5);
        i.cursor = 0;
        assert_eq!(i.byte_pos(), 0);
        i.cursor = 2;
        assert_eq!(&i.buf[i.byte_pos()..], "llo");
        i.delete();
        assert_eq!(i.buf, "hélo");
    }

    #[test]
    fn input_take_resets_state() {
        let mut i = Input::default();
        i.insert('x');
        i.insert('y');
        let got = i.take();
        assert_eq!(got, "xy");
        assert_eq!(i.buf, "");
        assert_eq!(i.cursor, 0);
    }

    #[test]
    fn move_up_stays_in_buffer_when_multiline() {
        let mut i = Input::default();
        i.insert_str("one\ntwo\nthree");
        // Cursor at end of "three" (char 13).
        assert_eq!(i.cursor, 13);
        i.move_up();
        // Column 5 clamped to "two"'s length 3 → cursor at 7.
        assert_eq!(i.cursor, 7);
        i.move_up();
        // Column 3 on "one" (length 3) → cursor at 3.
        assert_eq!(i.cursor, 3);
    }

    #[test]
    fn move_up_on_first_line_falls_through_to_history() {
        let mut i = Input::default();
        i.record("earlier");
        i.insert_str("draft");
        assert_eq!(i.cursor, 5);
        i.move_up();
        // history_prev kicked in — buffer replaced with historical entry.
        assert_eq!(i.buf, "earlier");
        assert_eq!(i.hist_idx, Some(0));
    }

    #[test]
    fn move_down_advances_within_multiline() {
        let mut i = Input::default();
        i.insert_str("alpha\nbeta");
        i.cursor = 2; // inside "alpha", column 2
        i.move_down();
        // Column 2 on "beta" → cursor at char 8 (6 = '\n' index + 1, + 2).
        assert_eq!(i.cursor, 8);
    }

    #[test]
    fn move_down_on_last_line_falls_through_to_history() {
        let mut i = Input::default();
        i.record("only");
        i.record("other");
        i.insert_str("draft");
        // Walk history back, then move_down — should step to next entry.
        i.history_prev();
        i.history_prev();
        assert_eq!(i.buf, "only");
        // move_down on single-line buffer with history → history_next.
        i.move_down();
        assert_eq!(i.buf, "other");
    }

    #[test]
    fn insert_str_normalises_crlf_and_cr() {
        let mut i = Input::default();
        i.insert_str("alpha\r\nbeta\rgamma");
        assert_eq!(i.buf, "alpha\nbeta\ngamma");
        // Cursor ended at char-length of the normalised string.
        assert_eq!(i.cursor, i.char_len());
    }

    #[test]
    fn insert_str_appends_at_cursor() {
        let mut i = Input::default();
        i.insert_str("hello");
        i.cursor = 2; // cursor between 'e' and 'l'
        i.insert_str("XY");
        assert_eq!(i.buf, "heXYllo");
        assert_eq!(i.cursor, 4); // after the XY
    }

    #[test]
    fn layout_empty_buffer_is_one_row_cursor_at_prefix() {
        let (rows, r, c) = compute_input_layout("", 0, 80);
        assert_eq!(rows, 1);
        assert_eq!((r, c), (0, PROMPT_PREFIX as u16));
    }

    #[test]
    fn layout_single_newline_gives_two_rows() {
        // "a\nb" with cursor past end → row 1, col 1 (no prefix on row 1).
        let (rows, r, c) = compute_input_layout("a\nb", 3, 80);
        assert_eq!(rows, 2);
        assert_eq!((r, c), (1, 1));
    }

    #[test]
    fn layout_wraps_long_line_when_over_width() {
        // width = 10 cols, first row cap = width - prefix = 8.
        // 10 'a's → row 0 fills to 8, then 2 wrap onto row 1.
        let buf = "a".repeat(10);
        let (rows, r, c) = compute_input_layout(&buf, 10, 10);
        assert_eq!(rows, 2);
        // Cursor is past the end: row 1, col 2.
        assert_eq!((r, c), (1, 2));
    }

    #[test]
    fn layout_cursor_mid_wrapped_segment() {
        // width 10 (cap 8 on row 0), buf = 12 'b's.
        // Row 0: cols 2..10 (8 chars). Row 1: cols 0..4 (4 chars).
        // Cursor at char 5 → still on row 0, col = 2 + 5 = 7.
        let buf = "b".repeat(12);
        let (_, r, c) = compute_input_layout(&buf, 5, 10);
        assert_eq!((r, c), (0, 7));
    }

    #[test]
    fn kill_prev_word_removes_run_and_saves_to_kill_buffer() {
        let mut i = Input::default();
        for c in "hello  world".chars() {
            i.insert(c);
        }
        i.kill_prev_word();
        assert_eq!(i.buf, "hello  ");
        assert_eq!(i.kill_buffer, "world");
        assert_eq!(i.cursor, 7);
        // Second Ctrl-W eats the trailing whitespace + "hello".
        i.kill_prev_word();
        assert_eq!(i.buf, "");
        assert_eq!(i.kill_buffer, "hello  ");
    }

    #[test]
    fn kill_to_line_start_stops_at_prev_newline() {
        let mut i = Input::default();
        for c in "line1\nhello world".chars() {
            i.insert(c);
        }
        // Cursor is after "world" (at char_len).
        i.kill_to_line_start();
        assert_eq!(i.buf, "line1\n");
        assert_eq!(i.kill_buffer, "hello world");
    }

    #[test]
    fn kill_to_line_end_stops_at_next_newline() {
        let mut i = Input::default();
        for c in "hello world\nnext".chars() {
            i.insert(c);
        }
        // Move cursor to after "hello " (char idx 6).
        i.cursor = 6;
        i.kill_to_line_end();
        assert_eq!(i.buf, "hello \nnext");
        assert_eq!(i.kill_buffer, "world");
    }

    #[test]
    fn yank_inserts_kill_buffer() {
        let mut i = Input::default();
        for c in "foo bar".chars() {
            i.insert(c);
        }
        i.kill_prev_word();
        assert_eq!(i.buf, "foo ");
        i.yank();
        assert_eq!(i.buf, "foo bar");
        // Yank again at cursor — doubles the text.
        i.yank();
        assert_eq!(i.buf, "foo barbar");
    }

    #[test]
    fn transpose_swaps_chars_around_cursor() {
        let mut i = Input::default();
        for c in "ab".chars() {
            i.insert(c);
        }
        // Cursor at end (2) → readline's "fix last typo" → swap last two.
        i.transpose_chars();
        assert_eq!(i.buf, "ba");
        assert_eq!(i.cursor, 2);
        // Reset and try mid-buffer: "abcd" cursor at 2 (between b and c).
        let mut j = Input::default();
        for c in "abcd".chars() {
            j.insert(c);
        }
        j.cursor = 2;
        j.transpose_chars();
        assert_eq!(j.buf, "acbd");
        assert_eq!(j.cursor, 3);
        // Cursor at 0 → no-op.
        let mut k = Input::default();
        for c in "ab".chars() {
            k.insert(c);
        }
        k.cursor = 0;
        k.transpose_chars();
        assert_eq!(k.buf, "ab");
    }

    #[test]
    fn backslash_continuation_strips_slash_and_adds_newline() {
        // Simulate the run_tui handler for bare Enter with a buffer
        // ending in `\`: pop the slash, bump the cursor back, insert
        // a newline. The handler is inline in the key match; this
        // test exercises the state transitions the handler performs.
        let mut i = Input::default();
        i.insert_str("foo\\");
        assert!(i.buf.ends_with('\\') && !i.buf.ends_with("\\\\"));
        i.buf.pop();
        i.cursor = i.cursor.saturating_sub(1);
        i.newline();
        assert_eq!(i.buf, "foo\n");
        assert_eq!(i.cursor, 4);
    }

    #[test]
    fn double_backslash_does_not_continue() {
        // `\\` (two trailing slashes) means "literal backslash"
        // not "line continuation". The run_tui handler guards with
        // `!buf.ends_with("\\\\")` so plain Enter submits instead.
        let i: Input = {
            let mut i = Input::default();
            i.insert_str("foo\\\\");
            i
        };
        assert!(i.buf.ends_with("\\\\"));
    }

    #[test]
    fn input_newline_preserves_cursor_and_buf() {
        let mut i = Input::default();
        i.insert('a');
        i.insert('b');
        i.newline();
        i.insert('c');
        // Buffer now reads "ab\nc" with cursor at 4 (after 'c').
        assert_eq!(i.buf, "ab\nc");
        assert_eq!(i.cursor, 4);
        // Backspace past the newline: 'c' is removed, then '\n'.
        i.backspace();
        assert_eq!(i.buf, "ab\n");
        i.backspace();
        assert_eq!(i.buf, "ab");
    }

    #[test]
    fn history_record_skips_empty_and_dedupes() {
        let mut i = Input::default();
        i.record("foo");
        i.record("   "); // whitespace-only → skipped
        i.record("");
        i.record("foo"); // exact dup of prior → skipped
        i.record("bar");
        assert_eq!(i.history, vec!["foo".to_string(), "bar".to_string()]);
    }

    #[test]
    fn history_up_stashes_draft_and_down_restores() {
        let mut i = Input::default();
        i.record("alpha");
        i.record("beta");
        // Type a draft, then press Up — draft must be stashed.
        i.insert('d');
        i.insert('r');
        assert_eq!(i.buf, "dr");
        i.history_prev();
        assert_eq!(i.buf, "beta");
        assert_eq!(i.hist_idx, Some(1));
        i.history_prev();
        assert_eq!(i.buf, "alpha");
        // Clamp at the oldest entry.
        i.history_prev();
        assert_eq!(i.buf, "alpha");
        // Down steps forward.
        i.history_next();
        assert_eq!(i.buf, "beta");
        // Past the newest → restore draft and leave browse mode.
        i.history_next();
        assert_eq!(i.buf, "dr");
        assert_eq!(i.hist_idx, None);
    }

    #[test]
    fn history_edit_drops_out_of_browse() {
        let mut i = Input::default();
        i.record("alpha");
        i.history_prev();
        assert_eq!(i.hist_idx, Some(0));
        // A keystroke must abandon browse mode so later Up doesn't
        // walk past what the operator is currently editing.
        i.insert('!');
        assert_eq!(i.hist_idx, None);
        assert_eq!(i.buf, "alpha!");
    }

    #[test]
    fn history_file_roundtrips() {
        let dir = std::env::temp_dir().join(format!(
            "kres-tui-hist-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("history");
        let entries = vec!["one".to_string(), "two".to_string(), "three".to_string()];
        save_history(&path, &entries);
        let loaded = load_history(&path);
        assert_eq!(loaded, entries);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn search_finds_newest_match_and_steps_older() {
        let mut i = Input::default();
        i.record("git status");
        i.record("cargo build");
        i.record("cargo test");
        i.record("git log");
        // First Ctrl-R: newest match for empty query = newest entry.
        i.search_start_or_step();
        let s = i.search.as_ref().unwrap();
        assert_eq!(s.match_idx, Some(3));
        // Type "git" — newest match containing "git" is "git log".
        i.search_push('g');
        i.search_push('i');
        i.search_push('t');
        assert_eq!(i.search.as_ref().unwrap().match_idx, Some(3));
        // Step: next-older "git" match = "git status" at index 0.
        i.search_start_or_step();
        assert_eq!(i.search.as_ref().unwrap().match_idx, Some(0));
        // Accept — buffer becomes "git status", search mode exits.
        i.search_accept();
        assert_eq!(i.buf, "git status");
        assert!(i.search.is_none());
    }

    #[test]
    fn search_no_match_leaves_buf_unchanged() {
        let mut i = Input::default();
        i.record("alpha");
        i.record("beta");
        i.insert('x');
        assert_eq!(i.buf, "x");
        i.search_start_or_step();
        i.search_push('z'); // no entry contains 'z'
        assert!(i.search.as_ref().unwrap().match_idx.is_none());
        i.search_accept();
        // No match → buf preserved, just search-mode cleared.
        assert_eq!(i.buf, "x");
        assert!(i.search.is_none());
    }

    #[test]
    fn search_cancel_leaves_everything_alone() {
        let mut i = Input::default();
        i.record("alpha");
        i.insert('d');
        i.insert('r');
        i.search_start_or_step();
        i.search_push('a');
        assert_eq!(i.search.as_ref().unwrap().match_idx, Some(0));
        i.search_cancel();
        assert!(i.search.is_none());
        assert_eq!(i.buf, "dr");
    }

    #[test]
    fn history_cap_trims_oldest() {
        let mut i = Input::default();
        for n in 0..(HISTORY_CAP + 5) {
            i.record(&format!("entry-{n}"));
        }
        assert_eq!(i.history.len(), HISTORY_CAP);
        // Oldest entries were dropped; newest survive.
        assert_eq!(i.history.first().unwrap(), "entry-5");
        assert_eq!(
            i.history.last().unwrap(),
            &format!("entry-{}", HISTORY_CAP + 4)
        );
    }
}
