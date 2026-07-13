//! The terminal-emulation seam: every read of a task's screen state and every
//! byte parsed into it goes through [`Emulator`], so the backend can change
//! without touching call-sites.

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

/// One task's terminal emulator: parser plus grid. An enum, not a trait
/// object, because the variant set is closed — two backends during a
/// migration, one at ship. Promote to a trait only if a third materializes.
pub enum Emulator {
    Vt100(vt100::Parser),
}

impl Emulator {
    /// A fresh `rows`×`cols` grid retaining `scrollback` rows of history.
    pub fn new(rows: u16, cols: u16, scrollback: usize) -> Self {
        Self::Vt100(vt100::Parser::new(rows, cols, scrollback))
    }

    /// Parse raw child output into the grid.
    pub fn process(&mut self, bytes: &[u8]) {
        let Self::Vt100(p) = self;
        p.process(bytes);
    }

    /// The visible screen as ANSI bytes, plus cursor position and whether the
    /// child hid the cursor.
    pub fn formatted(&self) -> (Vec<u8>, (u16, u16), bool) {
        let Self::Vt100(p) = self;
        let s = p.screen();
        (s.contents_formatted(), s.cursor_position(), s.hide_cursor())
    }

    /// Plain-text contents of the visible screen, one line per row.
    pub fn contents(&self) -> String {
        let Self::Vt100(p) = self;
        p.screen().contents()
    }

    /// Which mouse events the child asked for; highest requested mode wins.
    pub fn mouse_protocol_mode(&self) -> MouseProtocolMode {
        let Self::Vt100(p) = self;
        match p.screen().mouse_protocol_mode() {
            vt100::MouseProtocolMode::None => MouseProtocolMode::None,
            vt100::MouseProtocolMode::Press => MouseProtocolMode::Press,
            vt100::MouseProtocolMode::PressRelease => MouseProtocolMode::PressRelease,
            vt100::MouseProtocolMode::ButtonMotion => MouseProtocolMode::ButtonMotion,
            vt100::MouseProtocolMode::AnyMotion => MouseProtocolMode::AnyMotion,
        }
    }

    /// How mouse coordinates are encoded on the wire.
    pub fn mouse_protocol_encoding(&self) -> MouseProtocolEncoding {
        let Self::Vt100(p) = self;
        match p.screen().mouse_protocol_encoding() {
            vt100::MouseProtocolEncoding::Default => MouseProtocolEncoding::Default,
            vt100::MouseProtocolEncoding::Utf8 => MouseProtocolEncoding::Utf8,
            vt100::MouseProtocolEncoding::Sgr => MouseProtocolEncoding::Sgr,
        }
    }

    /// Whether the child is on the alternate screen (DECSET 1049).
    pub fn alternate_screen(&self) -> bool {
        let Self::Vt100(p) = self;
        p.screen().alternate_screen()
    }

    /// Whether application cursor keys are on (DECSET 1).
    pub fn application_cursor(&self) -> bool {
        let Self::Vt100(p) = self;
        p.screen().application_cursor()
    }

    /// Whether the child opted into bracketed paste (DECSET 2004).
    pub fn bracketed_paste(&self) -> bool {
        let Self::Vt100(p) = self;
        p.screen().bracketed_paste()
    }

    /// Rows the viewport is scrolled back from live output.
    pub fn scrollback(&self) -> usize {
        let Self::Vt100(p) = self;
        p.screen().scrollback()
    }

    /// Move the viewport `rows` back from live output; the backend clamps to
    /// retained history, so `usize::MAX` means the oldest stored row.
    pub fn set_scrollback(&mut self, rows: usize) {
        let Self::Vt100(p) = self;
        p.screen_mut().set_scrollback(rows);
    }

    /// Resize the grid to `rows`×`cols`.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let Self::Vt100(p) = self;
        p.screen_mut().set_size(rows, cols);
    }

    /// Grid size as `(rows, cols)`.
    #[cfg(test)]
    pub fn size(&self) -> (u16, u16) {
        let Self::Vt100(p) = self;
        p.screen().size()
    }
}
