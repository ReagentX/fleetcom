//! Differential tests for the production and vt100 terminal emulators. Both
//! backends parse the recorded PTY corpus, and each difference is either an
//! encoding equivalence, an explicitly asserted semantic difference, or a
//! test failure. Serialized output is compared by displayed state rather than
//! byte equality.
//!
//! **Compatibility suite.** Primary comparison is reconstructed screen state:
//! each backend's screen-as-ANSI (`vt100::Screen::contents_formatted` vs
//! [`serialize::formatted`]) is replayed into a fresh alacritty reference
//! terminal and the two reference grids are compared cell-by-cell. The replay
//! step is what absorbs encoding equivalences. The backends legitimately
//! choose different SGR parameters and addressing, but a client terminal must
//! display the same thing. Plain text and cursor position are compared across
//! backends directly as a secondary check.
//!
//! **Semantic suite.** Fixtures with parser-level differences assert exact
//! values for top-anchored-region scrollback retention, bold-plus-dim
//! intensity stacking, DEC charset translation, and VS16 width.

use alacritty_terminal::{
    Term,
    event::VoidListener,
    grid::Dimensions,
    index::{Column, Line, Point},
    term::{Config, TermMode, cell::Flags, test::TermSize},
    vte::ansi::{Color, Processor},
};

use crate::serialize;

/// Corpus dimensions: 40 rows by 120 columns.
const LINES: usize = 40;
const COLS: usize = 120;

/// Scrollback depth used by production tasks and the vt100 test backend.
const SCROLLBACK: usize = 2000;

/// Wrap bookkeeping is invisible on screen and diverges by construction:
/// vt100's serialization recreates soft wraps by writing through the right
/// edge, while [`serialize::formatted`] is CUP-per-row and never wraps.
/// These flags are masked in the reference-grid comparison.
const WRAP_ARTIFACTS: Flags = Flags::WRAPLINE.union(Flags::LEADING_WIDE_CHAR_SPACER);

fn alacritty(bytes: &[u8]) -> Term<VoidListener> {
    let mut term = Term::new(Config::default(), &TermSize::new(COLS, LINES), VoidListener);
    let mut parser: Processor = Processor::new();
    parser.advance(&mut term, bytes);
    term
}

fn vt100(bytes: &[u8]) -> vt100::Parser {
    let mut parser = vt100::Parser::new(LINES as u16, COLS as u16, SCROLLBACK);
    parser.process(bytes);
    parser
}

/// Rows vt100 retained in scrollback. vt100 exposes no direct count; the
/// viewport offset clamps to stored history, so requesting `usize::MAX` and
/// reading the offset back measures it. Restores the live view.
fn vt100_retained(parser: &mut vt100::Parser) -> usize {
    parser.screen_mut().set_scrollback(usize::MAX);
    let rows = parser.screen().scrollback();
    parser.screen_mut().set_scrollback(0);
    rows
}

/// Two spellings of one palette slot: `CSI 3x m` parses to a named color,
/// `CSI 38;5;x m` to an indexed one, and both address palette entry `x`: a
/// client displays them identically. vt100 stores every color as an index and
/// re-emits 0-15 in the short form; the alacritty serializer preserves the
/// child's spelling. Comparisons canonicalize both to the indexed form.
fn canon(color: Color) -> Color {
    match color {
        Color::Named(named) if (named as usize) < 16 => Color::Indexed(named as u8),
        other => other,
    }
}

/// Cell-by-cell comparison of two reference terminals that replayed each
/// backend's serialized screen: character, zero-width extras, colors, style
/// flags, underline color. Exactly two documented equivalences are absorbed:
/// the `WRAP_ARTIFACTS` mask and [`canon`]'s palette-spelling collapse.
///
/// `allow_bold_dim` admits one parser-level difference: after SGR 1 followed
/// by SGR 2 without SGR 22, alacritty stores both flags while vt100 stores
/// only the later intensity. The return value counts affected cells so the
/// semantic test can assert the exact footprint; compatibility fixtures pass
/// `false` and reject the difference.
fn assert_reference_grids_match(
    vt_ref: &Term<VoidListener>,
    al_ref: &Term<VoidListener>,
    fixture: &str,
    allow_bold_dim: bool,
) -> usize {
    let vgrid = vt_ref.grid();
    let agrid = al_ref.grid();
    let mut bold_dim_cells = 0;
    for row in 0..LINES {
        let vline = &vgrid[Line(row as i32)];
        let aline = &agrid[Line(row as i32)];
        for col in 0..COLS {
            let v = &vline[Column(col)];
            let a = &aline[Column(col)];
            assert_eq!(v.c, a.c, "{fixture}: char at ({row},{col})");
            assert_eq!(canon(v.fg), canon(a.fg), "{fixture}: fg at ({row},{col})");
            assert_eq!(canon(v.bg), canon(a.bg), "{fixture}: bg at ({row},{col})");
            let vflags = v.flags.difference(WRAP_ARTIFACTS);
            let aflags = a.flags.difference(WRAP_ARTIFACTS);
            // The bold+dim shape: alacritty holds both intensity flags,
            // vt100 exactly one of them, all other bits equal.
            let intensity = Flags::BOLD.union(Flags::DIM);
            let one_of = vflags.intersection(intensity);
            if allow_bold_dim
                && (one_of == Flags::BOLD || one_of == Flags::DIM)
                && aflags == vflags.union(intensity)
            {
                bold_dim_cells += 1;
            } else {
                assert_eq!(vflags, aflags, "{fixture}: flags at ({row},{col})");
            }
            assert_eq!(
                v.zerowidth().unwrap_or(&[]),
                a.zerowidth().unwrap_or(&[]),
                "{fixture}: zerowidth at ({row},{col})"
            );
            assert_eq!(
                v.underline_color(),
                a.underline_color(),
                "{fixture}: underline color at ({row},{col})"
            );
        }
    }
    bold_dim_cells
}

/// The compatibility comparison: both backends parse `bytes`, then
///
/// 1. each backend's screen-as-ANSI replays into a fresh reference terminal
///    and the reference grids, cursors, and cursor-visibility modes must
///    match: what a client terminal would display;
/// 2. cursor position, visibility, and per-row plain text are compared across
///    backends directly.
///
/// Returns both parsers so semantic goldens can assert their approved deltas
/// on top of a proven-equivalent visible screen, plus the count of cells the
/// bold+dim delta absorbed when `allow_bold_dim` admits it (see
/// [`assert_reference_grids_match`]).
fn compare_backends(
    fixture: &str,
    bytes: &[u8],
    allow_bold_dim: bool,
) -> (vt100::Parser, Term<VoidListener>, usize) {
    let vt = vt100(bytes);
    let al = alacritty(bytes);

    let vt_ref = alacritty(&vt.screen().contents_formatted());
    let (al_bytes, al_pos, al_hidden) = serialize::formatted(&al);
    let al_ref = alacritty(&al_bytes);
    let bold_dim_cells = assert_reference_grids_match(&vt_ref, &al_ref, fixture, allow_bold_dim);
    assert_eq!(
        vt_ref.grid().cursor.point,
        al_ref.grid().cursor.point,
        "{fixture}: reference cursor position"
    );
    assert_eq!(
        vt_ref.mode().contains(TermMode::SHOW_CURSOR),
        al_ref.mode().contains(TermMode::SHOW_CURSOR),
        "{fixture}: reference cursor visibility"
    );

    assert_eq!(
        vt.screen().cursor_position(),
        al_pos,
        "{fixture}: cursor position across backends"
    );
    assert_eq!(
        vt.screen().hide_cursor(),
        al_hidden,
        "{fixture}: cursor visibility across backends"
    );

    // Per-row text, not `vt100::Screen::contents()`: that joins soft-wrapped
    // rows without a newline, which is a representation choice, not a screen
    // difference. Both sides trim trailing blanks per row.
    let al_text = serialize::contents(&al);
    let al_rows: Vec<&str> = al_text.split('\n').collect();
    assert_eq!(al_rows.len(), LINES, "{fixture}: plain-text row count");
    for (row, vt_row) in vt.screen().rows(0, COLS as u16).enumerate() {
        assert_eq!(
            vt_row, al_rows[row],
            "{fixture}: plain text at row {row} (vt100 left, alacritty right)"
        );
    }

    (vt, al, bold_dim_cells)
}

/// Compatibility entry point: no parser-level deltas admitted.
fn assert_visible_equivalent(fixture: &str, bytes: &[u8]) -> (vt100::Parser, Term<VoidListener>) {
    let (vt, al, _) = compare_backends(fixture, bytes, false);
    (vt, al)
}

macro_rules! compat {
    ($name:ident, $file:literal) => {
        #[test]
        fn $name() {
            assert_visible_equivalent($file, include_bytes!(concat!("../tests/corpus/", $file)));
        }
    };
}

compat!(compat_tmux_split, "tmux_split.bin");
compat!(compat_vim_session, "vim_session.bin");
compat!(compat_less_altscreen, "less_altscreen.bin");
compat!(compat_top_live, "top_live.bin");
compat!(compat_shell_colors, "shell_colors.bin");
compat!(compat_build_log, "build_log.bin");

// Semantic tests assert cross-backend differences as concrete values.

/// The Codex fixture pushes chat history into scrollback through a top-anchored
/// DECSTBM region (`CSI 1;N r` plus `\r\n` at the bottom). vt100 retains no
/// rows from those scrolls, while alacritty retains 85.
///
/// The fixture styles its two-cell "› " prompt marker with bold followed by
/// dim. alacritty stores both flags, while vt100 stores only dim. Apart from
/// these asserted differences, the visible screen, cursor, and plain text are
/// equivalent.
#[test]
fn semantic_codex_resume_scrollback_retention() {
    let (mut vt, al, bold_dim_cells) = compare_backends(
        "codex_resume.bin",
        include_bytes!("../tests/corpus/codex_resume.bin"),
        true,
    );
    assert_eq!(
        vt100_retained(&mut vt),
        0,
        "vt100 drops all region-scrolled history"
    );
    assert_eq!(
        al.grid().history_size(),
        85,
        "alacritty retains the codex chat history"
    );

    // The bold+dim delta's exact footprint: the "› " prompt marker, row 7.
    assert_eq!(bold_dim_cells, 2, "cells the intensity delta touches");
    let marker = &al.grid()[Line(7)][Column(0)];
    assert_eq!(marker.c, '\u{203a}');
    assert!(
        marker.flags.contains(Flags::BOLD.union(Flags::DIM)),
        "alacritty stacks bold and dim"
    );
    let vt_marker = vt.screen().cell(7, 0).unwrap();
    assert!(
        !vt_marker.bold() && vt_marker.dim(),
        "vt100 keeps only the later SGR (dim)"
    );
}

/// Golden: the retention delta in its minimal synthetic form. A top-anchored
/// `CSI 1;20 r` region with 34 newlines scrolled through its bottom margin.
/// vt100 retains none of the scrolled-off rows, while alacritty retains all
/// 34. The visible screens stay equivalent; only history differs.
#[test]
fn semantic_topregion_scroll_retention() {
    let (mut vt, al, _) = compare_backends(
        "topregion_scroll.bin",
        include_bytes!("../tests/corpus/topregion_scroll.bin"),
        false,
    );
    assert_eq!(
        vt100_retained(&mut vt),
        0,
        "vt100 drops all region-scrolled history"
    );
    // alacritty retains 34 region scrolls plus the initial row preserved by
    // `ESC[2J`; vt100 erases the initial row in place.
    assert_eq!(
        al.grid().history_size(),
        35,
        "alacritty retains one row per bottom-margin newline, plus ED 2's"
    );
}

/// VS16 emoji-presentation width is equal in both configured backends.
/// U+26A0+VS16 occupies one cell, with VS16 stored as a zero-width attachment,
/// so the fixture has identical emoji column alignment.
#[test]
fn semantic_wide_emoji_vs16_width_parity() {
    let (vt, al) = assert_visible_equivalent(
        "wide_emoji.bin",
        include_bytes!("../tests/corpus/wide_emoji.bin"),
    );
    let grid = al.grid();

    // U+2705 has emoji presentation by default: two cells in both backends.
    let check = &grid[Line(0)][Column(8)];
    assert_eq!(check.c, '\u{2705}');
    assert!(check.flags.contains(Flags::WIDE_CHAR));
    assert!(vt.screen().cell(0, 8).unwrap().is_wide());

    // U+26A0 is width 1; VS16 attaches as a zero-width extra and does not
    // widen the cell, in either backend.
    let warn = &grid[Line(0)][Column(16)];
    assert_eq!(warn.c, '\u{26a0}');
    assert!(!warn.flags.contains(Flags::WIDE_CHAR));
    assert_eq!(warn.zerowidth(), Some(&['\u{fe0f}'][..]));
    let vt_warn = vt.screen().cell(0, 16).unwrap();
    assert_eq!(vt_warn.contents(), "\u{26a0}\u{fe0f}");
    assert!(!vt_warn.is_wide());

    // The alignment consequence: the following text starts one cell after
    // the narrow emoji in both backends, and the next default-wide emoji
    // lands on the same column.
    assert_eq!(grid[Line(0)][Column(18)].c, 'w');
    assert_eq!(vt.screen().cell(0, 18).unwrap().contents(), "w");
    let fire = &grid[Line(0)][Column(23)];
    assert_eq!(fire.c, '\u{1f525}');
    assert!(fire.flags.contains(Flags::WIDE_CHAR));
    assert!(vt.screen().cell(0, 23).unwrap().is_wide());
}

/// Golden: DEC line-drawing charset (SCS). vt100 leaves special-graphics bytes
/// as ASCII, while alacritty translates them to box-drawing glyphs.
///
/// The fixture's region starts at row 5 and does not scroll. Its only scroll
/// is a full-screen `\r\n` after the region resets, and both backends retain
/// that row.
#[test]
fn semantic_dec_scrollregion_charset_translation() {
    let bytes = include_bytes!("../tests/corpus/dec_scrollregion.bin");
    let mut vt = vt100(bytes);
    let al = alacritty(bytes);

    // Identical retention: one full-screen scroll, both backends keep it.
    assert_eq!(vt100_retained(&mut vt), 1, "vt100 retains the one scroll");
    assert_eq!(
        al.grid().history_size(),
        1,
        "alacritty retains the same one scroll"
    );

    // The box's top edge is the scrolled-off row: translated in alacritty's
    // history, untranslated in vt100's.
    let top = al.grid()[Line(-1)]
        .into_iter()
        .map(|cell| cell.c)
        .collect::<String>();
    assert_eq!(top.trim_end(), "┌─────┐");
    vt.screen_mut().set_scrollback(1);
    let vt_top = vt.screen().rows(0, COLS as u16).next().unwrap();
    vt.screen_mut().set_scrollback(0);
    assert_eq!(vt_top, "lqqqqqk");

    // Visible screen: the box body diverges per charset, everything after
    // `ESC ( B` (and everything the region touched) is identical.
    let al_text = serialize::contents(&al);
    let al_rows: Vec<&str> = al_text.split('\n').collect();
    let vt_rows: Vec<String> = vt.screen().rows(0, COLS as u16).collect();
    assert_eq!(al_rows[0], "│     │");
    assert_eq!(vt_rows[0], "x     x");
    assert_eq!(al_rows[1], "└─────┘");
    assert_eq!(vt_rows[1], "mqqqqqj");
    let shared = [
        (2, "ascii after charset"),
        (3, "inside region 1"),
        (4, "inside region 2"),
        (5, "inside region 3"),
        (38, "bottom line after region reset"),
    ];
    for (row, text) in shared {
        assert_eq!(al_rows[row], text, "alacritty row {row}");
        assert_eq!(vt_rows[row], text, "vt100 row {row}");
    }
    for row in (6..38).chain([39]) {
        assert_eq!(al_rows[row], "", "alacritty row {row} blank");
        assert_eq!(vt_rows[row], "", "vt100 row {row} blank");
    }
    assert_eq!(vt.screen().cursor_position(), (39, 0));
    assert_eq!(al.grid().cursor.point, Point::new(Line(39), Column(0)));
}
