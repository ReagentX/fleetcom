//! Serialize an `alacritty_terminal` grid into ANSI bytes for attached clients.
//!
//! Contract: replaying [`formatted`]'s bytes into a fresh terminal of the same
//! dimensions reproduces the source's *displayed* screen (every cell's
//! character, zero-width extras, colors, and style flags) plus the cursor's
//! position, visibility, and pending-wrap (phantom column) state. The
//! round-trip oracle in this module's tests enforces exactly that over the
//! recorded PTY corpus and targeted synthetic cases.
//!
//! # Soft wrap: CUP-per-row
//!
//! Each viewport row is emitted under an absolute cursor address (CUP), never
//! by letting output wrap at the right edge. Replay therefore cannot set
//! `WRAPLINE` or `LEADING_WIDE_CHAR_SPACER`: wrap bookkeeping the grid keeps
//! for reflow and selection, invisible on screen. Consequence: a client
//! copying a soft-wrapped logical line out of a replayed view gets hard
//! newlines at row boundaries. The round-trip comparison excludes exactly
//! those two flags.
//!
//! # Serialization limits and normalizations
//!
//! - Hyperlinks (OSC 8): modeled by the backend but not re-emitted or compared.
//! - Blink (SGR 5/6): alacritty stores no blink flag, so there is nothing to
//!   serialize; both source and replay drop it identically.
//! - Orphaned wide-char halves: ECH/DCH/ICH can strip a wide glyph's partner
//!   cell without repairing it. No byte stream recreates a lone half: writing
//!   the glyph would fabricate a spacer over the neighbor, or wrap at the last
//!   column, so orphans are emitted as blanks carrying the cell's attributes.
//! - A `'\t'` cell under a pending-wrap cursor: `put_tab` never sets pending
//!   wrap, so preserve cursor state and rewrite the cell as a styled
//!   blank.
//! - The source's pending SGR template: output ends with SGR 0 so the replay
//!   target is left in a known attribute state.

use std::fmt::Write as _;

use alacritty_terminal::{
    Term,
    grid::{Dimensions, Row},
    index::{Column, Line},
    term::{
        TermMode,
        cell::{Cell, Flags},
    },
    vte::ansi::{Color, NamedColor},
};

/// Style bits the serializer re-emits as SGR parameters. Wrap bookkeeping
/// (`WRAPLINE`, `LEADING_WIDE_CHAR_SPACER`) is unreproducible under
/// CUP-per-row; wide-char structure (`WIDE_CHAR`, `WIDE_CHAR_SPACER`) is
/// recreated by writing the wide glyph itself, not by SGR.
const STYLE_FLAGS: Flags = Flags::BOLD
    .union(Flags::DIM)
    .union(Flags::ITALIC)
    .union(Flags::INVERSE)
    .union(Flags::HIDDEN)
    .union(Flags::STRIKEOUT)
    .union(Flags::ALL_UNDERLINES);

/// SGR state carried across cells and rows. Preserve one running state across CUP. On
/// attribute change, specify all attributes from SGR 0 to restore defaults and reset
/// boundaries without tracking per-attribute deltas.
#[derive(PartialEq)]
struct Sgr {
    fg: Color,
    bg: Color,
    flags: Flags,
    underline: Option<Color>,
}

impl Sgr {
    /// The state SGR 0 establishes.
    fn reset() -> Self {
        Self {
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            flags: Flags::empty(),
            underline: None,
        }
    }

    fn of(cell: &Cell) -> Self {
        Self {
            fg: cell.fg,
            bg: cell.bg,
            flags: cell.flags.intersection(STYLE_FLAGS),
            underline: cell.underline_color(),
        }
    }
}

/// The visible screen as ANSI bytes, plus the live cursor position (row, col;
/// zero-based viewport coordinates) and whether the child hid the cursor.
/// Matches the tuple shape of `Emulator::formatted`.
///
/// Honors the display offset: a scrolled-back viewport serializes what is
/// displayed, read out of scrollback. The cursor tuple always reports the
/// live cursor regardless of offset; pending-wrap state is only recreated at
/// offset zero, because recreating it rewrites the cell under the cursor and
/// that cell is not part of a scrolled view.
pub fn formatted<T>(term: &Term<T>) -> (Vec<u8>, (u16, u16), bool) {
    let grid = term.grid();
    let cols = grid.columns();
    let offset = grid.display_offset() as i32;

    let mut buf = String::new();
    // Establish a known attribute state: production replay targets carry
    // whatever SGR the previous frame left behind.
    buf.push_str("\x1b[0m");
    let mut state = Sgr::reset();

    for row in 0..grid.screen_lines() {
        let line = &grid[Line(row as i32 - offset)];
        // Absolute address per row (CUP-per-row, see module docs). Also clears
        // any pending wrap left by writing the previous row's last column, so
        // replay never soft-wraps.
        cup(&mut buf, row, 0);
        let mut col = 0;
        while col < cols {
            let cell = &line[Column(col)];
            if paired_spacer(line, col) {
                col += 1;
                continue;
            }
            sync_sgr(&mut buf, &mut state, cell);
            if paired_wide(line, col, cols) {
                push_char(&mut buf, cell);
                // The spacer is written implicitly with identical attributes when
                // writing the glyph.
                col += 2;
                continue;
            }
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR | Flags::WIDE_CHAR_SPACER)
            {
                // Orphaned wide half (see module docs): styled blank, keeping
                // the attributes but not the unrepresentable structure.
                buf.push(' ');
                col += 1;
                continue;
            }
            if cell.c == '\t' {
                // `put_tab` stores a literal '\t' but only sets `c` on a cell
                // that still reads ' ', without applying the SGR template, and
                // it jumps the cursor to the next tab stop. So: write a styled
                // space to plant the attributes, step back, emit the tab to
                // flip `c`, then re-address past the cell before the tab stop
                // jump can misplace the next write.
                buf.push(' ');
                cup(&mut buf, row, col);
                buf.push('\t');
                cup(&mut buf, row, col + 1);
                // Zero-width marks attach one column behind the cursor: the
                // re-address above puts that exactly on the tab cell.
                push_zerowidth(&mut buf, cell);
                col += 1;
                continue;
            }
            push_char(&mut buf, cell);
            col += 1;
        }
    }

    let cursor = &grid.cursor;
    let point = cursor.point;
    if offset == 0 && cursor.input_needs_wrap {
        // Pending wrap (phantom column): only a write into the last column
        // sets it and any CUP clears it, so it must be recreated by rewriting
        // the cell under the cursor as the final write. The cursor always sits
        // in the last column when this flag is set.
        let last = cols - 1;
        let line = &grid[point.line];
        // If the last column holds a paired spacer the write that set the
        // flag was the wide glyph one cell left; rewrite that instead.
        let (base_col, base) = if paired_spacer(line, last) {
            (last - 1, &line[Column(last - 1)])
        } else {
            (last, &line[Column(last)])
        };
        cup(&mut buf, point.line.0.max(0) as usize, base_col);
        sync_sgr(&mut buf, &mut state, base);
        // A '\t' goes through `put_tab` (which never sets pending wrap) and a
        // lone wide half would wrap; both are unrepresentable under a pending
        // wrap and normalize to the styled blank the main pass emitted.
        let unwritable =
            base.c == '\t' || (base_col == last && base.flags.contains(Flags::WIDE_CHAR));
        if unwritable {
            buf.push(' ');
        } else {
            buf.push(base.c);
        }
        // Under pending wrap, keep the attach column unchanged to place marks on this
        // cell.
        push_zerowidth(&mut buf, base);
    } else {
        cup(&mut buf, point.line.0.max(0) as usize, point.column.0);
    }

    // Leave the replay target in a known attribute state; the source's
    // pending SGR template is outside the serialization contract.
    buf.push_str("\x1b[0m");
    buf.push_str(if term.mode().contains(TermMode::SHOW_CURSOR) {
        "\x1b[?25h"
    } else {
        "\x1b[?25l"
    });

    (
        buf.into_bytes(),
        (point.line.0.max(0) as u16, point.column.0 as u16),
        !term.mode().contains(TermMode::SHOW_CURSOR),
    )
}

/// Plain-text contents of the displayed screen, one line per row, honoring the display
/// offset. Paired wide-char spacers are skipped so wide glyphs appear once; zero-width
/// marks are included with their base character; `'\t'` cells, concealed (SGR 8) cells,
/// and orphaned wide halves read as the blank the replayed screen shows; trailing
/// spaces are trimmed per row. This display policy differs from
/// [`crate::emulator::Emulator::live_rows`], which preserves the stored glyphs for
/// structural matching.
pub fn contents<T>(term: &Term<T>) -> String {
    let grid = term.grid();
    let cols = grid.columns();
    let offset = grid.display_offset() as i32;

    let mut out = String::new();
    for row in 0..grid.screen_lines() {
        if row > 0 {
            out.push('\n');
        }
        let row_start = out.len();
        let line = &grid[Line(row as i32 - offset)];
        for col in 0..cols {
            let cell = &line[Column(col)];
            // Each concealed cell contributes one blank display column;
            // attached zero-width marks remain concealed as well.
            if cell.flags.contains(Flags::HIDDEN) {
                out.push(' ');
                continue;
            }
            if paired_spacer(line, col) {
                continue;
            }
            // Same normalizations as `formatted` so both views agree.
            if cell.c == '\t'
                || (!paired_wide(line, col, cols)
                    && cell
                        .flags
                        .intersects(Flags::WIDE_CHAR | Flags::WIDE_CHAR_SPACER))
            {
                out.push(' ');
            } else {
                out.push(cell.c);
            }
            push_zerowidth(&mut out, cell);
        }
        while out.len() > row_start && out.ends_with(' ') {
            out.pop();
        }
    }
    out
}

/// Absolute cursor address from zero-based coordinates.
fn cup(buf: &mut String, row: usize, col: usize) {
    let _ = write!(buf, "\x1b[{};{}H", row + 1, col + 1);
}

/// Whether `line[col]` is a spacer paired with a wide glyph in the cell to
/// its left. Such a spacer is recreated implicitly by writing the glyph;
/// emitting it too would double-write.
fn paired_spacer(line: &Row<Cell>, col: usize) -> bool {
    line[Column(col)].flags.contains(Flags::WIDE_CHAR_SPACER)
        && col > 0
        && line[Column(col - 1)].flags.contains(Flags::WIDE_CHAR)
}

/// Whether `line[col]` is a wide glyph paired with its spacer in the cell to
/// its right; writing the glyph recreates both cells with identical
/// attributes. An unpaired half is an orphan (see the module docs).
fn paired_wide(line: &Row<Cell>, col: usize, cols: usize) -> bool {
    line[Column(col)].flags.contains(Flags::WIDE_CHAR)
        && col + 1 < cols
        && line[Column(col + 1)]
            .flags
            .contains(Flags::WIDE_CHAR_SPACER)
}

/// Base character plus any zero-width marks stored in the cell's extras.
/// Marks must follow the base immediately: they attach one column behind the
/// cursor (or in place under pending wrap), which is exactly where the base
/// write leaves it.
fn push_char(buf: &mut String, cell: &Cell) {
    buf.push(cell.c);
    push_zerowidth(buf, cell);
}

fn push_zerowidth(buf: &mut String, cell: &Cell) {
    if let Some(zerowidth) = cell.zerowidth() {
        buf.extend(zerowidth.iter());
    }
}

/// Emit SGR only when the cell's attributes differ from the running state.
/// The respec always starts from 0: omitted parameters are thereby the
/// defaults, which is what makes default-color restoration and reset
/// boundaries fall out without per-attribute cancel codes.
fn sync_sgr(buf: &mut String, state: &mut Sgr, cell: &Cell) {
    let want = Sgr::of(cell);
    if *state == want {
        return;
    }
    buf.push_str("\x1b[0");
    if want.flags.contains(Flags::BOLD) {
        buf.push_str(";1");
    }
    if want.flags.contains(Flags::DIM) {
        buf.push_str(";2");
    }
    if want.flags.contains(Flags::ITALIC) {
        buf.push_str(";3");
    }
    // The parser clears ALL_UNDERLINES before inserting one, so a parsed cell
    // holds at most one underline kind; the chain picks it.
    if want.flags.contains(Flags::UNDERLINE) {
        buf.push_str(";4");
    } else if want.flags.contains(Flags::DOUBLE_UNDERLINE) {
        buf.push_str(";4:2");
    } else if want.flags.contains(Flags::UNDERCURL) {
        buf.push_str(";4:3");
    } else if want.flags.contains(Flags::DOTTED_UNDERLINE) {
        buf.push_str(";4:4");
    } else if want.flags.contains(Flags::DASHED_UNDERLINE) {
        buf.push_str(";4:5");
    }
    if want.flags.contains(Flags::INVERSE) {
        buf.push_str(";7");
    }
    if want.flags.contains(Flags::HIDDEN) {
        buf.push_str(";8");
    }
    if want.flags.contains(Flags::STRIKEOUT) {
        buf.push_str(";9");
    }
    push_color(buf, want.fg, 30);
    push_color(buf, want.bg, 40);
    if let Some(color) = want.underline {
        push_underline_color(buf, color);
    }
    buf.push('m');
    *state = want;
}

/// Foreground and background SGR parameters share one shape at different
/// bases (30/40): named colors at `base+i`, bright at `base+60`, and the
/// indexed (`;5;`) and RGB (`;2;`) forms introduced by `base+8` (38/48).
fn push_color(buf: &mut String, color: Color, base: u16) {
    let base = base as usize;
    match color {
        Color::Named(named) => match named as usize {
            // Black..=White and BrightBlack..=BrightWhite carry their ANSI
            // index as the enum discriminant.
            i @ 0..=7 => {
                let _ = write!(buf, ";{}", base + i);
            }
            i @ 8..=15 => {
                let _ = write!(buf, ";{}", base + 60 + i - 8);
            }
            // DimBlack..=DimWhite are renderer-side names the parser never
            // stores in cells; mapped to their base color defensively.
            i @ 259..=266 => {
                let _ = write!(buf, ";{}", base + i - 259);
            }
            // Foreground (and the other special names): SGR 0 already
            // restored the default.
            _ => {}
        },
        Color::Indexed(i) => {
            let _ = write!(buf, ";{};5;{i}", base + 8);
        }
        Color::Spec(rgb) => {
            let _ = write!(buf, ";{};2;{};{};{}", base + 8, rgb.r, rgb.g, rgb.b);
        }
    }
}

fn push_underline_color(buf: &mut String, color: Color) {
    match color {
        Color::Indexed(i) => {
            let _ = write!(buf, ";58;5;{i}");
        }
        Color::Spec(rgb) => {
            let _ = write!(buf, ";58;2;{};{};{}", rgb.r, rgb.g, rgb.b);
        }
        // SGR 58 only parses indexed and RGB forms, so a named underline
        // color cannot occur in a parsed grid.
        Color::Named(_) => {}
    }
}

/// Round-trip tests parse bytes, serialize the resulting grid, replay the
/// serialization, and compare every displayed cell and the cursor. Comparison
/// exclusions are documented by [`tests::assert_same_screen`].
#[cfg(test)]
mod tests {
    use alacritty_terminal::{
        event::VoidListener,
        grid::Scroll,
        term::{Config, test::TermSize},
        vte::ansi::Processor,
    };

    use super::*;
    use crate::testutil::{CORPUS_COLS, CORPUS_LINES};

    /// Flags replay can never set: CUP-per-row emission performs no soft
    /// wraps, so wrap bookkeeping (soft-wrap marker and the spacer left when a
    /// wide glyph spills to the next row) is excluded from comparison. See the
    /// module docs for the copy/paste consequence.
    const WRAP_ARTIFACTS: Flags = Flags::WRAPLINE.union(Flags::LEADING_WIDE_CHAR_SPACER);

    fn new_term(lines: usize, cols: usize) -> Term<VoidListener> {
        Term::new(Config::default(), &TermSize::new(cols, lines), VoidListener)
    }

    fn parse(bytes: &[u8], lines: usize, cols: usize) -> Term<VoidListener> {
        let mut term = new_term(lines, cols);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, bytes);
        term
    }

    /// Compare every displayed cell of `source` (offset-adjusted) against the
    /// replayed term's screen: character, zero-width extras, fg, bg,
    /// underline color, and flags.
    ///
    /// Exclusions, each mirroring an emission normalization documented in the
    /// module docs:
    /// - `WRAP_ARTIFACTS` bits are masked everywhere (CUP-per-row).
    /// - An orphaned wide half keeps its attributes but its character and
    ///   `WIDE_CHAR`/`WIDE_CHAR_SPACER` bit are expected as a blank: no byte
    ///   stream recreates a lone half.
    /// - A `'\t'` cell under a pending-wrap cursor is expected as a blank:
    ///   `put_tab` cannot set pending wrap, and cursor state is preserved.
    /// - Hyperlink extras are not compared because they are not serialized.
    fn assert_same_screen(source: &Term<VoidListener>, replay: &Term<VoidListener>, case: &str) {
        let sgrid = source.grid();
        let rgrid = replay.grid();
        assert_eq!(
            sgrid.screen_lines(),
            rgrid.screen_lines(),
            "{case}: line count"
        );
        assert_eq!(sgrid.columns(), rgrid.columns(), "{case}: column count");

        let cols = sgrid.columns();
        let offset = sgrid.display_offset() as i32;
        let cursor = &sgrid.cursor;
        for row in 0..sgrid.screen_lines() {
            let sline = &sgrid[Line(row as i32 - offset)];
            let rline = &rgrid[Line(row as i32)];
            for col in 0..cols {
                let s = &sline[Column(col)];
                let r = &rline[Column(col)];

                let paired_spacer = paired_spacer(sline, col);
                let paired_wide = paired_wide(sline, col, cols);
                let orphan_wide = s.flags.contains(Flags::WIDE_CHAR) && !paired_wide;
                let orphan_spacer = s.flags.contains(Flags::WIDE_CHAR_SPACER) && !paired_spacer;
                let phantom_tab = offset == 0
                    && cursor.input_needs_wrap
                    && cursor.point.line == Line(row as i32)
                    && col == cols - 1
                    && s.c == '\t';

                let expected_c = if orphan_wide || orphan_spacer || phantom_tab {
                    ' '
                } else {
                    s.c
                };
                let mut mask = WRAP_ARTIFACTS.complement();
                if orphan_wide {
                    mask.remove(Flags::WIDE_CHAR);
                }
                if orphan_spacer {
                    mask.remove(Flags::WIDE_CHAR_SPACER);
                }
                let expected_flags = s.flags.intersection(mask);
                let expected_zerowidth: &[char] = if orphan_wide {
                    &[]
                } else {
                    s.zerowidth().unwrap_or(&[])
                };

                assert_eq!(r.c, expected_c, "{case}: char at ({row},{col})");
                assert_eq!(r.fg, s.fg, "{case}: fg at ({row},{col})");
                assert_eq!(r.bg, s.bg, "{case}: bg at ({row},{col})");
                assert_eq!(r.flags, expected_flags, "{case}: flags at ({row},{col})");
                assert_eq!(
                    r.zerowidth().unwrap_or(&[]),
                    expected_zerowidth,
                    "{case}: zerowidth at ({row},{col})"
                );
                assert_eq!(
                    r.underline_color(),
                    s.underline_color(),
                    "{case}: underline color at ({row},{col})"
                );
            }
        }
    }

    /// Serialize `source`, replay into a fresh term, run the full comparison,
    /// and return the replayed term for case-specific follow-up asserts.
    ///
    /// Cursor pending-wrap state is only asserted at display offset zero: a
    /// scrolled view does not contain the cell whose rewrite would recreate
    /// it (see [`formatted`]).
    fn round_trip(source: &Term<VoidListener>, case: &str) -> Term<VoidListener> {
        let (bytes, pos, hidden) = formatted(source);
        let mut replay = new_term(source.grid().screen_lines(), source.grid().columns());
        let mut parser: Processor = Processor::new();
        parser.advance(&mut replay, &bytes);

        assert_same_screen(source, &replay, case);

        let scursor = &source.grid().cursor;
        let rcursor = &replay.grid().cursor;
        assert_eq!(
            pos,
            (
                scursor.point.line.0.max(0) as u16,
                scursor.point.column.0 as u16
            ),
            "{case}: returned cursor tuple"
        );
        assert_eq!(
            hidden,
            !source.mode().contains(TermMode::SHOW_CURSOR),
            "{case}: returned visibility"
        );
        assert_eq!(
            replay.mode().contains(TermMode::SHOW_CURSOR),
            source.mode().contains(TermMode::SHOW_CURSOR),
            "{case}: replayed cursor visibility"
        );
        assert_eq!(
            rcursor.point, scursor.point,
            "{case}: replayed cursor position"
        );
        if source.grid().display_offset() == 0 {
            assert_eq!(
                rcursor.input_needs_wrap, scursor.input_needs_wrap,
                "{case}: replayed pending-wrap state"
            );
        }

        // The plain-text views must agree too; both apply the same
        // normalizations, so this holds whenever the cell comparison does.
        assert_eq!(contents(source), contents(&replay), "{case}: contents");

        replay
    }

    macro_rules! corpus_oracle {
        ($name:ident, $file:literal) => {
            #[test]
            fn $name() {
                let source = parse(
                    include_bytes!(concat!("../../tests/corpus/", $file)),
                    CORPUS_LINES,
                    CORPUS_COLS,
                );
                round_trip(&source, $file);
            }
        };
    }

    corpus_oracle!(corpus_codex_resume, "codex_resume.bin");
    corpus_oracle!(corpus_tmux_split, "tmux_split.bin");
    corpus_oracle!(corpus_vim_session, "vim_session.bin");
    corpus_oracle!(corpus_less_altscreen, "less_altscreen.bin");
    corpus_oracle!(corpus_top_live, "top_live.bin");
    corpus_oracle!(corpus_shell_colors, "shell_colors.bin");
    corpus_oracle!(corpus_build_log, "build_log.bin");
    corpus_oracle!(corpus_wide_emoji, "wide_emoji.bin");
    corpus_oracle!(corpus_dec_scrollregion, "dec_scrollregion.bin");

    /// Scrolled-viewport oracle: a nonzero display offset must serialize the
    /// displayed (offset) content, not the live screen.
    #[test]
    fn corpus_scrolled_viewport() {
        let mut source = parse(
            include_bytes!("../../tests/corpus/codex_resume.bin"),
            CORPUS_LINES,
            CORPUS_COLS,
        );
        source.scroll_display(Scroll::Delta(10));
        assert!(
            source.grid().display_offset() > 0,
            "premise: fixture retains scrollback to scroll into"
        );
        round_trip(&source, "codex_resume.bin scrolled");

        source.scroll_display(Scroll::Top);
        round_trip(&source, "codex_resume.bin scrolled to top");
    }

    // Targeted synthetic cases cover each serialization edge case separately.

    #[test]
    fn sgr_reset_boundaries() {
        // Attributes active at a row's right edge must not leak into the next
        // row's default cells, and SGR 0 mid-row must restore defaults.
        let source = parse(
            b"\x1b[1;31;44mred on blue\x1b[0m plain\r\n\x1b[7mreverse to eol",
            4,
            20,
        );
        assert_eq!(
            source.grid()[Line(0)][Column(11)].fg,
            Color::Named(NamedColor::Foreground),
            "premise: SGR 0 restored default fg"
        );
        round_trip(&source, "sgr_reset_boundaries");
    }

    #[test]
    fn default_color_restoration() {
        // SGR 39/49 restore one default while the other stays set.
        let source = parse(b"\x1b[31;44mA\x1b[39mB\x1b[49mC", 2, 10);
        let cell = &source.grid()[Line(0)][Column(1)];
        assert_eq!(
            cell.fg,
            Color::Named(NamedColor::Foreground),
            "premise: 39 reset fg"
        );
        assert_eq!(
            cell.bg,
            Color::Named(NamedColor::Blue),
            "premise: bg survived 39"
        );
        round_trip(&source, "default_color_restoration");
    }

    #[test]
    fn wide_char_spacers_not_double_emitted() {
        let source = parse("ab漢字c🙂d".as_bytes(), 2, 20);
        assert!(
            source.grid()[Line(0)][Column(3)]
                .flags
                .contains(Flags::WIDE_CHAR_SPACER),
            "premise: spacer follows the wide glyph"
        );
        round_trip(&source, "wide_char_spacers_not_double_emitted");
    }

    #[test]
    fn wide_char_ending_in_last_column() {
        // Wide glyph occupying the last two columns: spacer sits in the last
        // column and the write leaves the cursor in pending-wrap state.
        let mut bytes = b"\x1b[1;9H".to_vec();
        bytes.extend("漢".as_bytes());
        let source = parse(&bytes, 3, 10);
        assert!(
            source.grid()[Line(0)][Column(9)]
                .flags
                .contains(Flags::WIDE_CHAR_SPACER),
            "premise: spacer in the last column"
        );
        assert!(
            source.grid().cursor.input_needs_wrap,
            "premise: pending wrap over the spacer"
        );
        round_trip(&source, "wide_char_ending_in_last_column");
    }

    #[test]
    fn wide_char_wrapped_from_last_column() {
        // Wide glyph that does not fit in the last column: the source leaves a
        // LEADING_WIDE_CHAR_SPACER there and wraps the glyph. Replay cannot
        // recreate the leading spacer (wrap artifact); the glyph itself must
        // land on the next row.
        let mut bytes = b"\x1b[1;10H".to_vec();
        bytes.extend("漢".as_bytes());
        let source = parse(&bytes, 3, 10);
        assert!(
            source.grid()[Line(0)][Column(9)]
                .flags
                .contains(Flags::LEADING_WIDE_CHAR_SPACER),
            "premise: leading spacer left behind"
        );
        assert_eq!(
            source.grid()[Line(1)][Column(0)].c,
            '漢',
            "premise: glyph wrapped"
        );
        round_trip(&source, "wide_char_wrapped_from_last_column");
    }

    #[test]
    fn zerowidth_combining_marks() {
        // Combining marks attach to narrow cells, wide cells (through the
        // spacer), and stack when repeated.
        let source = parse(
            "e\u{301}x 漢\u{20d7} a\u{300}\u{301}\u{308}".as_bytes(),
            2,
            20,
        );
        assert_eq!(
            source.grid()[Line(0)][Column(0)].zerowidth(),
            Some(&['\u{301}'][..]),
            "premise: mark stored as cell extra"
        );
        assert_eq!(
            source.grid()[Line(0)][Column(3)].zerowidth(),
            Some(&['\u{20d7}'][..]),
            "premise: mark attached to the wide cell, not its spacer"
        );
        round_trip(&source, "zerowidth_combining_marks");
    }

    #[test]
    fn erased_with_attrs_styled_blanks() {
        // BCE: ED/EL fill cells with the template background. Those blanks
        // carry attributes and must be re-emitted, never skipped as empty.
        let source = parse(b"\x1b[44m\x1b[2J\x1b[3;3Hx\x1b[45m\x1b[K", 6, 12);
        assert_eq!(
            source.grid()[Line(5)][Column(11)].bg,
            Color::Named(NamedColor::Blue),
            "premise: ED filled with colored background"
        );
        assert_eq!(
            source.grid()[Line(2)][Column(5)].bg,
            Color::Named(NamedColor::Magenta),
            "premise: EL filled with a different background"
        );
        round_trip(&source, "erased_with_attrs_styled_blanks");
    }

    #[test]
    fn cursor_phantom_column() {
        // Writing through the last column leaves pending wrap; replay must
        // recreate it without actually wrapping (write-then-reposition).
        let source = parse(b"0123456789", 3, 10);
        assert!(
            source.grid().cursor.input_needs_wrap,
            "premise: pending wrap set"
        );
        let replay = round_trip(&source, "cursor_phantom_column");
        // The rewrite that recreates pending wrap must not spill onto row 1.
        assert_eq!(
            replay.grid()[Line(1)][Column(0)].c,
            ' ',
            "no wrap during replay"
        );
    }

    #[test]
    fn cursor_position_and_visibility() {
        let hidden = parse(b"hello\x1b[?25l\x1b[2;4H", 4, 10);
        let (_, pos, hide) = formatted(&hidden);
        assert_eq!(pos, (1, 3));
        assert!(hide);
        round_trip(&hidden, "cursor_hidden");

        let visible = parse(b"hello\x1b[3;2H", 4, 10);
        let (_, _, hide) = formatted(&visible);
        assert!(!hide);
        round_trip(&visible, "cursor_visible");
    }

    #[test]
    fn colors_named_and_bright() {
        let mut bytes = Vec::new();
        for i in 30..=37 {
            bytes.extend(format!("\x1b[{i}mA").into_bytes());
        }
        for i in 90..=97 {
            bytes.extend(format!("\x1b[{i}mB").into_bytes());
        }
        bytes.extend(b"\r\n");
        for i in 40..=47 {
            bytes.extend(format!("\x1b[{i}mC").into_bytes());
        }
        for i in 100..=107 {
            bytes.extend(format!("\x1b[{i}mD").into_bytes());
        }
        let source = parse(&bytes, 3, 20);
        round_trip(&source, "colors_named_and_bright");
    }

    #[test]
    fn colors_indexed_256() {
        let mut bytes = Vec::new();
        for i in [0u8, 7, 15, 16, 123, 231, 232, 255] {
            bytes.extend(format!("\x1b[38;5;{i}mx\x1b[48;5;{i}my").into_bytes());
        }
        let source = parse(&bytes, 2, 20);
        round_trip(&source, "colors_indexed_256");
    }

    #[test]
    fn colors_rgb() {
        let source = parse(
            b"\x1b[38;2;1;2;3mA\x1b[48;2;250;128;0mB\x1b[38;2;255;255;255;48;2;0;0;0mC",
            2,
            10,
        );
        round_trip(&source, "colors_rgb");
    }

    #[test]
    fn attributes_each() {
        // One cell per attribute so a failure names the attribute by column.
        let cases: &[(&str, Flags)] = &[
            ("1", Flags::BOLD),
            ("2", Flags::DIM),
            ("3", Flags::ITALIC),
            ("4", Flags::UNDERLINE),
            ("4:2", Flags::DOUBLE_UNDERLINE),
            ("4:3", Flags::UNDERCURL),
            ("4:4", Flags::DOTTED_UNDERLINE),
            ("4:5", Flags::DASHED_UNDERLINE),
            ("7", Flags::INVERSE),
            ("8", Flags::HIDDEN),
            ("9", Flags::STRIKEOUT),
        ];
        let mut bytes = Vec::new();
        for (sgr, _) in cases {
            bytes.extend(format!("\x1b[{sgr}mA\x1b[0m").into_bytes());
        }
        let source = parse(&bytes, 2, 20);
        for (col, (sgr, flag)) in cases.iter().enumerate() {
            assert!(
                source.grid()[Line(0)][Column(col)].flags.contains(*flag),
                "premise: SGR {sgr} stored its flag"
            );
        }
        round_trip(&source, "attributes_each");
    }

    #[test]
    fn attr_blink_not_modeled() {
        // alacritty stores no blink flag: SGR 5/6 must not perturb the round
        // trip, and the drop is symmetric (documented in the module docs).
        let source = parse(b"\x1b[5mslow\x1b[6mfast\x1b[25m off", 2, 20);
        assert_eq!(
            source.grid()[Line(0)][Column(0)].flags,
            Flags::empty(),
            "premise: backend dropped blink entirely"
        );
        round_trip(&source, "attr_blink_not_modeled");
    }

    #[test]
    fn underline_color_extras() {
        let source = parse(b"\x1b[4;58;5;99mA\x1b[4;58;2;10;20;30mB\x1b[59mC", 2, 10);
        assert_eq!(
            source.grid()[Line(0)][Column(0)].underline_color(),
            Some(Color::Indexed(99)),
            "premise: underline color stored as extra"
        );
        round_trip(&source, "underline_color_extras");
    }

    #[test]
    fn tab_cells_keep_erased_attrs() {
        // put_tab stores a literal '\t' in the cell it lands on, preserving
        // whatever attributes the cell already had (here: a BCE fill).
        let source = parse(b"\x1b[44m\x1b[2J\x1b[Ha\tb", 3, 20);
        assert_eq!(
            source.grid()[Line(0)][Column(1)].c,
            '\t',
            "premise: tab stored in cell"
        );
        assert_eq!(
            source.grid()[Line(0)][Column(1)].bg,
            Color::Named(NamedColor::Blue),
            "premise: tab cell kept the BCE background"
        );
        round_trip(&source, "tab_cells_keep_erased_attrs");
    }

    #[test]
    fn dec_line_drawing_charset() {
        // The backend translates the DEC special graphics charset at write
        // time, so serialized cells are already Unicode; replay needs no
        // charset shifts.
        let source = parse(b"\x1b(0lqk\x1b(B done", 2, 12);
        assert_eq!(
            source.grid()[Line(0)][Column(0)].c,
            '┌',
            "premise: charset translated"
        );
        round_trip(&source, "dec_line_drawing_charset");
    }

    #[test]
    fn alt_screen_visible_grid() {
        // The serializer reads the active grid; an alt-screen source must
        // reproduce the alt content on the replay's (primary) screen.
        let source = parse(b"primary\x1b[?1049h\x1b[2;2Halt content\x1b[31mred", 4, 20);
        assert!(
            source.mode().contains(TermMode::ALT_SCREEN),
            "premise: on alt screen"
        );
        round_trip(&source, "alt_screen_visible_grid");
    }

    #[test]
    fn orphan_spacer_after_ech() {
        // ECH over a wide glyph resets it but leaves the spacer flagged: the
        // orphan is displayed as a styled blank and its structural flag is
        // excluded from comparison (unrepresentable).
        let mut bytes = "漢".as_bytes().to_vec();
        bytes.extend(b"\x1b[1;1H\x1b[1X");
        let source = parse(&bytes, 2, 10);
        let line = &source.grid()[Line(0)];
        assert!(
            line[Column(1)].flags.contains(Flags::WIDE_CHAR_SPACER)
                && !line[Column(0)].flags.contains(Flags::WIDE_CHAR),
            "premise: ECH orphaned the spacer"
        );
        round_trip(&source, "orphan_spacer_after_ech");
    }

    #[test]
    fn orphan_wide_after_ech() {
        // ECH on the spacer orphans the glyph half; it is normalized to a
        // styled blank (see module docs).
        let mut bytes = "漢".as_bytes().to_vec();
        bytes.extend(b"\x1b[1;2H\x1b[1X");
        let source = parse(&bytes, 2, 10);
        let line = &source.grid()[Line(0)];
        assert!(
            line[Column(0)].flags.contains(Flags::WIDE_CHAR)
                && !line[Column(1)].flags.contains(Flags::WIDE_CHAR_SPACER),
            "premise: ECH orphaned the wide glyph"
        );
        round_trip(&source, "orphan_wide_after_ech");
    }

    #[test]
    fn contents_plain_text() {
        // The '\t' cell reads as a blank; 'x' lands at the tab stop (col 8);
        // trailing spaces trim per row; the empty last row stays a line.
        let source = parse("one\r\ntwo 漢\u{301}字\r\n\tx".as_bytes(), 4, 12);
        assert_eq!(contents(&source), "one\ntwo 漢\u{301}字\n        x\n");
    }

    #[test]
    fn contents_conceals_hidden_cells() {
        // SGR 8 cells display blank, so the plain-text view reads them as
        // spaces (`formatted` re-emits SGR 8 and both views must agree). The
        // combining mark on the hidden 'S' must not leak, and the trailing
        // hidden run trims away like padding.
        let source = parse(
            "ab\x1b[8mS\u{301}ECRET\x1b[28mcd \x1b[8mtail".as_bytes(),
            2,
            20,
        );
        assert_eq!(contents(&source), "ab      cd\n");
    }

    #[test]
    fn hidden_wide_glyphs_keep_both_columns() {
        // A concealed wide glyph occupies two cells; both read as spaces so
        // later glyphs keep their columns.
        let source = parse("\x1b[8m日\x1b[28mx".as_bytes(), 1, 10);
        assert_eq!(contents(&source), "  x");
    }

    // Randomized escape-soup round trip: deterministic (fixed seeds, no
    // wall-clock-dependent sequences), covering interleavings the targeted
    // cases cannot enumerate.

    /// Small deterministic PRNG with no additional dependency.
    struct XorShift64(u64);

    impl XorShift64 {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// Pseudo-random mixture of text, SGR, cursor movement, erases, scroll
    /// regions, and partial escapes. Excluded on purpose: OSC 8 (hyperlinks
    /// are an excluded comparison dimension) and ?2026 (vte's synchronized-
    /// update timeout reads the wall clock, which would make the test
    /// time-dependent).
    fn escape_soup(seed: u64, tokens: usize, lines: u64, cols: u64) -> Vec<u8> {
        const WIDE: [char; 6] = ['漢', '字', '🙂', '🚀', '中', '文'];
        const COMBINING: [char; 4] = ['\u{300}', '\u{301}', '\u{308}', '\u{20d7}'];
        const SGR: [&str; 30] = [
            "0", "1", "2", "3", "4", "4:2", "4:3", "4:4", "4:5", "5", "7", "8", "9", "21", "22",
            "23", "24", "25", "27", "28", "29", "31", "33", "35", "37", "39", "44", "49", "97",
            "104",
        ];
        let mut rng = XorShift64(seed);
        let mut out = String::new();
        for _ in 0..tokens {
            match rng.below(24) {
                0..=6 => {
                    for _ in 0..=rng.below(6) {
                        out.push((b' ' + rng.below(95) as u8) as char);
                    }
                }
                7 => out.push(WIDE[rng.below(WIDE.len() as u64) as usize]),
                8 => out.push(COMBINING[rng.below(COMBINING.len() as u64) as usize]),
                9..=11 => {
                    let _ = write!(out, "\x1b[{}m", SGR[rng.below(SGR.len() as u64) as usize]);
                }
                12 => {
                    let _ = match rng.below(5) {
                        0 => write!(out, "\x1b[38;5;{}m", rng.below(256)),
                        1 => write!(out, "\x1b[48;5;{}m", rng.below(256)),
                        2 => write!(
                            out,
                            "\x1b[38;2;{};{};{}m",
                            rng.below(256),
                            rng.below(256),
                            rng.below(256)
                        ),
                        3 => write!(
                            out,
                            "\x1b[48;2;{};{};{}m",
                            rng.below(256),
                            rng.below(256),
                            rng.below(256)
                        ),
                        _ => write!(out, "\x1b[58;5;{}m", rng.below(256)),
                    };
                }
                13..=15 => match rng.below(8) {
                    0 => {
                        let _ = write!(
                            out,
                            "\x1b[{};{}H",
                            1 + rng.below(lines),
                            1 + rng.below(cols)
                        );
                    }
                    1 => {
                        let _ = write!(out, "\x1b[{}A", 1 + rng.below(4));
                    }
                    2 => {
                        let _ = write!(out, "\x1b[{}B", 1 + rng.below(4));
                    }
                    3 => {
                        let _ = write!(out, "\x1b[{}C", 1 + rng.below(8));
                    }
                    4 => {
                        let _ = write!(out, "\x1b[{}D", 1 + rng.below(8));
                    }
                    5 => out.push('\r'),
                    6 => out.push('\n'),
                    _ => out.push('\x08'),
                },
                16 => {
                    let _ = write!(out, "\x1b[{}J", rng.below(3));
                }
                17 => {
                    let _ = write!(out, "\x1b[{}K", rng.below(3));
                }
                18 => {
                    let n = 1 + rng.below(4);
                    let _ = match rng.below(3) {
                        0 => write!(out, "\x1b[{n}X"),
                        1 => write!(out, "\x1b[{n}P"),
                        _ => write!(out, "\x1b[{n}@"),
                    };
                }
                19 => {
                    let _ = match rng.below(4) {
                        0 => {
                            let top = 1 + rng.below(lines / 2);
                            write!(out, "\x1b[{};{}r", top, top + rng.below(lines / 2))
                        }
                        1 => write!(out, "\x1b[{}S", 1 + rng.below(3)),
                        2 => write!(out, "\x1b[{}T", 1 + rng.below(3)),
                        _ => write!(out, "\x1bM"),
                    };
                }
                20 => out.push_str(if rng.below(2) == 0 { "\x1b7" } else { "\x1b8" }),
                21 => {
                    let mode = ["?25", "?7", "?6"][rng.below(3) as usize];
                    let hl = if rng.below(2) == 0 { 'h' } else { 'l' };
                    let _ = write!(out, "\x1b[{mode}{hl}");
                }
                22 => {
                    let n = 1 + rng.below(3);
                    let _ = if rng.below(2) == 0 {
                        write!(out, "\x1b[{n}L")
                    } else {
                        write!(out, "\x1b[{n}M")
                    };
                }
                // After a bare ESC or unterminated CSI, the following token is parsed
                // as parameter bytes: deterministic parsing of partial input.
                _ => {
                    if rng.below(2) == 0 {
                        out.push('\x1b');
                    } else {
                        let _ = write!(out, "\x1b[{}", rng.below(100));
                    }
                }
            }
        }
        out.push('\t');
        out.into_bytes()
    }

    #[test]
    fn escape_soup_round_trip() {
        let mut wide_cells = 0usize;
        let mut colored_cells = 0usize;
        for seed in [
            0x9e3779b97f4a7c15u64,
            0xdeadbeefcafef00d,
            0x0123456789abcdef,
        ] {
            let bytes = escape_soup(seed, 4000, 24, 80);
            let source = parse(&bytes, 24, 80);
            for row in 0..24 {
                let line = &source.grid()[Line(row)];
                for col in 0..80 {
                    let cell = &line[Column(col)];
                    wide_cells += usize::from(cell.flags.contains(Flags::WIDE_CHAR));
                    colored_cells += usize::from(cell.fg != Color::Named(NamedColor::Foreground));
                }
            }
            round_trip(&source, &format!("escape_soup seed {seed:#x}"));
        }
        // Degeneracy guard: a generator edit that stops producing wide glyphs
        // or colors would leave the round trip green while testing nothing.
        assert!(wide_cells > 0, "soup produced no wide cells");
        assert!(colored_cells > 0, "soup produced no colored cells");
    }
}
