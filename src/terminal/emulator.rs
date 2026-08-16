//! `fleetcom` reconstructs each task's terminal state from raw PTY output.
//! `alacritty_terminal` provides the parser, visible grid, and scrollback.

use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

use alacritty_terminal::{
    Term,
    event::{Event, EventListener},
    grid::{Dimensions, Row, Scroll},
    index::{Column, Line},
    term::{
        Config, TermMode,
        cell::{Cell, Flags},
    },
    vte::ansi::{self as vt, Handler, Processor},
};
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};

use crate::{format::prefix_bytes, protocol::ClipboardKind};

/// Mouse event classes requested by the child through DECSET 1000/1002/1003.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MouseProtocolMode {
    None,
    PressRelease,
    ButtonMotion,
    AnyMotion,
}

/// Coordinate encoding for mouse reports (DECSET 1005/1006).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MouseProtocolEncoding {
    Default,
    Utf8,
    Sgr,
}

/// Maximum decoded size of one buffered OSC 52 clipboard payload.
pub(crate) const CLIPBOARD_STORE_MAX_BYTES: usize = 1024 * 1024;

// A maximum-size store expands to this base64 bound when re-encoded for
// forwarding. Reserve 64 KiB for the command envelope and keep the result
// within one frame.
const _: () = assert!(
    CLIPBOARD_STORE_MAX_BYTES.div_ceil(3) * 4 + 64 * 1024 <= crate::frame::MAX_FRAME as usize,
    "CLIPBOARD_STORE_MAX_BYTES must base64-encode to under frame::MAX_FRAME"
);

/// OSC 52 clipboard stores captured since the last drain.
#[derive(Debug, Default)]
pub struct ClipboardStores {
    /// Buffered stores, at most one per supported selector, ordered by arrival.
    pub stores: Vec<(ClipboardKind, String)>,
    /// Byte length of the most recent oversized store.
    pub oversized_len: Option<usize>,
}

/// Buffers backend-generated PTY responses while the parser advances. Other
/// events are discarded; [`ObservedTerm`] captures titles and clipboard stores.
pub struct ProbeSink {
    responses: Arc<Mutex<Vec<String>>>,
}

impl EventListener for ProbeSink {
    fn send_event(&self, event: Event) {
        if let Event::PtyWrite(text) = event {
            let mut buf = self
                .responses
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            buf.push(text);
        }
    }
}

/// Terminal dimensions supplied to `Term::new` and `Term::resize`.
struct GridSize {
    lines: usize,
    columns: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.lines
    }

    fn screen_lines(&self) -> usize {
        self.lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// Default-deny allowlist for backend-generated probe responses. `fleetcom`
/// forwards only CPR (`ESC[<row>;<col>R`), DSR-5 (`ESC[0n`), and primary DA
/// (`ESC[?<params>c`) responses.
fn allowed_probe_response(resp: &str) -> bool {
    let Some(body) = resp.strip_prefix("\x1b[") else {
        return false;
    };
    // DSR 5: the fixed "terminal ok" status reply.
    if body == "0n" {
        return true;
    }
    // CPR: exactly two numeric fields.
    if let Some(params) = body.strip_suffix('R') {
        let mut fields = params.split(';');
        return matches!(
            (fields.next(), fields.next(), fields.next()),
            (Some(row), Some(col), None) if is_digits(row) && is_digits(col)
        );
    }
    // Primary DA: `?`-prefixed attributes. Secondary DA uses `>` and fails
    // the prefix check.
    if let Some(params) = body.strip_prefix('?').and_then(|b| b.strip_suffix('c')) {
        return !params.is_empty() && params.bytes().all(|b| b.is_ascii_digit() || b == b';');
    }
    false
}

fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// Sanitized-title cap in UTF-8 bytes. Truncation lands on a char boundary,
/// so the result can undershoot by up to three bytes.
const TITLE_MAX_BYTES: usize = 512;

/// The bidi formatting controls stripped from titles: ALM, LRM/RLM, the
/// LRE/RLE/PDF/LRO/RLO embedding block, and the LRI/RLI/FSI/PDI isolate
/// block. A fixed set instead of a general-category `Cf` strip: `Cf` needs a
/// Unicode table and would also delete ZWJ (U+200D), mangling joined emoji.
fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
    )
}

/// Sanitize child-controlled title text: strip C0/C1 controls and bidi
/// formatting controls, collapse each whitespace run to one space, trim the
/// ends, cap at [`TITLE_MAX_BYTES`] on a char boundary. Empty output means
/// the caller must unset its capture: a blank label is never displayed.
fn sanitize_title(raw: &str) -> String {
    let mut out = String::new();
    for c in raw.chars() {
        // `is_control` is category Cc: C0, DEL, and C1.
        if c.is_control() || is_bidi_control(c) {
            continue;
        }
        if c.is_whitespace() {
            // Collapsing also trims the start: a leading run sees empty
            // output and pushes nothing.
            if !out.is_empty() && !out.ends_with(' ') {
                out.push(' ');
            }
            continue;
        }
        out.push(c);
    }
    let cut = prefix_bytes(&out, TITLE_MAX_BYTES).len();
    out.truncate(cut);
    // Runs are already collapsed, so at most one trailing space survives
    // (possibly exposed by the truncation).
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// A sanitized title and the alternate-screen epoch that owns it. Each
/// alternate-screen entry starts a new epoch.
struct CapturedTitle {
    text: String,
    alt_epoch: u64,
}

/// Maximum number of zero-width characters retained per cell. This bounds
/// the otherwise unbounded vector created by repeated zero-width input.
const MAX_ZEROWIDTH: usize = 16;

/// Child-output byte threshold for scanning oversized zero-width vectors.
/// Counting bytes lets repeated marks trigger a scan without a separate timer.
const SWEEP_INTERVAL_BYTES: usize = 256 * 1024;

/// One task's parser, terminal grid, and buffered probe responses. The parser
/// and grid advance together under the same caller-held lock.
pub struct Emulator {
    term: Term<ProbeSink>,
    parser: Processor,
    responses: Arc<Mutex<Vec<String>>>,
    /// OSC 52 stores captured since the last drain.
    clipboard: ClipboardStores,
    /// Alt-screen and title facts, advanced at parser-event granularity by
    /// [`ObservedTerm`] during the parse itself.
    alt: AltScreen,
    /// Bumped once per grid advance; cheap change detection for consumers
    /// that poll the grid.
    revision: u64,
    /// Bytes ingested since the last zero-width scan.
    bytes_since_sweep: usize,
}

impl Emulator {
    /// Drain buffered `PtyWrite` responses through the allowlist, preserving
    /// generation order. Runs after `advance`/`stop_sync` returns, outside
    /// the listener callback.
    fn drain_allowed(&mut self) -> Vec<String> {
        let mut buf = self
            .responses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        buf.drain(..)
            .filter(|r| allowed_probe_response(r))
            .collect()
    }

    /// Drain captured OSC 52 stores and the latest oversized-store length.
    pub fn drain_clipboard(&mut self) -> ClipboardStores {
        std::mem::take(&mut self.clipboard)
    }

    /// Truncate zero-width characters in each active-grid cell to
    /// `MAX_ZEROWIDTH`. The scan covers the viewport and all scrollback rows.
    ///
    /// The inactive screen does not receive parsed output and is scanned only
    /// after it becomes active. Synchronized-update bytes are counted before
    /// they reach the grid, so cells applied after an early scan remain until
    /// a later scan.
    fn sweep_zerowidth(&mut self) {
        let grid = self.term.grid_mut();
        // History rows are negative Line indices, oldest first.
        let top = -(grid.history_size() as i32);
        let bottom = grid.screen_lines() as i32 - 1;
        for line in top..=bottom {
            for cell in &mut grid[Line(line)][..] {
                if !cell.zerowidth().is_some_and(|z| z.len() > MAX_ZEROWIDTH) {
                    continue;
                }
                // Rebuild the cell because `Cell` has no setter for
                // truncating its zero-width vector.
                let mut rebuilt = Cell {
                    c: cell.c,
                    fg: cell.fg,
                    bg: cell.bg,
                    flags: cell.flags,
                    extra: None,
                };
                if let Some(z) = cell.zerowidth() {
                    for &mark in &z[..MAX_ZEROWIDTH] {
                        rebuilt.push_zerowidth(mark);
                    }
                }
                rebuilt.set_underline_color(cell.underline_color());
                rebuilt.set_hyperlink(cell.hyperlink());
                *cell = rebuilt;
            }
        }
    }

    /// A fresh `rows`×`cols` grid retaining `scrollback` rows of history.
    pub fn new(rows: u16, cols: u16, scrollback: usize) -> Self {
        let responses = Arc::new(Mutex::new(Vec::new()));
        let config = Config {
            // Use fleetcom's per-task history limit instead of the backend
            // default.
            scrolling_history: scrollback,
            ..Config::default()
        };
        let term = Term::new(
            config,
            &GridSize {
                lines: rows as usize,
                columns: cols as usize,
            },
            ProbeSink {
                responses: Arc::clone(&responses),
            },
        );
        Self {
            term,
            parser: Processor::new(),
            responses,
            clipboard: ClipboardStores::default(),
            alt: AltScreen::default(),
            revision: 0,
            bytes_since_sweep: 0,
        }
    }

    /// Record one parser or synchronized-frame advance.
    fn observe_advance(&mut self) {
        self.revision += 1;
    }

    /// Parse raw child output into the grid. Returns the probe replies the
    /// backend generated that pass the allowlist, in generation order; the
    /// caller owns delivering them to the child.
    pub fn process(&mut self, bytes: &[u8]) -> Vec<String> {
        let mut observed = ObservedTerm {
            term: &mut self.term,
            alt: &mut self.alt,
            clipboard: &mut self.clipboard,
        };
        self.parser.advance(&mut observed, bytes);
        self.observe_advance();
        self.bytes_since_sweep = self.bytes_since_sweep.saturating_add(bytes.len());
        if self.bytes_since_sweep >= SWEEP_INTERVAL_BYTES {
            self.bytes_since_sweep = 0;
            self.sweep_zerowidth();
        }
        self.drain_allowed()
    }

    /// Stop synchronized buffering, record the advance, and drain allowed replies.
    fn land_sync_frame(&mut self) -> Vec<String> {
        let mut observed = ObservedTerm {
            term: &mut self.term,
            alt: &mut self.alt,
            clipboard: &mut self.clipboard,
        };
        self.parser.stop_sync(&mut observed);
        self.observe_advance();
        self.drain_allowed()
    }

    /// Terminate a `?2026` synchronized update whose timeout has expired,
    /// flushing the buffered frame into the grid; returns any allowlisted
    /// probe replies the flushed bytes generated. vte re-checks its timeout
    /// only when more bytes arrive, so a child that opens BSU and stalls
    /// would freeze its view until then. The core's periodic tick calls this
    /// to bound the stall. No-op while the timeout is still pending (an
    /// in-flight frame is not torn) and when no sync is open.
    pub fn flush_expired_sync(&mut self) -> Vec<String> {
        let expired = self
            .parser
            .sync_timeout()
            .sync_timeout()
            .is_some_and(|deadline| deadline <= Instant::now());
        if !expired {
            return Vec::new();
        }
        self.land_sync_frame()
    }

    /// Terminate an open `?2026` synchronized update regardless of its
    /// timeout, landing the buffered frame in the grid; returns any
    /// allowlisted probe replies the landed bytes generated. Exists for
    /// reader EOF: every child fd is closed, so the closing ESU can never
    /// arrive and `flush_expired_sync`'s deadline wait protects nothing:
    /// the frame is landed, not torn. No-op when no sync is open.
    pub fn finish_output(&mut self) -> Vec<String> {
        if self.parser.sync_timeout().sync_timeout().is_none() {
            return Vec::new();
        }
        self.land_sync_frame()
    }

    /// The visible screen as ANSI bytes, plus cursor position and whether the
    /// child hid the cursor.
    pub fn formatted(&self) -> (Vec<u8>, (u16, u16), bool) {
        crate::ansi::formatted(&self.term)
    }

    /// Plain-text contents of the visible screen, one line per row.
    pub fn contents(&self) -> String {
        crate::ansi::contents(&self.term)
    }

    /// Reconstruct retained terminal text from the oldest history row through
    /// the live viewport. Soft wraps join into logical lines, hard lines lose
    /// trailing padding, and the current scroll offset does not affect output.
    pub fn text_with_history(&self) -> String {
        let grid = self.term.grid();
        let top = -(grid.history_size() as i32);
        let bottom = grid.screen_lines() as i32 - 1;
        let last_col = grid.columns() - 1;
        let mut out = String::new();
        for row in top..=bottom {
            let row_start = out.len();
            let line = &grid[Line(row)];
            push_row_glyphs(&mut out, line);
            // A soft wrap continues on the next grid row.
            if line[Column(last_col)].flags.contains(Flags::WRAPLINE) {
                continue;
            }
            while out.len() > row_start && out.ends_with(' ') {
                out.pop();
            }
            if row < bottom {
                out.push('\n');
            }
        }
        out
    }

    /// Which mouse events the child asked for; the most recent DECSET wins
    /// (the backend keeps the modes mutually exclusive). DECSET 9 (X10) is
    /// not modeled, so an X10-only child gets no mouse reports.
    pub fn mouse_protocol_mode(&self) -> MouseProtocolMode {
        let mode = self.term.mode();
        if mode.contains(TermMode::MOUSE_MOTION) {
            MouseProtocolMode::AnyMotion
        } else if mode.contains(TermMode::MOUSE_DRAG) {
            MouseProtocolMode::ButtonMotion
        } else if mode.contains(TermMode::MOUSE_REPORT_CLICK) {
            MouseProtocolMode::PressRelease
        } else {
            MouseProtocolMode::None
        }
    }

    /// How mouse coordinates are encoded on the wire. SGR wins over UTF-8
    /// if both bits are set. Each DECSET normally clears the other bit.
    pub fn mouse_protocol_encoding(&self) -> MouseProtocolEncoding {
        let mode = self.term.mode();
        if mode.contains(TermMode::SGR_MOUSE) {
            MouseProtocolEncoding::Sgr
        } else if mode.contains(TermMode::UTF8_MOUSE) {
            MouseProtocolEncoding::Utf8
        } else {
            MouseProtocolEncoding::Default
        }
    }

    /// Whether the child is on the alternate screen (DECSET 1049).
    pub fn alternate_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    /// Whether wheel events should reach the child as arrow keys: requires
    /// the alternate screen and DECSET 1007, which defaults enabled.
    pub fn alternate_scroll(&self) -> bool {
        self.term
            .mode()
            .contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL)
    }

    /// Whether application cursor keys are on (DECSET 1).
    pub fn application_cursor(&self) -> bool {
        self.term.mode().contains(TermMode::APP_CURSOR)
    }

    /// Whether the child opted into bracketed paste (DECSET 2004).
    pub fn bracketed_paste(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// Rows the viewport is scrolled back from live output.
    pub fn scrollback(&self) -> usize {
        self.term.grid().display_offset()
    }

    /// Move the viewport `rows` back from live output; the backend clamps to
    /// retained history, so `usize::MAX` means the oldest stored row.
    pub fn set_scrollback(&mut self, rows: usize) {
        // Clamp contract: absolute target, capped at retained history. The
        // grid API is relative; both offsets are bounded by the history cap,
        // so the delta fits i32.
        let grid = self.term.grid();
        let target = rows.min(grid.history_size());
        let delta = target as i32 - grid.display_offset() as i32;
        self.term.scroll_display(Scroll::Delta(delta));
    }

    /// Resize the grid to `rows`×`cols`.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        // Re-snapshot an unchanged restored floor after reflow. A floor that
        // already differs records primary output after the alt-screen exit.
        let untouched = self
            .alt
            .leave_floor
            .as_deref()
            .is_some_and(|snapshot| live_floor_of(&self.term) == snapshot);
        self.term.resize(GridSize {
            lines: rows as usize,
            columns: cols as usize,
        });
        if untouched {
            self.alt.leave_floor = Some(live_floor_of(&self.term));
        }
        // Reflow changes grid contents without parser input, so cached screen
        // facts must be invalidated.
        self.revision += 1;
    }

    /// Grid size as `(rows, cols)`.
    #[cfg(test)]
    pub fn size(&self) -> (u16, u16) {
        (
            self.term.grid().screen_lines() as u16,
            self.term.grid().columns() as u16,
        )
    }
}

/// Capture accessors for the dashboard-preview resolution layer
/// (`crate::preview`).
impl Emulator {
    /// Monotonic count of grid advances. Bumps on every `process` call and
    /// on each sync-frame landing; equal reads mean the grid did not advance
    /// in between, so a poller can skip re-reading it.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Count of alt-screen entries observed so far.
    pub fn alt_epoch(&self) -> u64 {
        self.alt.epoch
    }

    /// The live floor captured at the most recent alt-screen exit, or `None`
    /// before the first exit.
    pub fn alt_leave_floor(&self) -> Option<&str> {
        self.alt.leave_floor.as_deref()
    }

    /// The window title. On the alternate screen: the captured title,
    /// honored only while its alt-screen epoch is current; a title from a
    /// previous alt session reads as `None`. On the primary screen: a live
    /// staged announce (a title not yet disclaimed by printed output)
    /// surfaces first, then a still-current captured title.
    pub fn title(&self) -> Option<&str> {
        // Surface a staged primary-screen title until printable output
        // disclaims it or an alternate-screen entry claims it.
        if !self.alternate_screen()
            && let Some(staged) = self.alt.staged_title.as_deref()
        {
            return Some(staged);
        }
        self.alt
            .title
            .as_ref()
            .filter(|t| t.alt_epoch == self.alt.epoch)
            .map(|t| t.text.as_str())
    }

    /// Last sanitized primary-screen title. Printable output and
    /// alternate-screen transitions do not clear it; an empty title or RIS
    /// does.
    pub fn primary_title(&self) -> Option<&str> {
        self.alt.primary_title.as_deref()
    }

    /// The last non-blank row of the live screen, trailing padding trimmed;
    /// empty when the screen is blank. Ignores the scrollback view offset:
    /// `contents` follows `display_offset`, which would make a scrolled-back
    /// task preview historical rows instead of live output.
    pub fn live_floor(&self) -> String {
        live_floor_of(&self.term)
    }

    /// Every live-viewport row, top to bottom, trailing padding trimmed: the
    /// summary adapters' structural scan input. Ignores the scrollback view
    /// offset for the same reason as [`Emulator::live_floor`].
    pub fn live_rows(&self) -> Vec<String> {
        (0..self.term.grid().screen_lines() as i32)
            .map(|row| live_row_text_of(&self.term, row))
            .collect()
    }
}

/// The last non-blank live row, with trailing padding removed. This free
/// function can run while `Term` is borrowed through a parser handler.
fn live_floor_of(term: &Term<ProbeSink>) -> String {
    for row in (0..term.grid().screen_lines() as i32).rev() {
        let text = live_row_text_of(term, row);
        if !text.is_empty() {
            return text;
        }
    }
    String::new()
}

/// Append a grid row's glyphs, omitting wide-character spacers, mapping tabs
/// to spaces, and preserving combining marks. Callers handle trailing spaces.
fn push_row_glyphs(out: &mut String, row: &Row<Cell>) {
    for cell in row {
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }
        out.push(if cell.c == '\t' { ' ' } else { cell.c });
        if let Some(zerowidth) = cell.zerowidth() {
            out.extend(zerowidth.iter());
        }
    }
}

/// Plain text of one live-viewport row, trailing padding trimmed. Rows
/// `0..screen_lines` address live output regardless of the display
/// offset; only display iteration follows the offset.
fn live_row_text_of(term: &Term<ProbeSink>, row: i32) -> String {
    let grid = term.grid();
    let line = &grid[Line(row)];
    let mut text = String::new();
    push_row_glyphs(&mut text, line);
    while text.ends_with(' ') {
        text.pop();
    }
    text
}

/// Alternate-screen and title state updated at parser-event boundaries. Each
/// nonempty sanitized primary-screen title is retained and staged. An
/// alternate-screen entry consumes the staged copy unless printable output
/// disclaims it first.
#[derive(Default)]
struct AltScreen {
    /// Count of alt-screen entries. Compared against
    /// `CapturedTitle::alt_epoch` to expire titles at app boundaries.
    epoch: u64,
    /// Alt bit after the last observed mode event: transition detection
    /// needs the previous value, and the mode register only holds the
    /// current one.
    last_alt: bool,
    /// The live floor captured at each alt-screen exit, at the mode event
    /// itself: successor bytes in the same read have not parsed yet, so
    /// this is exactly what the restore left visible. Preview finalization
    /// compares the final floor against it to distinguish restored primary
    /// content from content written after teardown.
    leave_floor: Option<String>,
    /// Sanitized title owned by an alt session, epoch-stamped at its event.
    title: Option<CapturedTitle>,
    /// Sanitized title announced on the primary screen and awaiting the next
    /// alternate-screen entry. Printable output disclaims it; a reset clears
    /// it.
    staged_title: Option<String>,
    /// Last sanitized primary-screen title. Unlike `staged_title`, printable
    /// output and alternate-screen entry do not clear it. An empty title or
    /// reset does.
    primary_title: Option<String>,
    /// Mirror of the backend's raw (unsanitized) current title, kept only
    /// so the title-stack shadow pushes what the backend pushes.
    raw_title: Option<String>,
    /// Shadow of the backend title stack, needed because a backend pop does
    /// not emit a handler title event.
    title_stack: Vec<Option<String>>,
}

/// Capacity of the backend title stack mirrored by [`AltScreen::title_stack`].
const TITLE_STACK_SHADOW_MAX: usize = 4096;

/// Delegating [`Handler`] that forwards every parser event to the wrapped
/// [`Term`] and observes alt-screen transitions the moment they happen.
///
/// # Forwarding invariant
///
/// `Handler` methods default to no-ops, so every method must delegate to
/// `Term`. `golden::emulator_wrapper_matches_the_raw_backend_on_every_fixture`
/// compares wrapper and raw-backend replays to detect missing delegation.
/// `clipboard_store` is captured by this wrapper instead of delegated.
///
/// # Synchronized updates
///
/// The parser buffers a synchronized-update frame and drives the handler
/// only when the frame lands (in `advance` or `stop_sync`, both routed
/// through this wrapper), so these events fire exactly when the grid
/// moves: the observer can never see a transition the grid has not
/// performed, which no byte-scanner could guarantee.
struct ObservedTerm<'a> {
    term: &'a mut Term<ProbeSink>,
    alt: &'a mut AltScreen,
    clipboard: &'a mut ClipboardStores,
}

impl ObservedTerm<'_> {
    /// Update alternate-screen state after a delegated mode change by
    /// comparing the backend's current and previously observed mode bits.
    fn observe_alt(&mut self) {
        let alt = self.term.mode().contains(TermMode::ALT_SCREEN);
        if alt && !self.alt.last_alt {
            self.alt.epoch += 1;
            // Promote a staged primary-screen announce into the new epoch:
            // the announce belongs to exactly this entry, so promotion
            // consumes it.
            if let Some(text) = self.alt.staged_title.take() {
                self.alt.title = Some(CapturedTitle {
                    text,
                    alt_epoch: self.alt.epoch,
                });
            }
        }
        if !alt && self.alt.last_alt {
            self.alt.leave_floor = Some(live_floor_of(self.term));
        }
        self.alt.last_alt = alt;
    }

    /// Assign a sanitized title to the current alternate-screen epoch, or
    /// stage and retain it when on the primary screen. An empty title clears
    /// captured, staged, and retained titles. Printable output, but not
    /// control traffic, disclaims a staged title.
    fn observe_title(&mut self, title: Option<String>) {
        self.alt.raw_title.clone_from(&title);
        let text = title
            .map(|raw| sanitize_title(&raw))
            .filter(|text| !text.is_empty());
        let Some(text) = text else {
            self.alt.title = None;
            self.alt.staged_title = None;
            self.alt.primary_title = None;
            return;
        };
        if self.term.mode().contains(TermMode::ALT_SCREEN) {
            self.alt.title = Some(CapturedTitle {
                text,
                alt_epoch: self.alt.epoch,
            });
        } else {
            self.alt.staged_title = Some(text.clone());
            self.alt.primary_title = Some(text);
        }
    }
}

/// Generate [`Handler`] methods that forward each call to the wrapped terminal.
macro_rules! delegate {
    ($($name:ident($($arg:ident: $ty:ty),*);)+) => {
        $(
            fn $name(&mut self, $($arg: $ty),*) {
                self.term.$name($($arg),*);
            }
        )+
    };
}

/// Handler delegation. Five methods also update observed state:
/// `set_private_mode`, `unset_private_mode`, and `reset_state` observe the
/// alt bit (RIS exits the alt screen too); `set_title` observes title
/// ownership; `input` disclaims a staged title. `push_title`/`pop_title`
/// maintain the shadow stack because the backend's pop restores through
/// its own internal `set_title`, which never re-enters this wrapper.
impl Handler for ObservedTerm<'_> {
    fn set_title(&mut self, a0: Option<String>) {
        self.term.set_title(a0.clone());
        self.observe_title(a0);
    }
    delegate! {
        set_cursor_style(a0: Option<vt::CursorStyle>);
        set_cursor_shape(a0: vt::CursorShape);
    }
    fn input(&mut self, a0: char) {
        self.term.input(a0);
        // Printable output disclaims a staged primary-screen title.
        if self.alt.staged_title.is_some() {
            self.alt.staged_title = None;
        }
    }
    delegate! {
        goto(a0: i32, a1: usize);
        goto_line(a0: i32);
        goto_col(a0: usize);
        insert_blank(a0: usize);
        move_up(a0: usize);
        move_down(a0: usize);
        identify_terminal(a0: Option<char>);
        device_status(a0: usize);
        move_forward(a0: usize);
        move_backward(a0: usize);
        move_down_and_cr(a0: usize);
        move_up_and_cr(a0: usize);
        put_tab(a0: u16);
        backspace();
        carriage_return();
        linefeed();
        bell();
        substitute();
        newline();
        set_horizontal_tabstop();
        scroll_up(a0: usize);
        scroll_down(a0: usize);
        insert_blank_lines(a0: usize);
        delete_lines(a0: usize);
        erase_chars(a0: usize);
        delete_chars(a0: usize);
        move_backward_tabs(a0: u16);
        move_forward_tabs(a0: u16);
        save_cursor_position();
        restore_cursor_position();
        clear_line(a0: vt::LineClearMode);
        clear_screen(a0: vt::ClearMode);
        clear_tabs(a0: vt::TabulationClearMode);
        set_tabs(a0: u16);
    }
    fn reset_state(&mut self) {
        self.term.reset_state();
        self.observe_alt();
        // RIS clears the backend title and title stack without separate
        // handler events. The captured title remains epoch-gated.
        self.alt.raw_title = None;
        self.alt.title_stack.clear();
        self.alt.staged_title = None;
        self.alt.primary_title = None;
    }
    delegate! {
        reverse_index();
        terminal_attribute(a0: vt::Attr);
        set_mode(a0: vt::Mode);
        unset_mode(a0: vt::Mode);
        report_mode(a0: vt::Mode);
    }
    fn set_private_mode(&mut self, a0: vt::PrivateMode) {
        self.term.set_private_mode(a0);
        self.observe_alt();
    }
    fn unset_private_mode(&mut self, a0: vt::PrivateMode) {
        self.term.unset_private_mode(a0);
        self.observe_alt();
    }
    delegate! {
        report_private_mode(a0: vt::PrivateMode);
        set_scrolling_region(a0: usize, a1: Option<usize>);
        set_keypad_application_mode();
        unset_keypad_application_mode();
        set_active_charset(a0: vt::CharsetIndex);
        configure_charset(a0: vt::CharsetIndex, a1: vt::StandardCharset);
        set_color(a0: usize, a1: vt::Rgb);
        dynamic_color_sequence(a0: String, a1: usize, a2: &str);
        reset_color(a0: usize);
    }
    /// Capture supported OSC 52 stores while preserving their selector.
    fn clipboard_store(&mut self, a0: u8, a1: &[u8]) {
        // Ignore unsupported selectors; the dispatcher maps an empty one to `c`.
        let Some(selector) = ClipboardKind::from_selector(&[a0]) else {
            return;
        };
        // Accept padded standard base64 containing UTF-8 text.
        let Ok(bytes) = B64.decode(a1) else { return };
        let Ok(text) = String::from_utf8(bytes) else {
            return;
        };
        // Retain only the most recent value for this selector, including on overflow.
        self.clipboard.stores.retain(|(k, _)| *k != selector);
        if text.len() > CLIPBOARD_STORE_MAX_BYTES {
            self.clipboard.oversized_len = Some(text.len());
            return;
        }
        self.clipboard.stores.push((selector, text));
    }
    fn clipboard_load(&mut self, a0: u8, a1: &str) {
        // The configured terminal policy denies clipboard loads.
        self.term.clipboard_load(a0, a1);
    }
    fn decaln(&mut self) {
        self.term.decaln();
    }
    fn push_title(&mut self) {
        self.term.push_title();
        // Mirror the backend's bounded push of its current raw title.
        if self.alt.title_stack.len() >= TITLE_STACK_SHADOW_MAX {
            self.alt.title_stack.remove(0);
        }
        self.alt.title_stack.push(self.alt.raw_title.clone());
    }
    fn pop_title(&mut self) {
        self.term.pop_title();
        // Replay the pop against the shadow: the restored value is a title
        // event in every sense (a popped `None` is a reset).
        if let Some(popped) = self.alt.title_stack.pop() {
            self.observe_title(popped);
        }
    }
    delegate! {
        text_area_size_pixels();
        text_area_size_chars();
        set_hyperlink(a0: Option<vt::Hyperlink>);
        set_mouse_cursor_icon(a0: vt::cursor_icon::CursorIcon);
        report_keyboard_mode();
        push_keyboard_mode(a0: vt::KeyboardModes);
        pop_keyboard_modes(a0: u16);
        set_keyboard_mode(a0: vt::KeyboardModes, a1: vt::KeyboardModesApplyBehavior);
        set_modify_other_keys(a0: vt::ModifyOtherKeys);
        report_modify_other_keys();
        set_scp(a0: vt::ScpCharPath, a1: vt::ScpUpdateMode);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use alacritty_terminal::{
        index::Column,
        term::cell::Flags,
        vte::ansi::{Color, NamedColor},
    };

    use super::*;

    /// The allowlist accepts only the advertised response shapes and rejects
    /// other backend responses, malformed variants, and unknown strings.
    #[test]
    fn probe_allowlist_forwards_only_the_advertised_shapes() {
        // Allowed: CPR, DSR-5 ok, primary DA.
        assert!(allowed_probe_response("\x1b[1;1R"));
        assert!(allowed_probe_response("\x1b[24;80R"));
        assert!(allowed_probe_response("\x1b[0n"));
        assert!(allowed_probe_response("\x1b[?6c"));
        assert!(allowed_probe_response("\x1b[?62;22c"));

        // Denied: other backend response classes.
        assert!(!allowed_probe_response("\x1b[>0;2606;1c")); // secondary DA
        assert!(!allowed_probe_response("\x1b[?1u")); // kitty keyboard report
        assert!(!allowed_probe_response("\x1b[?2026;2$y")); // DECRPM, private
        assert!(!allowed_probe_response("\x1b[4;2$y")); // DECRPM, ANSI
        assert!(!allowed_probe_response("\x1b[8;40;120t")); // window size

        // Denied: OSC-shaped replies (color/clipboard) and DCS.
        assert!(!allowed_probe_response("\x1b]4;1;rgb:aa/bb/cc\x1b\\"));
        assert!(!allowed_probe_response("\x1b]52;c;aGk=\x07"));
        assert!(!allowed_probe_response("\x1bP>|term 1.0\x1b\\"));

        // Denied: malformed near-misses of allowed shapes.
        assert!(!allowed_probe_response("\x1b[1R")); // CPR needs two fields
        assert!(!allowed_probe_response("\x1b[1;2;3R"));
        assert!(!allowed_probe_response("\x1b[;1R"));
        assert!(!allowed_probe_response("\x1b[?c")); // DA needs params
        assert!(!allowed_probe_response("\x1b[?6xc"));

        // Denied: arbitrary unknown responses.
        assert!(!allowed_probe_response("\x1b[?9999;42z"));
        assert!(!allowed_probe_response("unrecognized"));
        assert!(!allowed_probe_response(""));
    }

    /// End to end through `process`: the queries fleetcom answers produce
    /// exactly their replies, nothing more.
    #[test]
    fn allowed_probe_queries_are_answered() {
        let mut emu = Emulator::new(24, 80, 0);
        // CPR reports the parse-time cursor position, 1-based.
        assert!(emu.process(b"ab").is_empty());
        assert_eq!(emu.process(b"\x1b[6n"), vec!["\x1b[1;3R".to_string()]);
        // DSR 5: status ok.
        assert_eq!(emu.process(b"\x1b[5n"), vec!["\x1b[0n".to_string()]);
        // Primary DA, both spellings.
        assert_eq!(emu.process(b"\x1b[c"), vec!["\x1b[?6c".to_string()]);
        assert_eq!(emu.process(b"\x1b[0c"), vec!["\x1b[?6c".to_string()]);
    }

    /// End to end through `process`: queries outside the contract produce
    /// nothing, even though the backend generates replies for them.
    #[test]
    fn denied_probe_queries_are_silenced() {
        let mut emu = Emulator::new(24, 80, 0);
        // Secondary DA: backend replies, allowlist drops it.
        assert!(emu.process(b"\x1b[>c").is_empty());
        // DECRQM in both forms: DECRPM replies dropped.
        assert!(emu.process(b"\x1b[?2026$p").is_empty());
        assert!(emu.process(b"\x1b[4$p").is_empty());
        // Window-size report dropped.
        assert!(emu.process(b"\x1b[18t").is_empty());
        // Kitty keyboard query: disabled in config, no reply generated; the
        // allowlist would drop the `ESC[?...u` shape regardless.
        assert!(emu.process(b"\x1b[?u").is_empty());
    }

    /// An OSC 52 store is captured without a probe reply and drains once.
    #[test]
    fn osc52_store_is_captured_and_drains_once() {
        let mut emu = Emulator::new(4, 20, 0);
        assert!(
            emu.process(b"\x1b]52;c;aGVsbG8=\x07").is_empty(),
            "a store is not a probe reply"
        );
        let drained = emu.drain_clipboard();
        assert_eq!(
            drained.stores,
            vec![(ClipboardKind::Clipboard, "hello".to_string())]
        );
        assert_eq!(drained.oversized_len, None);
        let again = emu.drain_clipboard();
        assert!(again.stores.is_empty(), "a drain empties the buffer");
        assert_eq!(again.oversized_len, None);
    }

    /// Primary and selection stores remain distinct.
    #[test]
    fn osc52_p_and_s_selectors_stay_distinct() {
        let mut emu = Emulator::new(4, 20, 0);
        emu.process(b"\x1b]52;p;YQ==\x07\x1b]52;s;Yg==\x07");
        assert_eq!(
            emu.drain_clipboard().stores,
            vec![
                (ClipboardKind::Primary, "a".to_string()),
                (ClipboardKind::Selection, "b".to_string()),
            ]
        );
    }

    /// An empty OSC 52 selector targets the system clipboard.
    #[test]
    fn osc52_empty_selector_defaults_to_clipboard() {
        let mut emu = Emulator::new(4, 20, 0);
        emu.process(b"\x1b]52;;aGk=\x07");
        assert_eq!(
            emu.drain_clipboard().stores,
            vec![(ClipboardKind::Clipboard, "hi".to_string())]
        );
    }

    /// Unsupported OSC 52 selectors are ignored.
    #[test]
    fn osc52_unknown_selectors_drop() {
        let mut emu = Emulator::new(4, 20, 0);
        emu.process(b"\x1b]52;q;aGk=\x07");
        emu.process(b"\x1b]52;0;aGk=\x07");
        let drained = emu.drain_clipboard();
        assert!(drained.stores.is_empty());
        assert_eq!(drained.oversized_len, None);
    }

    /// Only the latest store for each selector is retained.
    #[test]
    fn osc52_last_store_wins_per_kind() {
        let mut emu = Emulator::new(4, 20, 0);
        emu.process(b"\x1b]52;c;Zmlyc3Q=\x07\x1b]52;c;c2Vjb25k\x07");
        assert_eq!(
            emu.drain_clipboard().stores,
            vec![(ClipboardKind::Clipboard, "second".to_string())]
        );
    }

    /// Different selectors buffer independently and drain in arrival order.
    #[test]
    fn osc52_kinds_buffer_independently() {
        let mut emu = Emulator::new(4, 20, 0);
        emu.process(b"\x1b]52;s;c2Vs\x07\x1b]52;p;cHJp\x07\x1b]52;c;Y2xpcA==\x07");
        assert_eq!(
            emu.drain_clipboard().stores,
            vec![
                (ClipboardKind::Selection, "sel".to_string()),
                (ClipboardKind::Primary, "pri".to_string()),
                (ClipboardKind::Clipboard, "clip".to_string()),
            ]
        );
    }

    /// Invalid base64, clear requests, and non-UTF-8 payloads are ignored.
    #[test]
    fn osc52_invalid_base64_and_clear_buffer_nothing() {
        let mut emu = Emulator::new(4, 20, 0);
        emu.process(b"\x1b]52;c;%%%\x07");
        emu.process(b"\x1b]52;c;!\x07");
        // "/w==" decodes to 0xFF: valid base64, invalid UTF-8.
        emu.process(b"\x1b]52;c;/w==\x07");
        let drained = emu.drain_clipboard();
        assert!(drained.stores.is_empty());
        assert_eq!(drained.oversized_len, None);
    }

    /// OSC 52 clipboard queries are denied without a reply.
    #[test]
    fn osc52_query_is_denied_without_a_reply() {
        let mut emu = Emulator::new(4, 20, 0);
        assert!(emu.process(b"\x1b]52;c;?\x07").is_empty());
        let drained = emu.drain_clipboard();
        assert!(drained.stores.is_empty());
        assert_eq!(drained.oversized_len, None);
    }

    /// ST-terminated OSC 52 stores are captured.
    #[test]
    fn osc52_st_terminated_store_is_captured() {
        let mut emu = Emulator::new(4, 20, 0);
        emu.process(b"\x1b]52;c;aGVsbG8=\x1b\\");
        assert_eq!(
            emu.drain_clipboard().stores,
            vec![(ClipboardKind::Clipboard, "hello".to_string())]
        );
    }

    /// An oversized store clears its selector and records its length.
    #[test]
    fn osc52_oversized_store_supersedes_its_kind_and_records_length() {
        let mut emu = Emulator::new(4, 20, 0);
        emu.process(b"\x1b]52;c;aGVsbG8=\x07");
        emu.process(b"\x1b]52;s;c2Vs\x07");
        emu.process(b"\x1b]52;p;cHJp\x07");
        // This payload decodes to two bytes over the cap.
        let reps = CLIPBOARD_STORE_MAX_BYTES / 3 + 1;
        let payload = "YWFh".repeat(reps);
        emu.process(format!("\x1b]52;c;{payload}\x07").as_bytes());
        let drained = emu.drain_clipboard();
        assert_eq!(
            drained.stores,
            vec![
                (ClipboardKind::Selection, "sel".to_string()),
                (ClipboardKind::Primary, "pri".to_string()),
            ],
            "the drop clears its own selector's slot and no other"
        );
        assert_eq!(drained.oversized_len, Some(reps * 3));
    }

    /// The stall the flush hook exists for: BSU plus a partial frame, then
    /// silence. The buffered frame must stay invisible while the timeout is
    /// pending (the hook must not tear an in-flight frame) and flush once the
    /// hook runs after expiry.
    #[test]
    fn stalled_sync_update_flushes_after_timeout() {
        let mut emu = Emulator::new(4, 20, 0);
        emu.process(b"before\x1b[?2026hafter");
        assert!(emu.contents().contains("before"));
        assert!(
            !emu.contents().contains("after"),
            "sync update must buffer the frame"
        );

        // Not expired yet: the hook must not flush.
        assert!(emu.flush_expired_sync().is_empty());
        assert!(!emu.contents().contains("after"));

        // vte's sync timeout is 150 ms; wait it out, then flush.
        std::thread::sleep(Duration::from_millis(200));
        emu.flush_expired_sync();
        assert!(
            emu.contents().contains("after"),
            "expired sync must flush the buffered frame"
        );

        // The emulator parses normally after the forced flush.
        emu.process(b" and on");
        assert!(emu.contents().contains("and on"));
    }

    /// The end-of-life landing `finish_output` exists for: BSU, a hint, no
    /// ESU ever. The frame must land without waiting out the sync timeout,
    /// and a clean emulator must pass through untouched.
    #[test]
    fn finish_output_lands_an_open_sync_frame() {
        let mut emu = Emulator::new(4, 20, 0);
        assert!(
            emu.finish_output().is_empty(),
            "no open frame: the parser must not be touched"
        );

        emu.process(b"before\x1b[?2026hafter");
        assert!(
            !emu.text_with_history().contains("after"),
            "premise: the unclosed frame buffers the text"
        );
        emu.finish_output();
        assert!(
            emu.text_with_history().contains("after"),
            "finish_output must land the frame with the timeout still pending"
        );

        // The emulator parses normally after the landing.
        emu.process(b" and on");
        assert!(emu.contents().contains("and on"));
    }

    /// Mouse modes follow the most recent DECSET, return to none
    /// when unset, and keep 1005 and 1006 mutually exclusive.
    #[test]
    fn mouse_modes_map_to_termmode_bits() {
        let mut emu = Emulator::new(24, 80, 0);
        assert_eq!(emu.mouse_protocol_mode(), MouseProtocolMode::None);
        emu.process(b"\x1b[?1000h");
        assert_eq!(emu.mouse_protocol_mode(), MouseProtocolMode::PressRelease);
        emu.process(b"\x1b[?1002h");
        assert_eq!(emu.mouse_protocol_mode(), MouseProtocolMode::ButtonMotion);
        emu.process(b"\x1b[?1003h");
        assert_eq!(emu.mouse_protocol_mode(), MouseProtocolMode::AnyMotion);
        emu.process(b"\x1b[?1003l");
        assert_eq!(emu.mouse_protocol_mode(), MouseProtocolMode::None);

        assert_eq!(
            emu.mouse_protocol_encoding(),
            MouseProtocolEncoding::Default
        );
        emu.process(b"\x1b[?1005h");
        assert_eq!(emu.mouse_protocol_encoding(), MouseProtocolEncoding::Utf8);
        emu.process(b"\x1b[?1006h");
        assert_eq!(emu.mouse_protocol_encoding(), MouseProtocolEncoding::Sgr);
        emu.process(b"\x1b[?1006l");
        assert_eq!(
            emu.mouse_protocol_encoding(),
            MouseProtocolEncoding::Default
        );
    }

    /// DECSET 9 (X10 press-only) is not modeled, so an X10-only child gets no
    /// mouse reports.
    #[test]
    fn x10_decset9_is_not_modeled() {
        let mut emu = Emulator::new(24, 80, 0);
        emu.process(b"\x1b[?9h");
        assert_eq!(emu.mouse_protocol_mode(), MouseProtocolMode::None);
    }

    /// The wheel-as-arrows gate requires the alternate screen and DECSET
    /// 1007, which defaults on.
    #[test]
    fn alternate_scroll_requires_alt_screen_and_1007() {
        let mut emu = Emulator::new(24, 80, 0);
        assert!(!emu.alternate_scroll(), "primary screen never gates open");
        emu.process(b"\x1b[?1049h");
        assert!(emu.alternate_scroll(), "1007 defaults on");
        emu.process(b"\x1b[?1007l");
        assert!(!emu.alternate_scroll(), "the child's veto must stick");
        emu.process(b"\x1b[?1007h");
        assert!(emu.alternate_scroll());
        emu.process(b"\x1b[?1049l");
        assert!(!emu.alternate_scroll(), "leaving the alt screen closes it");
    }

    /// The clamp contract `scroll_view` relies on: absolute target capped at
    /// retained history, `usize::MAX` → oldest stored row, 0 → live.
    #[test]
    fn scrollback_clamps_to_retained_history() {
        let mut emu = Emulator::new(4, 10, 100);
        for i in 0..12 {
            emu.process(format!("l{i}\r\n").as_bytes());
        }
        // 12 newlines on a 4-row screen, cursor starting at the top: the
        // first 3 move the cursor, the remaining 9 scroll rows into history.
        emu.set_scrollback(usize::MAX);
        assert_eq!(emu.scrollback(), 9);
        assert!(
            emu.contents().starts_with("l0"),
            "the oldest stored row must be displayed"
        );
        emu.set_scrollback(3);
        assert_eq!(emu.scrollback(), 3);
        emu.set_scrollback(10_000);
        assert_eq!(emu.scrollback(), 9, "over-scroll clamps at history");
        emu.set_scrollback(0);
        assert_eq!(emu.scrollback(), 0);
    }

    /// Retained text includes scrollback in chronological order and does not
    /// change when the viewport scroll offset changes.
    #[test]
    fn text_with_history_includes_scrolled_off_rows() {
        let mut emu = Emulator::new(4, 10, 100);
        for i in 0..12 {
            emu.process(format!("l{i}\r\n").as_bytes());
        }
        // 12 newlines on a 4-row screen: the first rows are history now.
        assert!(!emu.contents().contains("l0"));
        let full = emu.text_with_history();
        assert!(full.starts_with("l0"), "oldest history row leads");
        assert!(full.contains("l11"), "the live screen is included");
        // 9 history rows plus the 4-row viewport, one line per row.
        assert_eq!(full.split('\n').count(), 13);
        // The view offset must not change what is reported.
        emu.set_scrollback(usize::MAX);
        assert_eq!(emu.text_with_history(), full);
    }

    /// Soft wraps reconstruct one logical line without erasing explicit line
    /// breaks.
    #[test]
    fn text_with_history_joins_soft_wrapped_rows() {
        let mut emu = Emulator::new(6, 20, 100);
        let hint = "claude --resume 123e4567-e89b-42d3-a456-426614174000";
        emu.process(format!("before\r\n{hint}\r\nafter").as_bytes());
        let full = emu.text_with_history();
        assert!(
            full.contains(hint),
            "52 chars over 3 rows at 20 columns must come back unbroken: {full:?}"
        );
        // Explicit newlines still bound logical lines on both sides.
        assert!(full.contains(&format!("before\n{hint}\nafter")));
    }

    /// A wrapped codex named-thread hint remains one logical line.
    #[test]
    fn text_with_history_joins_codex_hint_across_rows() {
        let mut emu = Emulator::new(8, 40, 100);
        let hint = "To continue this session, run codex resume, then select \
                    mythic-otter (123e4567-e89b-42d3-a456-426614174000)";
        emu.process(hint.as_bytes());
        assert!(
            emu.text_with_history().contains(hint),
            "the hint spans 3 rows at 40 columns and must join unbroken"
        );
    }

    /// Wrap markers travel with rows into scrollback: a wrapped line pushed
    /// off the live screen still joins, including across the history to
    /// viewport boundary.
    #[test]
    fn text_with_history_joins_wrapped_rows_in_scrollback() {
        let mut emu = Emulator::new(4, 20, 100);
        let hint = "claude --resume 123e4567-e89b-42d3-a456-426614174000";
        emu.process(format!("{hint}\r\n").as_bytes());
        for i in 0..6 {
            emu.process(format!("pad {i}\r\n").as_bytes());
        }
        let full = emu.text_with_history();
        assert!(
            !emu.contents().contains("claude"),
            "premise: the hint scrolled fully into history"
        );
        assert!(
            full.contains(hint),
            "history rows keep their wrap markers: {full:?}"
        );
    }

    /// Top-anchored region scrollback remains reachable after shrinking and
    /// regrowing the grid, while new output continues to accumulate.
    #[test]
    fn region_scrolled_history_survives_resize() {
        let mut emu = Emulator::new(40, 120, 2000);
        for i in 1..=20 {
            emu.process(format!("\x1b[{i};1Hseed {i:02}").as_bytes());
        }
        // Insert history through newlines at a top-anchored region's bottom.
        emu.process(b"\x1b[1;20r\x1b[20;1H");
        for i in 1..=30 {
            emu.process(format!("\r\nhist {i:02}").as_bytes());
        }
        emu.process(b"\x1b[r");
        emu.set_scrollback(usize::MAX);
        assert_eq!(emu.scrollback(), 30);
        assert!(
            emu.contents().starts_with("seed 01"),
            "oldest region-scrolled row heads the history"
        );
        emu.set_scrollback(0);

        // Shrink, then keep inserting at the new geometry. Row counts shift
        // with reflow (shrinking parks the excess viewport rows in history),
        // so assert reachability and monotonic growth, not exact totals.
        emu.resize(30, 100);
        emu.process(b"\x1b[1;15r\x1b[15;1H");
        for i in 1..=20 {
            emu.process(format!("\r\nmore {i:02}").as_bytes());
        }
        emu.process(b"\x1b[r");
        emu.set_scrollback(usize::MAX);
        let after_shrink = emu.scrollback();
        assert!(
            after_shrink >= 50,
            "history keeps accumulating at the new size: {after_shrink}"
        );
        assert!(
            emu.contents().starts_with("seed 01"),
            "pre-resize history remains reachable"
        );

        // Growing back must not orphan anything either.
        emu.set_scrollback(0);
        emu.resize(40, 120);
        emu.set_scrollback(usize::MAX);
        assert!(
            emu.contents().contains("seed 01"),
            "history survives the round trip"
        );
        let live = {
            emu.set_scrollback(0);
            emu.contents()
        };
        assert!(
            live.contains("more 20"),
            "the newest insertion is on the live screen"
        );
    }

    /// Total zero-width characters retained across the viewport and history.
    fn total_zerowidth(emu: &Emulator) -> usize {
        let grid = emu.term.grid();
        let top = -(grid.history_size() as i32);
        let bottom = grid.screen_lines() as i32 - 1;
        (top..=bottom)
            .flat_map(|line| grid[Line(line)][..].iter())
            .map(|cell| cell.zerowidth().map_or(0, <[char]>::len))
            .sum()
    }

    /// A full interval of combining marks on one cell is capped before
    /// `process` returns.
    #[test]
    fn zerowidth_spam_on_one_cell_is_capped() {
        let mut emu = Emulator::new(4, 10, 0);
        emu.process(b"a");
        // U+0301 is two UTF-8 bytes, making this chunk one full interval.
        let chunk = "\u{0301}".repeat(SWEEP_INTERVAL_BYTES / 2);

        emu.process(chunk.as_bytes());
        let len = emu.term.grid()[Line(0)][Column(0)]
            .zerowidth()
            .map_or(0, <[char]>::len);
        assert!(
            len <= MAX_ZEROWIDTH,
            "hot cell retains {len} marks after process returned"
        );

        // A second interval on the same cell is capped independently.
        emu.process(chunk.as_bytes());
        assert!(
            total_zerowidth(&emu) <= MAX_ZEROWIDTH,
            "marks retained beyond the single spammed cell"
        );
    }

    /// A scan caps combining marks in every cell across a populated row.
    #[test]
    fn zerowidth_spray_across_cells_is_capped() {
        let mut emu = Emulator::new(4, 80, 0);
        let marks = "\u{0301}".repeat(2048);
        let mut payload = String::new();
        for col in 1..=80 {
            payload.push_str(&format!("\x1b[2;{col}Hx"));
            payload.push_str(&marks);
        }
        assert!(
            payload.len() >= SWEEP_INTERVAL_BYTES,
            "payload must cross the sweep interval in one call"
        );
        emu.process(payload.as_bytes());

        let grid = emu.term.grid();
        for col in 0..80 {
            let z = grid[Line(1)][Column(col)]
                .zerowidth()
                .expect("sprayed cell lost its marks entirely");
            // Exactly the cap: truncation keeps the first marks, it does not
            // clear the cell.
            assert_eq!(z.len(), MAX_ZEROWIDTH, "column {col}");
            assert!(z.iter().all(|&m| m == '\u{0301}'));
        }
        assert!(total_zerowidth(&emu) <= 80 * MAX_ZEROWIDTH);
    }

    /// A scan triggered by unrelated output preserves an under-limit styled
    /// cluster and all of its cell attributes.
    #[test]
    fn legitimate_cluster_survives_sweep_untouched() {
        let mut emu = Emulator::new(4, 80, 0);
        let cluster = "\x1b]8;;https://example.com\x1b\\\
                       \x1b[1;4;31;44m\x1b[58;5;42m\
                       e\u{0301}\u{0302}\u{0304}\
                       \x1b[0m\x1b]8;;\x1b\\";
        emu.process(cluster.as_bytes());

        // CUP keeps the filler on row 2 while enough bytes trigger a scan.
        let filler = format!("\x1b[2;1H{}", "x".repeat(64)).repeat(1024);
        let mut fed = cluster.len();
        while fed < SWEEP_INTERVAL_BYTES {
            emu.process(filler.as_bytes());
            fed += filler.len();
        }

        let cell = &emu.term.grid()[Line(0)][Column(0)];
        assert_eq!(cell.c, 'e');
        assert_eq!(
            cell.zerowidth(),
            Some(&['\u{0301}', '\u{0302}', '\u{0304}'][..])
        );
        assert_eq!(cell.fg, Color::Named(NamedColor::Red));
        assert_eq!(cell.bg, Color::Named(NamedColor::Blue));
        assert!(cell.flags.contains(Flags::BOLD | Flags::UNDERLINE));
        assert_eq!(cell.underline_color(), Some(Color::Indexed(42)));
        assert_eq!(
            cell.hyperlink().map(|h| h.uri().to_owned()),
            Some("https://example.com".to_owned())
        );
    }

    /// A scan also caps a cell that entered scrollback before the threshold.
    #[test]
    fn history_cells_are_swept() {
        let mut emu = Emulator::new(4, 10, 100);
        // This oversized cluster remains below the scan threshold.
        let spam = format!("h{}", "\u{0301}".repeat(4096));
        emu.process(spam.as_bytes());
        emu.process(b"\r\n\r\n\r\n\r\n\r\n\r\n");

        // Confirm that the oversized cell reached history before the scan.
        let find_h = |emu: &Emulator| -> (i32, usize) {
            let grid = emu.term.grid();
            let top = -(grid.history_size() as i32);
            (top..0)
                .find_map(|line| {
                    let cell = &grid[Line(line)][Column(0)];
                    (cell.c == 'h').then(|| (line, cell.zerowidth().map_or(0, <[char]>::len)))
                })
                .expect("spammed row must be in history")
        };
        let (line, len) = find_h(&emu);
        assert!(line < 0);
        assert_eq!(len, 4096, "excess must predate the sweep");

        // CUP keeps filler on the last row so the history position is stable.
        let filler = "\x1b[4;1Hxxxxxxxx".repeat(1024);
        let mut fed = spam.len() + 12;
        while fed < SWEEP_INTERVAL_BYTES {
            emu.process(filler.as_bytes());
            fed += filler.len();
        }

        let (line_after, len_after) = find_h(&emu);
        assert_eq!(line_after, line, "row must not have moved");
        assert_eq!(len_after, MAX_ZEROWIDTH);
    }

    /// C0, DEL, and C1 controls are stripped from titles.
    #[test]
    fn sanitize_strips_c0_and_c1_controls() {
        assert_eq!(sanitize_title("a\x07b\x1bc\u{7f}d\u{9b}e"), "abcde");
        assert_eq!(sanitize_title("\x01\x02\x03"), "");
    }

    /// Each bidi formatting control is stripped individually: the fixed set
    /// the sanitizer names, not a general `Cf` sweep.
    #[test]
    fn sanitize_strips_each_bidi_control() {
        let bidi = [
            '\u{061C}', '\u{200E}', '\u{200F}', '\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}',
            '\u{202E}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}',
        ];
        for c in bidi {
            assert_eq!(
                sanitize_title(&format!("a{c}b")),
                "ab",
                "U+{:04X}",
                c as u32
            );
        }
    }

    /// ZWJ (U+200D) must survive: it is format-category like the bidi
    /// controls, but stripping it would break joined emoji.
    #[test]
    fn sanitize_preserves_zwj_sequences() {
        let technologist = "\u{1F469}\u{200D}\u{1F4BB}";
        assert_eq!(sanitize_title(technologist), technologist);
    }

    /// Whitespace runs collapse to one space and the ends are trimmed.
    #[test]
    fn sanitize_collapses_and_trims_whitespace() {
        assert_eq!(sanitize_title("  a \t\r\n b  "), "a b");
        assert_eq!(sanitize_title(" \t "), "");
    }

    /// The byte cap cannot split a UTF-8 sequence: 512 is not a multiple of
    /// three, so a stream of three-byte chars must cut at the previous
    /// boundary.
    #[test]
    fn sanitize_caps_on_a_char_boundary() {
        let long = "\u{20AC}".repeat(200); // 600 bytes of '€'
        let out = sanitize_title(&long);
        assert!(out.len() <= TITLE_MAX_BYTES);
        assert_eq!(out.len(), 510);
        assert_eq!(out.chars().count(), 170);
    }

    /// Within one chunk only the last title event matters; an empty OSC title
    /// and a ResetTitle (title-stack pop, CSI 23 t) both unset the capture
    /// rather than freezing the previous one.
    #[test]
    fn empty_title_and_reset_unset_the_capture() {
        let mut emu = Emulator::new(4, 20, 0);
        emu.process(b"\x1b]0;first\x07\x1b]0;second\x07");
        assert_eq!(emu.title(), Some("second"), "last event of a chunk wins");
        emu.process(b"\x1b]0;\x07");
        assert_eq!(emu.title(), None, "an empty title clears, never blanks");

        // CSI 22 t pushes the pre-title state (no title); popping it back
        // with CSI 23 t makes the backend emit ResetTitle.
        let mut emu = Emulator::new(4, 20, 0);
        emu.process(b"\x1b[22t");
        emu.process(b"\x1b]0;named\x07");
        assert_eq!(emu.title(), Some("named"));
        emu.process(b"\x1b[23t");
        assert_eq!(emu.title(), None, "ResetTitle must unset the capture");
    }

    /// A title immediately before alt-screen entry is promoted from staging
    /// into the new epoch when no glyph intervenes.
    #[test]
    fn title_entering_alt_in_one_chunk_is_honored() {
        let mut emu = Emulator::new(4, 20, 0);
        emu.process(b"\x1b]0;app\x07\x1b[?1049h");
        assert_eq!(emu.alt_epoch(), 1);
        assert_eq!(emu.title(), Some("app"));
    }

    /// Printable primary-screen output disclaims a staged title; control-only
    /// traffic leaves it available for the next alternate-screen entry.
    #[test]
    fn title_before_alt_entry_in_a_prior_chunk_expires() {
        let mut emu = Emulator::new(4, 20, 0);
        emu.process(b"\x1b]0;shell\x07");
        assert_eq!(emu.title(), Some("shell"));
        emu.process(b"$ make\r\n");
        emu.process(b"\x1b[?1049h");
        assert_eq!(
            emu.title(),
            None,
            "printed output disclaimed the staged title"
        );
    }

    /// A leave and re-entry expires the first alternate-screen epoch's title,
    /// independent of how the bytes are split across reads.
    #[test]
    fn in_alt_title_expires_across_a_bounce_on_any_read_boundary() {
        for split in [false, true] {
            let mut emu = Emulator::new(4, 20, 0);
            emu.process(b"\x1b[?1049hui");
            assert_eq!(emu.alt_epoch(), 1);
            if split {
                emu.process(b"\x1b]0;first\x07");
                emu.process(b"\x1b[?1049l\x1b[?1049h");
            } else {
                emu.process(b"\x1b]0;first\x07\x1b[?1049l\x1b[?1049h");
            }
            assert_eq!(emu.alt_epoch(), 2, "split={split}");
            assert_eq!(
                emu.title(),
                None,
                "split={split}: A's title must not label B"
            );
        }
    }

    /// grok's announce shape: a primary-screen title, a control-only gap
    /// (clears and cursor moves), then the alt entry. Honored on either
    /// read boundary, because control traffic never disclaims staging.
    #[test]
    fn staged_title_survives_a_control_only_gap_into_the_entry() {
        for split in [false, true] {
            let mut emu = Emulator::new(4, 20, 0);
            if split {
                emu.process(b"\x1b]0;grok\x07\x1b[2J\x1b[H");
                emu.process(b"\x1b[?1049h");
            } else {
                emu.process(b"\x1b]0;grok\x07\x1b[2J\x1b[H\x1b[?1049h");
            }
            assert_eq!(emu.alt_epoch(), 1, "split={split}");
            assert_eq!(emu.title(), Some("grok"), "split={split}");
        }
    }

    /// One printed glyph between a primary-screen title and the entry
    /// disclaims the staging: a prompt-titling shell never leaks its title
    /// into the next app.
    #[test]
    fn staged_title_is_disclaimed_by_a_single_glyph() {
        let mut emu = Emulator::new(4, 20, 0);
        emu.process(b"\x1b]0;shell\x07x\x1b[?1049h");
        assert_eq!(emu.alt_epoch(), 1);
        assert_eq!(emu.title(), None);
    }

    /// A primary-screen title between two alternate-screen sessions stages
    /// into the second session when no printable output intervenes.
    #[test]
    fn inter_app_title_with_no_glyphs_stages_into_the_next_app() {
        let mut emu = Emulator::new(4, 20, 0);
        emu.process(b"\x1b[?1049hui A");
        emu.process(b"\x1b[?1049l\x1b]0;handoff\x07\x1b[?1049h");
        assert_eq!(emu.alt_epoch(), 2);
        assert_eq!(emu.title(), Some("handoff"));
    }

    /// Each alt entry advances the epoch and expires prior titles; leaving
    /// does not advance it, so a title stays honored across the exit.
    #[test]
    fn each_alt_entry_advances_the_epoch() {
        let mut emu = Emulator::new(4, 20, 0);
        emu.process(b"\x1b]0;one\x07\x1b[?1049h");
        assert_eq!(emu.alt_epoch(), 1);
        emu.process(b"\x1b[?1049l");
        assert_eq!(emu.alt_epoch(), 1, "leaving must not advance the epoch");
        assert_eq!(emu.title(), Some("one"), "epoch still current after exit");
        emu.process(b"\x1b[?1049h");
        assert_eq!(emu.alt_epoch(), 2);
        assert_eq!(emu.title(), None, "re-entry expires the previous title");
    }

    /// A title followed by leaving the alt screen: the title event fires
    /// while the alt screen is still active, capturing into the current
    /// epoch, and the exit keeps the epoch, so it stays honored.
    #[test]
    fn title_just_before_alt_exit_stays_honored() {
        let mut emu = Emulator::new(4, 20, 0);
        emu.process(b"\x1b[?1049h");
        assert_eq!(emu.alt_epoch(), 1);
        emu.process(b"\x1b]0;done\x07\x1b[?1049l");
        assert!(!emu.alternate_screen());
        assert_eq!(emu.title(), Some("done"));
    }

    /// Printable output clears the staged title but not the retained title.
    #[test]
    fn primary_title_survives_the_printable_output_that_disclaims_staging() {
        let mut emu = Emulator::new(4, 20, 0);
        emu.process(b"\x1b]0;omp\x07");
        assert_eq!(emu.title(), Some("omp"), "premise: the announce staged");
        assert_eq!(emu.primary_title(), Some("omp"));
        emu.process(b"$ ls\r\n");
        assert_eq!(emu.title(), None, "staged: disclaimed by printed output");
        assert_eq!(emu.primary_title(), Some("omp"), "retained: survives it");
    }

    /// The latest nonempty primary-screen title replaces the retained title.
    #[test]
    fn primary_title_is_overwritten_by_a_newer_announce() {
        let mut emu = Emulator::new(4, 20, 0);
        emu.process("\x1b]0;\u{3c0} >\x07build output\r\n".as_bytes());
        assert_eq!(emu.primary_title(), Some("\u{3c0} >"));
        emu.process("\x1b]0;\u{3c0} > check\x07".as_bytes());
        assert_eq!(emu.primary_title(), Some("\u{3c0} > check"));
    }

    /// An empty title or RIS clears the retained primary-screen title.
    #[test]
    fn empty_announce_and_reset_clear_the_primary_title() {
        let mut emu = Emulator::new(4, 20, 0);
        emu.process(b"\x1b]0;codex\x07");
        assert_eq!(emu.primary_title(), Some("codex"));
        emu.process(b"\x1b]0;\x07");
        assert_eq!(emu.primary_title(), None, "an empty announce clears");

        let mut emu = Emulator::new(4, 20, 0);
        emu.process(b"\x1b]0;codex\x07");
        emu.process(b"\x1bc");
        assert_eq!(emu.primary_title(), None, "RIS clears");
    }

    /// Alternate-screen title changes do not replace the retained primary
    /// title.
    #[test]
    fn primary_title_is_unaffected_by_an_alt_round_trip() {
        let mut emu = Emulator::new(4, 20, 0);
        emu.process(b"\x1b]0;shell\x07\x1b[?1049h");
        assert_eq!(emu.alt_epoch(), 1);
        assert_eq!(emu.title(), Some("shell"), "premise: entry claimed staging");
        emu.process(b"\x1b]0;altapp\x07");
        assert_eq!(emu.title(), Some("altapp"));
        assert_eq!(
            emu.primary_title(),
            Some("shell"),
            "an in-alt announce must not touch the slot"
        );
        emu.process(b"\x1b[?1049l");
        assert_eq!(emu.primary_title(), Some("shell"));
    }

    /// The end-of-life landing runs the same bookkeeping as `process`: a
    /// title and alt entry buffered inside a never-closed ?2026 frame must
    /// count when `finish_output` lands it.
    #[test]
    fn finish_output_runs_the_advance_bookkeeping() {
        let mut emu = Emulator::new(4, 20, 0);
        emu.process(b"\x1b[?2026h\x1b]0;app\x07\x1b[?1049hui");
        let rev = emu.revision();
        assert_eq!(emu.alt_epoch(), 0, "premise: the open frame buffers 1049h");
        assert_eq!(emu.title(), None, "premise: the open frame buffers OSC 0");
        emu.finish_output();
        assert_eq!(emu.revision(), rev + 1, "the landing is a grid advance");
        assert_eq!(emu.alt_epoch(), 1);
        assert_eq!(emu.title(), Some("app"));
    }

    /// An expired synchronized frame updates the revision, alternate-screen
    /// epoch, and title when it lands.
    #[test]
    fn flush_expired_sync_runs_the_advance_bookkeeping() {
        let mut emu = Emulator::new(4, 20, 0);
        emu.process(b"\x1b[?2026h\x1b]0;app\x07\x1b[?1049hui");
        let rev = emu.revision();
        std::thread::sleep(Duration::from_millis(200));
        emu.flush_expired_sync();
        assert_eq!(emu.revision(), rev + 1, "the landing is a grid advance");
        assert_eq!(emu.alt_epoch(), 1);
        assert_eq!(emu.title(), Some("app"));
    }

    /// The revision counts every `process` call; a landing hook that finds
    /// no open frame did not advance the grid and must not bump it.
    #[test]
    fn revision_is_monotonic_and_noop_landings_do_not_bump() {
        let mut emu = Emulator::new(4, 20, 0);
        assert_eq!(emu.revision(), 0);
        emu.process(b"a");
        assert_eq!(emu.revision(), 1);
        emu.process(b"b");
        assert_eq!(emu.revision(), 2);
        emu.flush_expired_sync();
        assert_eq!(emu.revision(), 2, "no open frame: nothing advanced");
        emu.finish_output();
        assert_eq!(emu.revision(), 2, "no open frame: nothing advanced");
    }

    /// A resize reflows the grid with no bytes arriving: revision-keyed
    /// pollers would otherwise carry a pre-resize snapshot indefinitely on
    /// a quiet task.
    #[test]
    fn resize_bumps_the_revision_without_bytes() {
        let mut emu = Emulator::new(4, 20, 0);
        emu.process(b"hello\r\nworld");
        let before = emu.revision();
        emu.resize(6, 30);
        assert_eq!(emu.revision(), before + 1);
    }

    /// The alternate-screen exit snapshot captures the restored floor before
    /// any following bytes, including bytes coalesced into the same read.
    #[test]
    fn alt_leave_floor_snapshots_the_restore() {
        let mut emu = Emulator::new(4, 20, 0);
        emu.process(b"junk\r\n");
        assert_eq!(emu.alt_leave_floor(), None, "no exit yet");
        emu.process(b"\x1b[?1049halt body");
        emu.process(b"\x1b[?1049l");
        assert_eq!(emu.alt_leave_floor(), Some("junk"));

        emu.process(b"done\r\n");
        assert_eq!(
            emu.alt_leave_floor(),
            Some("junk"),
            "later chunks leave the snapshot alone"
        );
        assert_eq!(emu.live_floor(), "done");

        emu.process(b"\x1b[?1049halt again");
        emu.process(b"\x1b[?1049lcoalesced\r\n");
        assert_eq!(
            emu.alt_leave_floor(),
            Some("done"),
            "a coalesced read still snapshots at the mode event"
        );
        assert_eq!(emu.live_floor(), "coalesced");
    }

    /// A leave and re-entry within one read advances the epoch and expires the
    /// preceding alternate-screen title.
    #[test]
    fn same_read_alt_bounce_advances_the_epoch_and_expires_the_title() {
        let mut emu = Emulator::new(4, 20, 0);
        emu.process(b"\x1b[?1049h\x1b]0;first app\x07ui");
        assert_eq!(emu.alt_epoch(), 1);
        assert_eq!(emu.title(), Some("first app"), "premise: title honored");

        emu.process(b"\x1b[?1049l\x1b[?1049h");
        assert_eq!(emu.alt_epoch(), 2, "the bounce is two transitions");
        assert_eq!(
            emu.title(),
            None,
            "the old app's title must not survive the swap"
        );
    }

    /// `live_floor` reads the live grid's last non-blank row even while the
    /// viewport is scrolled back; `contents` follows the offset instead.
    #[test]
    fn live_floor_ignores_the_scrollback_offset() {
        let mut emu = Emulator::new(4, 10, 100);
        for i in 0..12 {
            emu.process(format!("l{i}\r\n").as_bytes());
        }
        emu.process(b"latest");
        assert_eq!(emu.live_floor(), "latest");
        emu.set_scrollback(usize::MAX);
        assert!(
            emu.contents().starts_with("l0"),
            "premise: the view shows history"
        );
        assert!(!emu.contents().contains("latest"));
        assert_eq!(
            emu.live_floor(),
            "latest",
            "the floor must not follow the view"
        );
    }

    /// A blank screen has no floor.
    #[test]
    fn live_floor_of_a_blank_screen_is_empty() {
        let emu = Emulator::new(4, 10, 0);
        assert_eq!(emu.live_floor(), "");
    }
}
