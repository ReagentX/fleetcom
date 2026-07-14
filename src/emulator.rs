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
    vte::ansi::Processor,
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

/// Routes the backend's `Event::PtyWrite` probe responses into a buffer. The
/// listener fires inside `Processor::advance`, while the caller holds the
/// emulator lock, so it only appends, never blocks; the caller drains and
/// filters after `advance` returns. Every other backend event (title,
/// clipboard, color requests, bell) is discarded here: the default-deny probe
/// policy starts with what never gets buffered.
pub struct ProbeSink(Arc<Mutex<Vec<String>>>);

impl EventListener for ProbeSink {
    fn send_event(&self, event: Event) {
        if let Event::PtyWrite(text) = event {
            let mut buf = self
                .0
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
            ProbeSink(Arc::clone(&responses)),
        );
        Self {
            term,
            parser: Processor::new(),
            responses,
            bytes_since_sweep: 0,
        }
    }

    /// Parse raw child output into the grid. Returns the probe replies the
    /// backend generated that pass the allowlist, in generation order; the
    /// caller owns delivering them to the child.
    pub fn process(&mut self, bytes: &[u8]) -> Vec<String> {
        self.parser.advance(&mut self.term, bytes);
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
        self.parser.stop_sync(&mut self.term);
        self.drain_allowed()
    }

    /// The visible screen as ANSI bytes, plus cursor position and whether the
    /// child hid the cursor.
    pub fn formatted(&self) -> (Vec<u8>, (u16, u16), bool) {
        crate::serialize::formatted(&self.term)
    }

    /// Plain-text contents of the visible screen, one line per row.
    pub fn contents(&self) -> String {
        crate::serialize::contents(&self.term)
    }

    /// Return retained plain text from the oldest scrollback row through the
    /// viewport. Soft-wrapped rows join without a separator; trailing padding
    /// is trimmed from other rows. The current scroll offset has no effect.
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
                // Skip wide-character spacers and preserve displayed tabs as
                // spaces.
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
            // A soft-wrapped row continues without trimming or a newline.
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
        self.term.resize(GridSize {
            lines: rows as usize,
            columns: cols as usize,
        });
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

    /// The exit-hint scrape depends on this: a line the child printed past
    /// the grid width soft-wraps, and the wrapped rows must join back into
    /// the one line the child wrote (no synthetic newline through the UUID),
    /// while explicit newlines still separate logical lines.
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
}
