//! The terminal-emulation seam: every read of a task's screen state and every
//! byte parsed into it goes through [`Emulator`], so the backend can change
//! without touching call-sites. The production backend is
//! `alacritty_terminal`; a `#[cfg(test)]` vt100 variant survives as the
//! reference side of the differential golden suites (a dev-dependency —
//! release builds compile it out entirely).

use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

use alacritty_terminal::{
    Term,
    event::{Event, EventListener},
    grid::{Dimensions, Scroll},
    term::{Config, TermMode},
    vte::ansi::Processor,
};

/// Mouse event classes the child requested (DECSET 9/1000/1002/1003).
/// Backend-neutral so callers route input without naming a backend type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MouseProtocolMode {
    None,
    Press,
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
/// emulator lock — so it only appends, never blocks; the caller drains and
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

/// `Dimensions` carrier for `Term::new`/`Term::resize`. alacritty's own
/// concrete impl (`term::test::TermSize`) lives in its test module, which
/// production code shouldn't reach into.
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

/// Default-deny allowlist over backend-generated probe responses
/// (EMULATOR_MIGRATION.md, probe policy). The advertised contract is exactly
/// three shapes: CPR (`ESC[<row>;<col>R`), the DSR-5 ok reply (`ESC[0n`), and
/// the primary DA response (`ESC[?<params>c`). Everything else the backend
/// can emit — secondary DA `ESC[>…c`, kitty keyboard reports `ESC[?…u`,
/// DECRPM `…$y`, window-size `ESC[8;…t`, and whatever a future pin adds — is
/// dropped, so a backend bump cannot silently widen what fleetcom advertises
/// to children.
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

/// The alacritty backend: grid plus parser, advanced together so one lock
/// covers both, plus the probe-response buffer shared with `term`'s listener.
pub struct AlacrittyBackend {
    term: Term<ProbeSink>,
    parser: Processor,
    responses: Arc<Mutex<Vec<String>>>,
}

impl AlacrittyBackend {
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
}

/// One task's terminal emulator: parser plus grid. An enum, not a trait
/// object, because the variant set is closed — two backends during a
/// migration, one at ship. Promote to a trait only if a third materializes.
pub enum Emulator {
    /// The differential-harness reference backend: the golden and
    /// mouse-contract tests construct it, production cannot — vt100 is a
    /// dev-dependency, so the variant only compiles under `cfg(test)`.
    /// Both variants are boxed: each backend's inline state runs to
    /// kilobytes, and every task holds exactly one emulator behind an `Arc`,
    /// so the indirection costs nothing that matters.
    #[cfg(test)]
    Vt100(Box<vt100::Parser>),
    Alacritty(Box<AlacrittyBackend>),
}

impl Emulator {
    /// A fresh `rows`×`cols` grid retaining `scrollback` rows of history.
    pub fn new(rows: u16, cols: u16, scrollback: usize) -> Self {
        let responses = Arc::new(Mutex::new(Vec::new()));
        let config = Config {
            // The plan's fixed history depth, not alacritty's 10k default:
            // per-task memory stays bounded at the depth the golden retention
            // measurements assume.
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
        Self::Alacritty(Box::new(AlacrittyBackend {
            term,
            parser: Processor::new(),
            responses,
        }))
    }

    /// A fresh vt100-backed emulator, for tests pinning cross-backend
    /// behavior against the differential reference.
    #[cfg(test)]
    pub fn new_vt100(rows: u16, cols: u16, scrollback: usize) -> Self {
        Self::Vt100(Box::new(vt100::Parser::new(rows, cols, scrollback)))
    }

    /// Parse raw child output into the grid. Returns the probe replies the
    /// backend generated that pass the allowlist, in generation order; the
    /// caller owns delivering them to the child.
    pub fn process(&mut self, bytes: &[u8]) -> Vec<String> {
        match self {
            #[cfg(test)]
            Self::Vt100(p) => {
                p.process(bytes);
                // vt100 has no response machinery at all.
                Vec::new()
            }
            Self::Alacritty(b) => {
                b.parser.advance(&mut b.term, bytes);
                b.drain_allowed()
            }
        }
    }

    /// Terminate a `?2026` synchronized update whose timeout has expired,
    /// flushing the buffered frame into the grid; returns any allowlisted
    /// probe replies the flushed bytes generated. vte re-checks its timeout
    /// only when more bytes arrive, so a child that opens BSU and stalls
    /// would freeze its view until then — the core's periodic tick calls this
    /// to bound the stall. No-op while the timeout is still pending (an
    /// in-flight frame is not torn) and when no sync is open.
    pub fn flush_expired_sync(&mut self) -> Vec<String> {
        match self {
            // vt100 never buffers: there is nothing to flush.
            #[cfg(test)]
            Self::Vt100(_) => Vec::new(),
            Self::Alacritty(b) => {
                let expired = b
                    .parser
                    .sync_timeout()
                    .sync_timeout()
                    .is_some_and(|deadline| deadline <= Instant::now());
                if !expired {
                    return Vec::new();
                }
                b.parser.stop_sync(&mut b.term);
                b.drain_allowed()
            }
        }
    }

    /// The visible screen as ANSI bytes, plus cursor position and whether the
    /// child hid the cursor.
    pub fn formatted(&self) -> (Vec<u8>, (u16, u16), bool) {
        match self {
            #[cfg(test)]
            Self::Vt100(p) => {
                let s = p.screen();
                (s.contents_formatted(), s.cursor_position(), s.hide_cursor())
            }
            Self::Alacritty(b) => crate::serialize::formatted(&b.term),
        }
    }

    /// Plain-text contents of the visible screen, one line per row.
    pub fn contents(&self) -> String {
        match self {
            #[cfg(test)]
            Self::Vt100(p) => p.screen().contents(),
            Self::Alacritty(b) => crate::serialize::contents(&b.term),
        }
    }

    /// Which mouse events the child asked for; the most recent DECSET wins
    /// (both backends keep the modes mutually exclusive). The alacritty
    /// backend does not model DECSET 9 (vte's `NamedPrivateMode` has no
    /// mode 9), so it never reports `Press`: an X10-only child gets no mouse
    /// reports at all, matching alacritty the terminal.
    pub fn mouse_protocol_mode(&self) -> MouseProtocolMode {
        match self {
            #[cfg(test)]
            Self::Vt100(p) => match p.screen().mouse_protocol_mode() {
                vt100::MouseProtocolMode::None => MouseProtocolMode::None,
                vt100::MouseProtocolMode::Press => MouseProtocolMode::Press,
                vt100::MouseProtocolMode::PressRelease => MouseProtocolMode::PressRelease,
                vt100::MouseProtocolMode::ButtonMotion => MouseProtocolMode::ButtonMotion,
                vt100::MouseProtocolMode::AnyMotion => MouseProtocolMode::AnyMotion,
            },
            Self::Alacritty(b) => {
                let mode = b.term.mode();
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
        }
    }

    /// How mouse coordinates are encoded on the wire. SGR wins over UTF-8
    /// when both bits are somehow set; the backend keeps them exclusive
    /// (each DECSET clears the other), so the order is belt-and-braces.
    pub fn mouse_protocol_encoding(&self) -> MouseProtocolEncoding {
        match self {
            #[cfg(test)]
            Self::Vt100(p) => match p.screen().mouse_protocol_encoding() {
                vt100::MouseProtocolEncoding::Default => MouseProtocolEncoding::Default,
                vt100::MouseProtocolEncoding::Utf8 => MouseProtocolEncoding::Utf8,
                vt100::MouseProtocolEncoding::Sgr => MouseProtocolEncoding::Sgr,
            },
            Self::Alacritty(b) => {
                let mode = b.term.mode();
                if mode.contains(TermMode::SGR_MOUSE) {
                    MouseProtocolEncoding::Sgr
                } else if mode.contains(TermMode::UTF8_MOUSE) {
                    MouseProtocolEncoding::Utf8
                } else {
                    MouseProtocolEncoding::Default
                }
            }
        }
    }

    /// Whether the child is on the alternate screen (DECSET 1049).
    pub fn alternate_screen(&self) -> bool {
        match self {
            #[cfg(test)]
            Self::Vt100(p) => p.screen().alternate_screen(),
            Self::Alacritty(b) => b.term.mode().contains(TermMode::ALT_SCREEN),
        }
    }

    /// Whether wheel events should reach the child as arrow keys: on the
    /// alternate screen with DECSET 1007 in effect. The gate mirrors
    /// alacritty the terminal's own arrow-emission check —
    /// `mode().contains(ALT_SCREEN | ALTERNATE_SCROLL)` in its
    /// `scroll_terminal` — and 1007 defaults *on* (xterm semantics), so a
    /// full-screen child scrolls without opting in but keeps `?1007l` as its
    /// veto. vt100 cannot model 1007; its arm keeps the pre-step-6 heuristic
    /// (alt screen alone) so the cross-backend tests retain their meaning.
    pub fn alternate_scroll(&self) -> bool {
        match self {
            #[cfg(test)]
            Self::Vt100(p) => p.screen().alternate_screen(),
            Self::Alacritty(b) => b
                .term
                .mode()
                .contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL),
        }
    }

    /// Whether application cursor keys are on (DECSET 1).
    pub fn application_cursor(&self) -> bool {
        match self {
            #[cfg(test)]
            Self::Vt100(p) => p.screen().application_cursor(),
            Self::Alacritty(b) => b.term.mode().contains(TermMode::APP_CURSOR),
        }
    }

    /// Whether the child opted into bracketed paste (DECSET 2004).
    pub fn bracketed_paste(&self) -> bool {
        match self {
            #[cfg(test)]
            Self::Vt100(p) => p.screen().bracketed_paste(),
            Self::Alacritty(b) => b.term.mode().contains(TermMode::BRACKETED_PASTE),
        }
    }

    /// Rows the viewport is scrolled back from live output.
    pub fn scrollback(&self) -> usize {
        match self {
            #[cfg(test)]
            Self::Vt100(p) => p.screen().scrollback(),
            Self::Alacritty(b) => b.term.grid().display_offset(),
        }
    }

    /// Move the viewport `rows` back from live output; the backend clamps to
    /// retained history, so `usize::MAX` means the oldest stored row.
    pub fn set_scrollback(&mut self, rows: usize) {
        match self {
            #[cfg(test)]
            Self::Vt100(p) => p.screen_mut().set_scrollback(rows),
            Self::Alacritty(b) => {
                // Same clamp contract as vt100: absolute target, capped at
                // retained history. The grid API is relative; both offsets
                // are bounded by the history cap, so the delta fits i32.
                let grid = b.term.grid();
                let target = rows.min(grid.history_size());
                let delta = target as i32 - grid.display_offset() as i32;
                b.term.scroll_display(Scroll::Delta(delta));
            }
        }
    }

    /// Resize the grid to `rows`×`cols`.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        match self {
            #[cfg(test)]
            Self::Vt100(p) => p.screen_mut().set_size(rows, cols),
            Self::Alacritty(b) => b.term.resize(GridSize {
                lines: rows as usize,
                columns: cols as usize,
            }),
        }
    }

    /// Grid size as `(rows, cols)`.
    #[cfg(test)]
    pub fn size(&self) -> (u16, u16) {
        match self {
            Self::Vt100(p) => p.screen().size(),
            Self::Alacritty(b) => (
                b.term.grid().screen_lines() as u16,
                b.term.grid().columns() as u16,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// The filter's exact contract, shape by shape. The denied cases name
    /// every response class the backend can emit at this pin plus arbitrary
    /// unknown strings, so a pin-bump that grows the response surface cannot
    /// silently widen the advertised contract.
    #[test]
    fn probe_allowlist_forwards_only_the_advertised_shapes() {
        // Allowed: CPR, DSR-5 ok, primary DA.
        assert!(allowed_probe_response("\x1b[1;1R"));
        assert!(allowed_probe_response("\x1b[24;80R"));
        assert!(allowed_probe_response("\x1b[0n"));
        assert!(allowed_probe_response("\x1b[?6c"));
        assert!(allowed_probe_response("\x1b[?62;22c"));

        // Denied: named response classes alacritty emits today.
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

        // Denied: arbitrary unknown responses (the pin-bump guard).
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
        // allowlist would drop the `ESC[?…u` shape regardless.
        assert!(emu.process(b"\x1b[?u").is_empty());
    }

    /// The vt100 backend answers nothing: it has no response machinery.
    #[test]
    fn vt100_backend_answers_no_probes() {
        let mut emu = Emulator::new_vt100(24, 80, 0);
        assert!(emu.process(b"\x1b[6n\x1b[5n\x1b[c").is_empty());
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

    /// vt100→TermMode mouse-mode mapping: most recent DECSET wins, unset
    /// returns to none, and 1005/1006 stay mutually exclusive.
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

    /// The step-4 modeling gap, pinned: DECSET 9 (X10 press-only) is ignored
    /// by alacritty/vte — no `NamedPrivateMode` for mode 9 — where vt100
    /// reported `Press`. An X10-only child gets no mouse reports after the
    /// swap, matching alacritty the terminal.
    #[test]
    fn x10_decset9_unmodeled_by_alacritty() {
        let mut emu = Emulator::new(24, 80, 0);
        emu.process(b"\x1b[?9h");
        assert_eq!(emu.mouse_protocol_mode(), MouseProtocolMode::None);

        let mut vt = Emulator::new_vt100(24, 80, 0);
        vt.process(b"\x1b[?9h");
        assert_eq!(vt.mouse_protocol_mode(), MouseProtocolMode::Press);
    }

    /// The wheel-as-arrows gate is alt screen *and* DECSET 1007, with 1007
    /// defaulting on — and the child's `?1007l` veto is honored, which the
    /// old alt-screen heuristic could not do. The vt100 arm keeps that
    /// heuristic (no 1007 state to read), pinned here so the divergence is
    /// explicit rather than a silent cross-backend drift.
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

        let mut vt = Emulator::new_vt100(24, 80, 0);
        vt.process(b"\x1b[?1049h\x1b[?1007l");
        assert!(vt.alternate_scroll(), "vt100 has no 1007 state; heuristic");
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

    /// History gathered through a top-anchored scroll region must survive
    /// resize, and insertion at the new geometry must keep accumulating.
    /// Multiplexers with homegrown grids historically lose exactly this
    /// (zellij drops region-scrolled history after a pane resize until the
    /// original size returns); alacritty reflows history through resize in
    /// both directions, and this pins that our seam preserves that.
    #[test]
    fn region_scrolled_history_survives_resize() {
        let mut emu = Emulator::new(40, 120, 2000);
        for i in 1..=20 {
            emu.process(format!("\x1b[{i};1Hseed {i:02}").as_bytes());
        }
        // Codex-style insertion: top-anchored region, newlines at its bottom.
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
}
