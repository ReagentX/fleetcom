//! Terminal parser changes can preserve plain text while shifting cursor state,
//! styling, or scrollback. These golden tests replay the recorded PTY corpus
//! (`tests/corpus`) and report the exact row, cell, or count on a mismatch.
//!
//! **Displayed state.** Each fixture pins every final plain-text row via
//! [`ansi::contents`], cursor position and visibility via
//! [`ansi::formatted`], and selected styled cells for the fixture's
//! purpose (see `tests/corpus/README.md`).
//!
//! **Parser semantics.** Targeted fixtures pin exact scrollback retention,
//! bold-plus-dim intensity stacking, DEC charset translation, and VS16 width.

use alacritty_terminal::{
    Term,
    event::VoidListener,
    grid::Dimensions,
    index::{Column, Line, Point},
    term::{Config, TermMode, cell::Flags, test::TermSize},
    vte::ansi::{Color, NamedColor, Processor, Rgb},
};

use crate::{
    ansi,
    testutil::{CORPUS_COLS as COLS, CORPUS_LINES as LINES},
};

fn alacritty(bytes: &[u8]) -> Term<VoidListener> {
    let mut term = Term::new(Config::default(), &TermSize::new(COLS, LINES), VoidListener);
    let mut parser: Processor = Processor::new();
    parser.advance(&mut term, bytes);
    term
}

/// Pin every visible text row, cursor position and visibility, and the active
/// primary screen. Any row omitted from `rows` must be blank.
fn assert_screen(
    fixture: &str,
    al: &Term<VoidListener>,
    rows: &[(usize, &str)],
    cursor: (u16, u16),
) {
    assert!(
        !al.mode().contains(TermMode::ALT_SCREEN),
        "{fixture}: primary screen active"
    );
    let text = ansi::contents(al);
    let got: Vec<&str> = text.split('\n').collect();
    assert_eq!(got.len(), LINES, "{fixture}: plain-text row count");
    for (row, line) in got.iter().enumerate() {
        let expected = rows
            .iter()
            .find_map(|&(r, t)| (r == row).then_some(t))
            .unwrap_or("");
        assert_eq!(*line, expected, "{fixture}: plain text at row {row}");
    }
    let (_, pos, hidden) = ansi::formatted(al);
    assert_eq!(pos, cursor, "{fixture}: cursor position");
    assert!(!hidden, "{fixture}: cursor visibility");
}

/// Pin one load-bearing styled cell: character, colors, exact flag set.
fn assert_cell(
    fixture: &str,
    al: &Term<VoidListener>,
    (row, col): (usize, usize),
    c: char,
    fg: Color,
    bg: Color,
    flags: Flags,
) {
    let cell = &al.grid()[Line(row as i32)][Column(col)];
    assert_eq!(cell.c, c, "{fixture}: char at ({row},{col})");
    assert_eq!(cell.fg, fg, "{fixture}: fg at ({row},{col})");
    assert_eq!(cell.bg, bg, "{fixture}: bg at ({row},{col})");
    assert_eq!(cell.flags, flags, "{fixture}: flags at ({row},{col})");
}

/// Pin default characters, colors, and flags across the primary grid after an
/// alternate-screen fixture exits. This catches styling on blank cells, which
/// the plain-text assertion cannot observe.
fn assert_grid_unstyled(fixture: &str, al: &Term<VoidListener>) {
    for row in 0..LINES {
        for col in 0..COLS {
            let cell = &al.grid()[Line(row as i32)][Column(col)];
            assert_eq!(cell.c, ' ', "{fixture}: char at ({row},{col})");
            assert_eq!(
                cell.fg,
                Color::Named(NamedColor::Foreground),
                "{fixture}: fg at ({row},{col})"
            );
            assert_eq!(
                cell.bg,
                Color::Named(NamedColor::Background),
                "{fixture}: bg at ({row},{col})"
            );
            assert!(cell.flags.is_empty(), "{fixture}: flags at ({row},{col})");
        }
    }
}

/// tmux detach leaves its message on the primary screen with the cursor on
/// the following row. The first cell pins the message's default styling.
#[test]
fn compat_tmux_split() {
    let al = alacritty(include_bytes!("../../tests/corpus/tmux_split.bin"));
    assert_screen(
        "tmux_split.bin",
        &al,
        &[(0, "[detached (from session 0)]")],
        (1, 0),
    );
    assert_cell(
        "tmux_split.bin",
        &al,
        (0, 0),
        '[',
        Color::Named(NamedColor::Foreground),
        Color::Named(NamedColor::Background),
        Flags::empty(),
    );
}

/// `vim` runs entirely on the alternate screen; `:q!` restores a blank primary
/// grid with default colors and flags and the cursor at the origin.
#[test]
fn compat_vim_session() {
    let al = alacritty(include_bytes!("../../tests/corpus/vim_session.bin"));
    assert_screen("vim_session.bin", &al, &[], (0, 0));
    assert_grid_unstyled("vim_session.bin", &al);
}

/// `less` pages on the alternate screen; `q` restores a blank primary grid
/// with default colors and flags and the cursor at the origin.
#[test]
fn compat_less_altscreen() {
    let al = alacritty(include_bytes!("../../tests/corpus/less_altscreen.bin"));
    assert_screen("less_altscreen.bin", &al, &[], (0, 0));
    assert_grid_unstyled("less_altscreen.bin", &al);
}

/// `top` redraws the alternate screen rapidly; `q` restores a blank primary
/// grid with default colors and flags.
#[test]
fn compat_top_live() {
    let al = alacritty(include_bytes!("../../tests/corpus/top_live.bin"));
    assert_screen("top_live.bin", &al, &[], (0, 0));
    assert_grid_unstyled("top_live.bin", &al);
}

/// Pin SGR output from `ls --color`, `git log --color`, and scripted 16-color,
/// 256-color, and truecolor sequences. Selected cells cover each color depth,
/// bold, underline, reverse, and a background color.
#[test]
fn compat_shell_colors() {
    const F: &str = "shell_colors.bin";
    let al = alacritty(include_bytes!("../../tests/corpus/shell_colors.bin"));
    assert_screen(
        F,
        &al,
        &[
            (0, "-rwxr-xr-x   78 root   wheel    118928 May 21 01:57 as"),
            (1, "-rwxr-xr-x   78 root   wheel    118928 May 21 01:57 asa"),
            (
                2,
                "-rwxr-xr-x    1 root   wheel    171888 May 21 01:57 AssetCacheLocatorUtil",
            ),
            (
                3,
                "-rwxr-xr-x    1 root   wheel    227664 May 21 01:57 AssetCacheManagerUtil",
            ),
            (
                4,
                "-rwxr-xr-x    1 root   wheel    172976 May 21 01:57 AssetCacheTetheratorUtil",
            ),
            (
                5,
                "-rwxr-xr-x    1 root   wheel   4034112 May 21 01:57 assetutil",
            ),
            (6, "-r-sr-xr-x    3 root   wheel    170832 May 21 01:57 at"),
            (
                7,
                "-rwxr-xr-x    1 root   wheel    211936 May 21 01:57 atos",
            ),
            (8, "-r-sr-xr-x    3 root   wheel    170832 May 21 01:57 atq"),
            (
                9,
                "-r-sr-xr-x    3 root   wheel    170832 May 21 01:57 atrm",
            ),
            (
                10,
                "-rwxr-xr-x    1 root   wheel    138096 May 21 01:57 atsutil",
            ),
            (
                11,
                "-rwxr-xr-x    1 root   wheel    136416 May 21 01:57 automationmodetool",
            ),
            (
                12,
                "-rwxr-xr-x    1 root   wheel    171440 May 21 01:57 automator",
            ),
            (
                13,
                "lrwxr-xr-x    1 root   wheel        18 May 21 01:57 auval -> /usr/bin/auvaltool",
            ),
            (
                14,
                "-rwxr-xr-x    1 root   wheel    400544 May 21 01:57 auvaltool",
            ),
            (
                15,
                "-rwxr-xr-x    1 root   wheel    567072 May 21 01:57 avbanalyse",
            ),
            (16, "ls: stdout: Undefined error: 0"),
            (
                17,
                "f00e685 docs: add emulator migration plan (vt100 → alacritty_terminal)",
            ),
            (
                18,
                "3e00d45 Merge pull request #15 from ReagentX/feat/cs/cleanup-imports",
            ),
            (
                19,
                "600908f refactor: clean up import statements across multiple files for improved readability",
            ),
            (
                20,
                "b7800d5 Merge pull request #14 from ReagentX/fix/cs/terminal-restore-guard",
            ),
            (
                21,
                "f21ea98 fix: improve code formatting and comments for clarity in terminal restoration",
            ),
            (
                22,
                "8cc6623 fix: restore the terminal on every exit path after raw mode is enabled",
            ),
            (
                23,
                "a84a60c Merge pull request #13 from ReagentX/feat/cs/session-ownership",
            ),
            (
                24,
                "1e52bae fix: improve code comments and formatting for clarity",
            ),
            (
                25,
                "78f9ddc feat: protocol v5 — session listing and paths follow the connection's launch context",
            ),
            (
                26,
                "411545a Merge pull request #12 from ReagentX/fix/cs/blocking-write-wedge",
            ),
            (
                27,
                "865249d fix: improve documentation for writer queue and message handling",
            ),
            (
                28,
                "ba2d23a fix: bound client socket writes and declare a failed transport dead",
            ),
            (
                29,
                "4496980 fix: move PTY writes to a per-task writer worker so a stalled child cannot wedge the core",
            ),
            (
                30,
                "74e6520 Merge pull request #11 from ReagentX/feat/cs/protocol-v4-hardening",
            ),
            (
                31,
                "47efd3a feat: enhance protocol v4 handling with strict decoding and base64 encoding for paths",
            ),
            (
                32,
                "36d7cc9 feat: enforce MAX_FRAME on write; compile-check the paste-size chain",
            ),
            (
                33,
                "b65f9b4 feat: protocol v4 — strict decode, base64 input/paste bytes, lossless paths",
            ),
            (
                34,
                "efd09da Merge pull request #10 from ReagentX/refactor/cs/mechanical-cleanups",
            ),
            (
                35,
                "e74973e refactor: improve documentation for clarity and conciseness",
            ),
            (
                36,
                "4db69a3 fix: require the hello ack in --kill's socket path; single connect-error report",
            ),
            (37, "bold red underline green reverse"),
            (38, "256color truecolor"),
        ],
        (39, 0),
    );

    let fg = Color::Named(NamedColor::Foreground);
    let bg = Color::Named(NamedColor::Background);
    // ls colors: executable, setuid (fg plus bg), symlink.
    assert_cell(
        F,
        &al,
        (0, 52),
        'a',
        Color::Named(NamedColor::Red),
        bg,
        Flags::empty(),
    );
    assert_cell(
        F,
        &al,
        (6, 52),
        'a',
        Color::Named(NamedColor::Black),
        Color::Named(NamedColor::Red),
        Flags::empty(),
    );
    assert_cell(
        F,
        &al,
        (13, 52),
        'a',
        Color::Named(NamedColor::Magenta),
        bg,
        Flags::empty(),
    );
    // git log hash.
    assert_cell(
        F,
        &al,
        (17, 0),
        'f',
        Color::Named(NamedColor::Yellow),
        bg,
        Flags::empty(),
    );
    // Scripted attribute row: bold, underline, reverse.
    assert_cell(
        F,
        &al,
        (37, 0),
        'b',
        Color::Named(NamedColor::Red),
        bg,
        Flags::BOLD,
    );
    assert_cell(
        F,
        &al,
        (37, 9),
        'u',
        Color::Named(NamedColor::Green),
        bg,
        Flags::UNDERLINE,
    );
    assert_cell(F, &al, (37, 25), 'r', fg, bg, Flags::INVERSE);
    // Color depth: 256-color index and truecolor RGB.
    assert_cell(
        F,
        &al,
        (38, 0),
        '2',
        Color::Indexed(208),
        bg,
        Flags::empty(),
    );
    assert_cell(
        F,
        &al,
        (38, 9),
        't',
        Color::Spec(Rgb {
            r: 100,
            g: 200,
            b: 50,
        }),
        bg,
        Flags::empty(),
    );
}

/// Bulk scrolling `cargo check`/`cargo clippy` output. The bold bright-green
/// "Compiling" and "Finished" cells pin Cargo's status styling.
#[test]
fn compat_build_log() {
    const F: &str = "build_log.bin";
    let al = alacritty(include_bytes!("../../tests/corpus/build_log.bin"));
    assert_screen(
        F,
        &al,
        &[
            (0, "   Compiling libc v0.2.186"),
            (1, "   Compiling serde_core v1.0.228"),
            (2, "   Compiling proc-macro2 v1.0.106"),
            (3, "   Compiling unicode-ident v1.0.24"),
            (4, "   Compiling quote v1.0.46"),
            (5, "   Compiling rustix v1.1.4"),
            (6, "   Compiling serde v1.0.228"),
            (7, "    Checking memchr v2.8.3"),
            (8, "    Checking cfg-if v1.0.4"),
            (9, "   Compiling parking_lot_core v0.9.12"),
            (10, "    Checking scopeguard v1.2.0"),
            (11, "    Checking smallvec v1.15.2"),
            (12, "   Compiling signal-hook v0.4.4"),
            (13, "    Checking lock_api v0.4.14"),
            (14, "    Checking log v0.4.33"),
            (15, "    Checking regex-syntax v0.8.11"),
            (16, "    Checking cursor-icon v1.2.0"),
            (17, "    Checking arrayvec v0.7.8"),
            (18, "    Checking base64 v0.22.1"),
            (19, "    Checking unicode-width v0.2.2"),
            (20, "    Checking home v0.5.12"),
            (21, "    Checking aho-corasick v1.1.4"),
            (22, "    Checking errno v0.3.14"),
            (23, "    Checking signal-hook-registry v1.4.8"),
            (24, "    Checking parking_lot v0.12.5"),
            (25, "   Compiling syn v2.0.118"),
            (26, "    Checking regex-automata v0.4.15"),
            (27, "    Checking bitflags v2.13.0"),
            (28, "   Compiling serde_derive v1.0.228"),
            (29, "    Checking polling v3.11.0"),
            (30, "    Checking rustix-openpty v0.2.0"),
            (31, "    Checking vte v0.15.0"),
            (32, "    Checking alacritty_terminal v0.26.0"),
            (
                33,
                "    Checking depcheck v0.0.0 (/Users/chris/.claude/jobs/6638be1a/tmp/depcheck)",
            ),
            (
                34,
                "    Finished `dev` profile [unoptimized + debuginfo] target(s) in 3.63s",
            ),
            (
                35,
                "    Checking depcheck v0.0.0 (/Users/chris/.claude/jobs/6638be1a/tmp/depcheck)",
            ),
            (
                36,
                "    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.11s",
            ),
        ],
        (37, 0),
    );

    let bg = Color::Named(NamedColor::Background);
    assert_cell(
        F,
        &al,
        (0, 3),
        'C',
        Color::Named(NamedColor::BrightGreen),
        bg,
        Flags::BOLD,
    );
    assert_cell(
        F,
        &al,
        (34, 4),
        'F',
        Color::Named(NamedColor::BrightGreen),
        bg,
        Flags::BOLD,
    );
}

// These fixtures isolate parser behavior that visible-screen checks miss.

/// The Codex fixture pushes chat history through a top-anchored DECSTBM region
/// (`CSI 1;N r` plus `\r\n` at the bottom). The resulting scrollback contains
/// exactly 85 rows.
///
/// The fixture styles its two-cell "› " prompt marker with bold followed by
/// dim and no intervening SGR 22: both intensity flags stack on the cells.
#[test]
fn semantic_codex_resume_scrollback_retention() {
    let al = alacritty(include_bytes!("../../tests/corpus/codex_resume.bin"));
    assert_eq!(al.grid().history_size(), 85, "codex chat history retention");

    let marker = &al.grid()[Line(7)][Column(0)];
    assert_eq!(marker.c, '\u{203a}');
    assert!(
        marker.flags.contains(Flags::BOLD.union(Flags::DIM)),
        "bold and dim stack on the prompt marker"
    );
    let pad = &al.grid()[Line(7)][Column(1)];
    assert!(
        pad.flags.contains(Flags::BOLD.union(Flags::DIM)),
        "the marker's padding cell carries both flags too"
    );
}

/// Isolate top-anchored-region retention with a `CSI 1;20 r` region and 34
/// newlines through its bottom margin. The parser retains 35 rows: 34 region
/// scrolls plus the initial row that `ESC[2J` moves into history.
#[test]
fn semantic_topregion_scroll_retention() {
    let al = alacritty(include_bytes!("../../tests/corpus/topregion_scroll.bin"));
    assert_eq!(
        al.grid().history_size(),
        35,
        "one row per bottom-margin newline, plus ED 2's"
    );
}

/// Pin VS16 emoji-presentation width. U+26A0+VS16 occupies one cell because
/// VS16 remains a zero-width attachment; default-emoji codepoints remain wide.
#[test]
fn semantic_wide_emoji_vs16_width() {
    let al = alacritty(include_bytes!("../../tests/corpus/wide_emoji.bin"));
    let grid = al.grid();

    // U+2705 has emoji presentation by default: two cells.
    let check = &grid[Line(0)][Column(8)];
    assert_eq!(check.c, '\u{2705}');
    assert!(check.flags.contains(Flags::WIDE_CHAR));

    // U+26A0 is width 1; VS16 attaches as a zero-width extra and does not
    // widen the cell.
    let warn = &grid[Line(0)][Column(16)];
    assert_eq!(warn.c, '\u{26a0}');
    assert!(!warn.flags.contains(Flags::WIDE_CHAR));
    assert_eq!(warn.zerowidth(), Some(&['\u{fe0f}'][..]));

    // The alignment consequence: the following text starts one cell after
    // the narrow emoji, and the next default-wide emoji lands on column 23.
    assert_eq!(grid[Line(0)][Column(18)].c, 'w');
    let fire = &grid[Line(0)][Column(23)];
    assert_eq!(fire.c, '\u{1f525}');
    assert!(fire.flags.contains(Flags::WIDE_CHAR));
}

/// Pin the DEC line-drawing charset (SCS). Special-graphics bytes translate to
/// box-drawing glyphs in retained history and on the visible screen; ASCII
/// resumes after `ESC ( B`.
///
/// The fixture's region starts at row 5 and does not scroll; its only scroll
/// is a full-screen `\r\n` after the region resets, retained as one row.
#[test]
fn semantic_dec_scrollregion_charset_translation() {
    let al = alacritty(include_bytes!("../../tests/corpus/dec_scrollregion.bin"));

    assert_eq!(
        al.grid().history_size(),
        1,
        "the one full-screen scroll is retained"
    );

    // The box's top edge is the scrolled-off row: translated in history.
    let top = al.grid()[Line(-1)]
        .into_iter()
        .map(|cell| cell.c)
        .collect::<String>();
    assert_eq!(top.trim_end(), "┌─────┐");

    // Visible screen: box body translated, ASCII rows verbatim, the rest
    // blank.
    let al_text = ansi::contents(&al);
    let al_rows: Vec<&str> = al_text.split('\n').collect();
    assert_eq!(al_rows[0], "│     │");
    assert_eq!(al_rows[1], "└─────┘");
    let shared = [
        (2, "ascii after charset"),
        (3, "inside region 1"),
        (4, "inside region 2"),
        (5, "inside region 3"),
        (38, "bottom line after region reset"),
    ];
    for (row, text) in shared {
        assert_eq!(al_rows[row], text, "row {row}");
    }
    for row in (6..38).chain([39]) {
        assert_eq!(al_rows[row], "", "row {row} blank");
    }
    assert_eq!(al.grid().cursor.point, Point::new(Line(39), Column(0)));
}

/// Compare the [`emulator::Emulator`] wrapper and the raw backend on one
/// fixture: screen, cursor, and alternate-screen mode must match.
///
/// [`emulator::Emulator`]: crate::emulator::Emulator
fn assert_wrapper_matches(file: &str, bytes: &[u8]) {
    let al = alacritty(bytes);
    let mut emu = crate::testutil::corpus_emulator();
    emu.process(bytes);
    let (_, al_cursor, al_hidden) = ansi::formatted(&al);
    let (_, emu_cursor, emu_hidden) = emu.formatted();
    assert_eq!(emu.contents(), ansi::contents(&al), "{file}: screen");
    assert_eq!(
        (emu_cursor, emu_hidden),
        (al_cursor, al_hidden),
        "{file}: cursor"
    );
    assert_eq!(
        emu.alternate_screen(),
        al.mode().contains(TermMode::ALT_SCREEN),
        "{file}: alt bit"
    );
}

macro_rules! wrapper_oracle {
    ($name:ident, $file:literal) => {
        #[test]
        fn $name() {
            assert_wrapper_matches($file, include_bytes!(concat!("../../tests/corpus/", $file)));
        }
    };
}

wrapper_oracle!(wrapper_tmux_split, "tmux_split.bin");
wrapper_oracle!(wrapper_vim_session, "vim_session.bin");
wrapper_oracle!(wrapper_less_altscreen, "less_altscreen.bin");
wrapper_oracle!(wrapper_top_live, "top_live.bin");
wrapper_oracle!(wrapper_shell_colors, "shell_colors.bin");
wrapper_oracle!(wrapper_build_log, "build_log.bin");
wrapper_oracle!(wrapper_claude_resume, "claude_resume.bin");
wrapper_oracle!(wrapper_codex_resume, "codex_resume.bin");
wrapper_oracle!(wrapper_grok_resume, "grok_resume.bin");
wrapper_oracle!(wrapper_wide_emoji, "wide_emoji.bin");
wrapper_oracle!(wrapper_dec_scrollregion, "dec_scrollregion.bin");
wrapper_oracle!(wrapper_topregion_scroll, "topregion_scroll.bin");
