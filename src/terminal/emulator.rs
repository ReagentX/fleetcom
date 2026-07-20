//! `fleetcom` reconstructs each task's terminal state from raw PTY output.
//! `alacritty_terminal` provides the parser, visible grid, and scrollback.

use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

use alacritty_terminal::{
    Term,
    event::{Event, EventListener},
    grid::{Dimensions, Scroll},
    index::{Column, Line},
    term::{
        Config, TermMode,
        cell::{Cell, Flags},
    },
    vte::ansi::{self as vt, Handler, Processor},
};

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

/// Routes the backend's `Event::PtyWrite` probe responses into a buffer.
/// The listener fires inside `Processor::advance`, while the caller holds
/// the emulator lock, so it only stores, never blocks; the caller drains,
/// filters, and sanitizes after `advance` returns. Every other backend
/// event (titles, clipboard, color requests, bell) is discarded here: the
/// default-deny probe policy starts with what never gets buffered, and
/// titles are observed at their handler event by [`ObservedTerm`], not
/// through this listener.
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
/// the caller must unset its capture — a blank label is never displayed.
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
    if out.len() > TITLE_MAX_BYTES {
        let mut cut = TITLE_MAX_BYTES;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
    }
    // Runs are already collapsed, so at most one trailing space survives
    // (possibly exposed by the truncation).
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// A sanitized title plus the alt-screen epoch it was captured in — at the
/// title event itself, or by promotion from staging at the entry event (see
/// [`ObservedTerm::observe_title`]). The title is honored only while its
/// epoch is current: each alt-screen entry starts a new epoch, so a title
/// from a previous full-screen app never labels the app that replaced it.
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
            alt: AltScreen::default(),
            revision: 0,
            bytes_since_sweep: 0,
        }
    }

    /// The shared bookkeeping step behind every grid advance — `process` and
    /// both sync-frame landings. Nothing but the revision bump remains:
    /// epochs, teardown snapshots, and title ownership are all observed at
    /// their parser events by [`ObservedTerm`], so no preview semantics
    /// depend on where PTY reads split.
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
        let mut observed = ObservedTerm {
            term: &mut self.term,
            alt: &mut self.alt,
        };
        self.parser.stop_sync(&mut observed);
        self.observe_advance();
        self.drain_allowed()
    }

    /// Terminate an open `?2026` synchronized update regardless of its
    /// timeout, landing the buffered frame in the grid; returns any
    /// allowlisted probe replies the landed bytes generated. Exists for
    /// reader EOF: every child fd is closed, so the closing ESU can never
    /// arrive and `flush_expired_sync`'s deadline wait protects nothing —
    /// the frame is landed, not torn. No-op when no sync is open.
    pub fn finish_output(&mut self) -> Vec<String> {
        if self.parser.sync_timeout().sync_timeout().is_none() {
            return Vec::new();
        }
        let mut observed = ObservedTerm {
            term: &mut self.term,
            alt: &mut self.alt,
        };
        self.parser.stop_sync(&mut observed);
        self.observe_advance();
        self.drain_allowed()
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
            for col in 0..grid.columns() {
                let cell = &line[Column(col)];
                // Spacers have no glyph; terminal tabs occupy visible spaces.
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
        // The teardown snapshot must survive the reflow: when the floor
        // still equals it (nothing written since the alt exit), the resize
        // rewraps one side of the finalize comparison, so re-snapshot the
        // reflowed floor afterwards to keep both sides equal. When they
        // already differ, real output arrived and the mismatch is evidence
        // — leave it standing. Never simply clear: a cleared snapshot
        // fails the teardown carve-out and freezes restored junk, the
        // wrong direction. Exact equality in both branches; no heuristic.
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
        // A resize reflows the grid — wrapping, row positions — without any
        // bytes arriving, so revision-keyed pollers must re-read. A bare
        // bump, not `observe_advance`: that step consumes byte-driven state
        // (a pending title event) that a resize never produces, and
        // consuming it here would misattribute it.
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

    /// The floor snapshotted at the most recent alt-screen exit (see the
    /// field docs); `None` until the child first leaves the alt screen.
    pub fn alt_leave_floor(&self) -> Option<&str> {
        self.alt.leave_floor.as_deref()
    }

    /// The window title. On the alternate screen: the captured title,
    /// honored only while its alt-screen epoch is current — a title from a
    /// previous alt session reads as `None`. On the primary screen: a live
    /// staged announce (a title not yet disclaimed by printed output)
    /// surfaces first, then a still-current captured title.
    pub fn title(&self) -> Option<&str> {
        // On the primary screen a live staged announce surfaces, so shell
        // titles read as before; the preview cascade never consults titles
        // there. The captured title keeps its epoch gate unchanged.
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

    /// The last non-blank row of the live screen, trailing padding trimmed;
    /// empty when the screen is blank. Ignores the scrollback view offset —
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

/// The last non-blank row of `term`'s live screen, trailing padding
/// trimmed; empty when the screen is blank (see [`Emulator::live_floor`]).
/// Free over the term so the alt-exit observer can snapshot mid-advance,
/// while the `&mut Term` is borrowed as a handler.
fn live_floor_of(term: &Term<ProbeSink>) -> String {
    for row in (0..term.grid().screen_lines() as i32).rev() {
        let text = live_row_text_of(term, row);
        if !text.is_empty() {
            return text;
        }
    }
    String::new()
}

/// Plain text of one live-viewport row, trailing padding trimmed. Rows
/// `0..screen_lines` address live output regardless of the display
/// offset; only display iteration follows the offset.
fn live_row_text_of(term: &Term<ProbeSink>, row: i32) -> String {
    let grid = term.grid();
    let line = &grid[Line(row)];
    let mut text = String::new();
    for col in 0..grid.columns() {
        let cell = &line[Column(col)];
        // Spacers have no glyph; terminal tabs occupy visible spaces.
        if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            continue;
        }
        text.push(if cell.c == '\t' { ' ' } else { cell.c });
        if let Some(zerowidth) = cell.zerowidth() {
            text.extend(zerowidth.iter());
        }
    }
    while text.ends_with(' ') {
        text.pop();
    }
    text
}

/// Alt-screen and title facts observed at parser-event granularity: every
/// field moves at its exact event, never at read boundaries, so no preview
/// semantics depend on where PTY reads split — a read that coalesces a
/// teardown with successor output, a leave and re-enter, or a title with a
/// following mode flip all observe identically however the reads land.
///
/// The one accepted residual: a title emitted between two apps (after A's
/// 1049l, before B's 1049h) with no intervening glyphs stages into B —
/// indistinguishable from grok's legitimate pre-entry announce by any fact
/// held here.
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
    /// compares the final floor against it to tell restored pre-launch
    /// junk from real primary output written after teardown.
    leave_floor: Option<String>,
    /// Sanitized title owned by an alt session, epoch-stamped at its event.
    title: Option<CapturedTitle>,
    /// Sanitized title announced on the primary screen, awaiting the next
    /// alt entry: grok titles the window just before its 1049h, and the
    /// entry event promotes this into the new epoch. Printable output
    /// disclaims it (see the `input` forward); a reset clears it.
    staged_title: Option<String>,
    /// Mirror of the backend's raw (unsanitized) current title, kept only
    /// so the title-stack shadow pushes what the backend pushes.
    raw_title: Option<String>,
    /// Mirror of the backend's title stack. A pop restores through the
    /// backend's own internal `set_title`, which never re-enters the
    /// wrapper, so the pop is replayed against this shadow instead. Same
    /// bound and eviction as the backend (`TITLE_STACK_MAX_DEPTH`, pinned
    /// `=0.26.0`).
    title_stack: Vec<Option<String>>,
}

/// The backend's `TITLE_STACK_MAX_DEPTH` (term/mod.rs, pinned `=0.26.0`):
/// the shadow stack must evict exactly when the backend does or a deep
/// stack would desynchronize pops.
const TITLE_STACK_SHADOW_MAX: usize = 4096;

/// Delegating [`Handler`] that forwards every parser event to the wrapped
/// [`Term`] and observes alt-screen transitions the moment they happen.
///
/// # Missed-forward hazard
///
/// Every `Handler` method has an empty `{}` default, so a missing forward
/// compiles silently and swallows that escape. Two fences hold: the
/// backend is pinned `=0.26.0` in Cargo.toml, and
/// `golden::emulator_wrapper_matches_the_raw_backend_on_every_fixture`
/// replays every corpus fixture through this wrapper and diffs the full
/// screen, cursor, and mode against a raw backend replay — a swallowed
/// method breaks it loudly (the classic goldens alone cannot serve: they
/// drive the raw backend and never touch this path). The forwards below
/// are mechanically generated from the vte 0.15 trait: 71 methods,
/// count-verified against the trait definition.
///
/// # Why this is sound under `?2026`
///
/// The parser buffers a synchronized-update frame and drives the handler
/// only when the frame lands (in `advance` or `stop_sync` — both routed
/// through this wrapper), so these events fire exactly when the grid
/// moves: the observer can never see a transition the grid has not
/// performed, which no byte-scanner could guarantee.
struct ObservedTerm<'a> {
    term: &'a mut Term<ProbeSink>,
    alt: &'a mut AltScreen,
}

impl ObservedTerm<'_> {
    /// Compare the wrapped term's alt bit against the last observed value
    /// after a delegated mode-touching event. Mode-number-agnostic by
    /// design — no 1049/1047/47 literals: the bit compare tracks whatever
    /// modes the backend maps to the alt screen, surviving backend
    /// changes.
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

    /// Title ownership at the event. A title set ON the alt screen labels
    /// the current epoch, where it was spoken. A title set on the primary
    /// screen is STAGED for the next alt entry — grok announces its title
    /// just before its 1049h — and promoted at the entry event. A reset,
    /// or a title that sanitizes to nothing, clears both: `printf
    /// '\x1b]0;\x07'` un-titles the window, it does not freeze a stale
    /// one. Staging is disclaimed by printable output (`input`), never by
    /// control traffic: grok's gap between its title and 1049h is clears
    /// and cursor moves, which must not disclaim, while a shell prompt
    /// always prints glyphs, so a prompt-titling shell cannot leak its
    /// title into the next app. Input-only is the deliberate, minimal
    /// rule.
    fn observe_title(&mut self, title: Option<String>) {
        self.alt.raw_title.clone_from(&title);
        let text = title
            .map(|raw| sanitize_title(&raw))
            .filter(|text| !text.is_empty());
        let Some(text) = text else {
            self.alt.title = None;
            self.alt.staged_title = None;
            return;
        };
        if self.term.mode().contains(TermMode::ALT_SCREEN) {
            self.alt.title = Some(CapturedTitle {
                text,
                alt_epoch: self.alt.epoch,
            });
        } else {
            self.alt.staged_title = Some(text);
        }
    }
}

/// Mechanical forwards. Five carry observations after delegating:
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
    fn set_cursor_style(&mut self, a0: Option<vt::CursorStyle>) {
        self.term.set_cursor_style(a0);
    }
    fn set_cursor_shape(&mut self, a0: vt::CursorShape) {
        self.term.set_cursor_shape(a0);
    }
    fn input(&mut self, a0: char) {
        self.term.input(a0);
        // Printable output disclaims a staged title (rationale on
        // `observe_title`). One branch, predictably not-taken: staging is
        // only ever live between a primary-screen title and the next alt
        // entry.
        if self.alt.staged_title.is_some() {
            self.alt.staged_title = None;
        }
    }
    fn goto(&mut self, a0: i32, a1: usize) {
        self.term.goto(a0, a1);
    }
    fn goto_line(&mut self, a0: i32) {
        self.term.goto_line(a0);
    }
    fn goto_col(&mut self, a0: usize) {
        self.term.goto_col(a0);
    }
    fn insert_blank(&mut self, a0: usize) {
        self.term.insert_blank(a0);
    }
    fn move_up(&mut self, a0: usize) {
        self.term.move_up(a0);
    }
    fn move_down(&mut self, a0: usize) {
        self.term.move_down(a0);
    }
    fn identify_terminal(&mut self, a0: Option<char>) {
        self.term.identify_terminal(a0);
    }
    fn device_status(&mut self, a0: usize) {
        self.term.device_status(a0);
    }
    fn move_forward(&mut self, a0: usize) {
        self.term.move_forward(a0);
    }
    fn move_backward(&mut self, a0: usize) {
        self.term.move_backward(a0);
    }
    fn move_down_and_cr(&mut self, a0: usize) {
        self.term.move_down_and_cr(a0);
    }
    fn move_up_and_cr(&mut self, a0: usize) {
        self.term.move_up_and_cr(a0);
    }
    fn put_tab(&mut self, a0: u16) {
        self.term.put_tab(a0);
    }
    fn backspace(&mut self) {
        self.term.backspace();
    }
    fn carriage_return(&mut self) {
        self.term.carriage_return();
    }
    fn linefeed(&mut self) {
        self.term.linefeed();
    }
    fn bell(&mut self) {
        self.term.bell();
    }
    fn substitute(&mut self) {
        self.term.substitute();
    }
    fn newline(&mut self) {
        self.term.newline();
    }
    fn set_horizontal_tabstop(&mut self) {
        self.term.set_horizontal_tabstop();
    }
    fn scroll_up(&mut self, a0: usize) {
        self.term.scroll_up(a0);
    }
    fn scroll_down(&mut self, a0: usize) {
        self.term.scroll_down(a0);
    }
    fn insert_blank_lines(&mut self, a0: usize) {
        self.term.insert_blank_lines(a0);
    }
    fn delete_lines(&mut self, a0: usize) {
        self.term.delete_lines(a0);
    }
    fn erase_chars(&mut self, a0: usize) {
        self.term.erase_chars(a0);
    }
    fn delete_chars(&mut self, a0: usize) {
        self.term.delete_chars(a0);
    }
    fn move_backward_tabs(&mut self, a0: u16) {
        self.term.move_backward_tabs(a0);
    }
    fn move_forward_tabs(&mut self, a0: u16) {
        self.term.move_forward_tabs(a0);
    }
    fn save_cursor_position(&mut self) {
        self.term.save_cursor_position();
    }
    fn restore_cursor_position(&mut self) {
        self.term.restore_cursor_position();
    }
    fn clear_line(&mut self, a0: vt::LineClearMode) {
        self.term.clear_line(a0);
    }
    fn clear_screen(&mut self, a0: vt::ClearMode) {
        self.term.clear_screen(a0);
    }
    fn clear_tabs(&mut self, a0: vt::TabulationClearMode) {
        self.term.clear_tabs(a0);
    }
    fn set_tabs(&mut self, a0: u16) {
        self.term.set_tabs(a0);
    }
    fn reset_state(&mut self) {
        self.term.reset_state();
        self.observe_alt();
        // RIS clears the backend's title and title stack directly, without
        // a handler event (term/mod.rs `reset_state`): mirror both, and
        // drop any staged announce with the rest of the pre-reset world.
        // The captured title stays — it is epoch-gated and expires at the
        // next entry, matching the pre-observer behavior.
        self.alt.raw_title = None;
        self.alt.title_stack.clear();
        self.alt.staged_title = None;
    }
    fn reverse_index(&mut self) {
        self.term.reverse_index();
    }
    fn terminal_attribute(&mut self, a0: vt::Attr) {
        self.term.terminal_attribute(a0);
    }
    fn set_mode(&mut self, a0: vt::Mode) {
        self.term.set_mode(a0);
    }
    fn unset_mode(&mut self, a0: vt::Mode) {
        self.term.unset_mode(a0);
    }
    fn report_mode(&mut self, a0: vt::Mode) {
        self.term.report_mode(a0);
    }
    fn set_private_mode(&mut self, a0: vt::PrivateMode) {
        self.term.set_private_mode(a0);
        self.observe_alt();
    }
    fn unset_private_mode(&mut self, a0: vt::PrivateMode) {
        self.term.unset_private_mode(a0);
        self.observe_alt();
    }
    fn report_private_mode(&mut self, a0: vt::PrivateMode) {
        self.term.report_private_mode(a0);
    }
    fn set_scrolling_region(&mut self, a0: usize, a1: Option<usize>) {
        self.term.set_scrolling_region(a0, a1);
    }
    fn set_keypad_application_mode(&mut self) {
        self.term.set_keypad_application_mode();
    }
    fn unset_keypad_application_mode(&mut self) {
        self.term.unset_keypad_application_mode();
    }
    fn set_active_charset(&mut self, a0: vt::CharsetIndex) {
        self.term.set_active_charset(a0);
    }
    fn configure_charset(&mut self, a0: vt::CharsetIndex, a1: vt::StandardCharset) {
        self.term.configure_charset(a0, a1);
    }
    fn set_color(&mut self, a0: usize, a1: vt::Rgb) {
        self.term.set_color(a0, a1);
    }
    fn dynamic_color_sequence(&mut self, a0: String, a1: usize, a2: &str) {
        self.term.dynamic_color_sequence(a0, a1, a2);
    }
    fn reset_color(&mut self, a0: usize) {
        self.term.reset_color(a0);
    }
    fn clipboard_store(&mut self, a0: u8, a1: &[u8]) {
        self.term.clipboard_store(a0, a1);
    }
    fn clipboard_load(&mut self, a0: u8, a1: &str) {
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
    fn text_area_size_pixels(&mut self) {
        self.term.text_area_size_pixels();
    }
    fn text_area_size_chars(&mut self) {
        self.term.text_area_size_chars();
    }
    fn set_hyperlink(&mut self, a0: Option<vt::Hyperlink>) {
        self.term.set_hyperlink(a0);
    }
    fn set_mouse_cursor_icon(&mut self, a0: vt::cursor_icon::CursorIcon) {
        self.term.set_mouse_cursor_icon(a0);
    }
    fn report_keyboard_mode(&mut self) {
        self.term.report_keyboard_mode();
    }
    fn push_keyboard_mode(&mut self, a0: vt::KeyboardModes) {
        self.term.push_keyboard_mode(a0);
    }
    fn pop_keyboard_modes(&mut self, a0: u16) {
        self.term.pop_keyboard_modes(a0);
    }
    fn set_keyboard_mode(&mut self, a0: vt::KeyboardModes, a1: vt::KeyboardModesApplyBehavior) {
        self.term.set_keyboard_mode(a0, a1);
    }
    fn set_modify_other_keys(&mut self, a0: vt::ModifyOtherKeys) {
        self.term.set_modify_other_keys(a0);
    }
    fn report_modify_other_keys(&mut self) {
        self.term.report_modify_other_keys();
    }
    fn set_scp(&mut self, a0: vt::ScpCharPath, a1: vt::ScpUpdateMode) {
        self.term.set_scp(a0, a1);
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

    /// Each bidi formatting control is stripped individually — the fixed set
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

    /// A title just before the alt entry stages and is promoted into the
    /// new epoch at the entry event — children emit the title bytes just
    /// before DECSET 1049, and no glyphs intervene to disclaim it.
    #[test]
    fn title_entering_alt_in_one_chunk_is_honored() {
        let mut emu = Emulator::new(4, 20, 0);
        emu.process(b"\x1b]0;app\x07\x1b[?1049h");
        assert_eq!(emu.alt_epoch(), 1);
        assert_eq!(emu.title(), Some("app"));
    }

    /// A primary-screen title followed by printed output is the shell
    /// titling itself: the glyphs are what disclaim it now — a bare title
    /// with only control traffic until the entry stays valid by the
    /// staging rule, deliberately (that is grok's announce shape).
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

    /// The maintainer's counterexample: app A titles itself inside alt
    /// epoch 1, then bounces to app B. Whether the title, the 1049l, and
    /// the 1049h share one read or split before the 1049l, the outcome is
    /// identical — the title event stamped epoch 1 at the event, and the
    /// bounce advanced to 2.
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
    /// (clears and cursor moves), then the alt entry — honored on either
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

    /// The accepted residual, pinned as documented behavior: a title
    /// emitted between two apps — after A's 1049l, before B's 1049h — with
    /// no intervening glyphs stages into B. No fact held here can tell it
    /// from grok's legitimate pre-entry announce (`AltScreen` docs).
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
    /// epoch, and the exit keeps the epoch — so it stays honored.
    #[test]
    fn title_just_before_alt_exit_stays_honored() {
        let mut emu = Emulator::new(4, 20, 0);
        emu.process(b"\x1b[?1049h");
        assert_eq!(emu.alt_epoch(), 1);
        emu.process(b"\x1b]0;done\x07\x1b[?1049l");
        assert!(!emu.alternate_screen());
        assert_eq!(emu.title(), Some("done"));
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

    /// The expired-timeout landing, same premise: sleeping past vte's 150 ms
    /// deadline is a minimum-duration wait, so expiry is guaranteed, not
    /// raced.
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

    /// The alt-exit snapshot captures what the 1049l restore left visible,
    /// at the mode event itself: text after the 1049l — in a later read OR
    /// coalesced into the same one — moves the floor without touching the
    /// snapshot. PTY reads do not preserve write boundaries, so the
    /// coalesced case is routine on a loaded machine, not rare.
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

    /// The maintainer's epoch regression: a leave and re-enter inside one
    /// read must advance the epoch and expire the previous app's title —
    /// read boundaries are not allowed to decide title expiry.
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
