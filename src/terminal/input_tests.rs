use super::*;
use crate::protocol::MouseBtn;

/// Paste encoding follows the child's DECSET 2004 opt-in: markers only
/// when asked for, newline→CR conversion only when not.
#[test]
fn paste_wraps_only_when_child_opted_in() {
    assert_eq!(
        paste_bytes(true, b"hello"),
        b"\x1b[200~hello\x1b[201~".to_vec()
    );
    // Inside brackets the content rides verbatim: the child's own paste
    // handling decides what a newline means.
    assert_eq!(
        paste_bytes(true, b"a\nb"),
        b"\x1b[200~a\nb\x1b[201~".to_vec()
    );
    assert_eq!(paste_bytes(false, b"hello"), b"hello".to_vec());
}

/// A clipboard containing the end marker must not terminate the paste
/// early: the remainder would arrive as live keystrokes.
#[test]
fn paste_strips_embedded_terminator() {
    assert_eq!(
        paste_bytes(true, b"safe\x1b[201~rm -rf /\n"),
        b"\x1b[200~saferm -rf /\n\x1b[201~".to_vec()
    );
    // Multiple embedded markers all go.
    assert_eq!(
        paste_bytes(true, b"\x1b[201~a\x1b[201~b\x1b[201~"),
        b"\x1b[200~ab\x1b[201~".to_vec()
    );
}

/// Unbracketed paste converts both `\r\n` and bare `\n` to the `\r` Enter
/// sends, without doubling a CRLF into two returns.
#[test]
fn legacy_paste_converts_line_endings() {
    assert_eq!(paste_bytes(false, b"a\r\nb\nc\r"), b"a\rb\rc\r".to_vec());
}

/// Wheel routing follows the child's own escape sequences: nothing for an
/// inline child, alternate-scroll arrows for a full-screen one, real mouse
/// events once a protocol is requested, in the negotiated encoding.
#[test]
fn wheel_routes_by_child_state() {
    let up = MouseKind::WheelUp;
    let down = MouseKind::WheelDown;
    let mut p = Emulator::new(24, 80, 0);
    // Inline child, no mouse: dropped, not translated into arrow spam.
    assert_eq!(mouse_bytes(&p, up, 0, 0), None);
    // Full-screen child: three arrows per notch, normal cursor keys.
    p.process(b"\x1b[?1049h");
    assert_eq!(
        mouse_bytes(&p, up, 0, 0),
        Some(b"\x1b[A\x1b[A\x1b[A".to_vec())
    );
    // Clicks mean nothing to a full-screen child without a mouse mode.
    assert_eq!(
        mouse_bytes(&p, MouseKind::Press(MouseBtn::Left), 0, 0),
        None
    );
    // Application cursor keys switch the arrows to SS3 form.
    p.process(b"\x1b[?1h");
    assert_eq!(
        mouse_bytes(&p, down, 0, 0),
        Some(b"\x1bOB\x1bOB\x1bOB".to_vec())
    );
    // SGR mouse protocol: a real wheel event, 1-based coordinates.
    p.process(b"\x1b[?1000h\x1b[?1006h");
    assert_eq!(mouse_bytes(&p, up, 4, 2), Some(b"\x1b[<64;5;3M".to_vec()));
    // Default encoding: single-byte cells, clamped to fit.
    p.process(b"\x1b[?1006l");
    assert_eq!(
        mouse_bytes(&p, down, 0, 0),
        Some(vec![0x1b, b'[', b'M', 32 + 65, 33, 33])
    );
    assert_eq!(
        mouse_bytes(&p, down, 500, 500),
        Some(vec![0x1b, b'[', b'M', 32 + 65, 255, 255])
    );
    // UTF-8 mouse coordinates can use multiple bytes.
    p.process(b"\x1b[?1005h");
    assert_eq!(
        mouse_bytes(&p, up, 200, 2),
        Some(vec![0x1b, b'[', b'M', 32 + 64, 0xc3, 0xa9, 33 + 2])
    );
    // UTF-8 mouse coordinates cap at the protocol limit.
    assert_eq!(
        mouse_bytes(&p, up, 5000, 5000),
        Some(vec![0x1b, b'[', b'M', 32 + 64, 0xdf, 0xbf, 0xdf, 0xbf])
    );
}

/// A full-screen child receives wheel arrows only while DECSET 1007 is
/// enabled; the mode defaults on.
#[test]
fn wheel_arrows_honor_decset_1007() {
    let up = MouseKind::WheelUp;
    let mut p = Emulator::new(24, 80, 0);
    p.process(b"\x1b[?1049h\x1b[?1007l");
    assert_eq!(mouse_bytes(&p, up, 0, 0), None, "1007 off: no arrows");
    p.process(b"\x1b[?1007h");
    assert_eq!(
        mouse_bytes(&p, up, 0, 0),
        Some(b"\x1b[A\x1b[A\x1b[A".to_vec()),
        "1007 back on: arrows resume"
    );
    // A mouse protocol still outranks the gate: real wheel events.
    p.process(b"\x1b[?1000h\x1b[?1006h");
    assert_eq!(mouse_bytes(&p, up, 0, 0), Some(b"\x1b[<64;1;1M".to_vec()));
}

/// DECSET 1000/1002/1003 all report presses, releases, and wheel events;
/// only motion modes 1002 and 1003 report drags. SGR marks releases with
/// the `m` suffix and preserves the button code; the default and UTF-8
/// encodings use code 3 for every release.
#[test]
fn buttons_respect_mode_granularity_and_encoding() {
    let press = MouseKind::Press(MouseBtn::Left);
    let drag = MouseKind::Drag(MouseBtn::Left);
    let release = MouseKind::Release(MouseBtn::Left);
    let wheel = MouseKind::WheelUp;

    for (mode, drags) in [(1000, false), (1002, true), (1003, true)] {
        let mut p = Emulator::new(24, 80, 0);
        p.process(format!("\x1b[?{mode}h").as_bytes());

        // Default encoding: single-byte fields.
        assert_eq!(
            mouse_bytes(&p, press, 4, 2),
            Some(vec![0x1b, b'[', b'M', 32, 33 + 4, 33 + 2]),
            "mode {mode}: default press"
        );
        assert_eq!(
            mouse_bytes(&p, release, 4, 2),
            Some(vec![0x1b, b'[', b'M', 32 + 3, 33 + 4, 33 + 2]),
            "mode {mode}: default release"
        );
        assert_eq!(
            mouse_bytes(&p, wheel, 4, 2),
            Some(vec![0x1b, b'[', b'M', 32 + 64, 33 + 4, 33 + 2]),
            "mode {mode}: default wheel"
        );
        assert_eq!(
            mouse_bytes(&p, drag, 4, 2),
            drags.then(|| vec![0x1b, b'[', b'M', 32 + 32, 33 + 4, 33 + 2]),
            "mode {mode}: default drag"
        );

        // UTF-8 encoding: same codes, multi-byte coordinates.
        p.process(b"\x1b[?1005h");
        assert_eq!(
            mouse_bytes(&p, press, 200, 2),
            Some(vec![0x1b, b'[', b'M', 32, 0xc3, 0xa9, 33 + 2]),
            "mode {mode}: utf8 press"
        );
        assert_eq!(
            mouse_bytes(&p, release, 200, 2),
            Some(vec![0x1b, b'[', b'M', 32 + 3, 0xc3, 0xa9, 33 + 2]),
            "mode {mode}: utf8 release"
        );
        assert_eq!(
            mouse_bytes(&p, wheel, 200, 2),
            Some(vec![0x1b, b'[', b'M', 32 + 64, 0xc3, 0xa9, 33 + 2]),
            "mode {mode}: utf8 wheel"
        );
        assert_eq!(
            mouse_bytes(&p, drag, 200, 2),
            drags.then(|| vec![0x1b, b'[', b'M', 32 + 32, 0xc3, 0xa9, 33 + 2]),
            "mode {mode}: utf8 drag"
        );

        // SGR encoding: parameterized fields, release keeps its code.
        p.process(b"\x1b[?1006h");
        assert_eq!(
            mouse_bytes(&p, press, 4, 2),
            Some(b"\x1b[<0;5;3M".to_vec()),
            "mode {mode}: sgr press"
        );
        assert_eq!(
            mouse_bytes(&p, release, 4, 2),
            Some(b"\x1b[<0;5;3m".to_vec()),
            "mode {mode}: sgr release"
        );
        assert_eq!(
            mouse_bytes(&p, wheel, 4, 2),
            Some(b"\x1b[<64;5;3M".to_vec()),
            "mode {mode}: sgr wheel"
        );
        assert_eq!(
            mouse_bytes(&p, drag, 4, 2),
            drags.then(|| b"\x1b[<32;5;3M".to_vec()),
            "mode {mode}: sgr drag"
        );
    }
}

fn mods(shift: bool, alt: bool, ctrl: bool) -> Mods {
    Mods { shift, alt, ctrl }
}

/// Cursor and Home/End keys: application-cursor mode picks SS3 vs CSI for
/// the unmodified sequence, and any modifier forces the CSI `1;m` form even
/// in application mode.
#[test]
fn cursor_keys_encode_by_mode_and_modifier() {
    let none = Mods::default();
    for (code, l) in [
        (Key::Up, 'A'),
        (Key::Down, 'B'),
        (Key::Right, 'C'),
        (Key::Left, 'D'),
        (Key::Home, 'H'),
        (Key::End, 'F'),
    ] {
        assert_eq!(
            key_bytes(false, code, none),
            Some(format!("\x1b[{l}").into_bytes()),
            "{code:?} normal",
        );
        assert_eq!(
            key_bytes(true, code, none),
            Some(format!("\x1bO{l}").into_bytes()),
            "{code:?} app-cursor",
        );
        assert_eq!(
            key_bytes(true, code, mods(false, true, false)),
            Some(format!("\x1b[1;3{l}").into_bytes()),
            "{code:?} alt forces CSI even in app mode",
        );
    }
}

/// The modifier parameter is `1 + shift + 2·alt + 4·ctrl`: shift=2, alt=3,
/// ctrl=5, ctrl+alt=7, all-three=8.
#[test]
fn modifier_param_formula() {
    for (m, digit) in [
        (mods(true, false, false), '2'),
        (mods(false, true, false), '3'),
        (mods(false, false, true), '5'),
        (mods(false, true, true), '7'),
        (mods(true, true, true), '8'),
    ] {
        assert_eq!(
            key_bytes(false, Key::Up, m),
            Some(format!("\x1b[1;{digit}A").into_bytes()),
            "param for {m:?}",
        );
    }
}

/// Application-cursor mode uses SS3 only for unmodified cursor keys.
#[test]
fn app_cursor_drives_unmodified_only() {
    assert_eq!(
        key_bytes(true, Key::Left, mods(false, true, false)),
        Some(b"\x1b[1;3D".to_vec()),
    );
    assert_eq!(
        key_bytes(true, Key::Up, Mods::default()),
        Some(b"\x1bOA".to_vec()),
    );
}

/// The full F1–F12 table, including the terminfo gaps (no 16 between
/// F5=15 and F6=17; no 22 before F11=23) and the modified forms.
#[test]
fn function_keys_cover_the_terminfo_gaps() {
    let none = Mods::default();
    for (n, seq) in [
        (1u8, b"\x1bOP".to_vec()),
        (2, b"\x1bOQ".to_vec()),
        (3, b"\x1bOR".to_vec()),
        (4, b"\x1bOS".to_vec()),
        (5, b"\x1b[15~".to_vec()),
        (6, b"\x1b[17~".to_vec()),
        (7, b"\x1b[18~".to_vec()),
        (8, b"\x1b[19~".to_vec()),
        (9, b"\x1b[20~".to_vec()),
        (10, b"\x1b[21~".to_vec()),
        (11, b"\x1b[23~".to_vec()),
        (12, b"\x1b[24~".to_vec()),
    ] {
        assert_eq!(key_bytes(false, Key::F(n), none), Some(seq), "F{n}");
    }
    // F1–F4 collapse to CSI `1;m`; F5–F12 splice m before the tilde.
    assert_eq!(
        key_bytes(false, Key::F(1), mods(true, false, false)),
        Some(b"\x1b[1;2P".to_vec()),
    );
    assert_eq!(
        key_bytes(false, Key::F(5), mods(false, false, true)),
        Some(b"\x1b[15;5~".to_vec()),
    );
    assert_eq!(
        key_bytes(false, Key::F(12), mods(false, true, false)),
        Some(b"\x1b[24;3~".to_vec()),
    );
    assert_eq!(key_bytes(false, Key::F(0), none), None);
    assert_eq!(key_bytes(false, Key::F(13), none), None);
}

/// The Insert/Delete/PageUp/PageDown cluster is CSI `<n>~` regardless of
/// application-cursor mode.
#[test]
fn nav_cluster_is_mode_independent() {
    for (code, n) in [
        (Key::Insert, 2),
        (Key::Delete, 3),
        (Key::PageUp, 5),
        (Key::PageDown, 6),
    ] {
        assert_eq!(
            key_bytes(false, code, Mods::default()),
            Some(format!("\x1b[{n}~").into_bytes()),
            "{code:?} normal",
        );
        assert_eq!(
            key_bytes(true, code, Mods::default()),
            Some(format!("\x1b[{n}~").into_bytes()),
            "{code:?} app-cursor unchanged",
        );
        assert_eq!(
            key_bytes(false, code, mods(false, false, true)),
            Some(format!("\x1b[{n};5~").into_bytes()),
            "{code:?} modified",
        );
    }
}

/// Supported Ctrl symbol/digit aliases produce their C0 control bytes.
#[test]
fn ctrl_symbol_and_digit_table() {
    let ctrl = mods(false, false, true);
    for (c, byte) in [
        (' ', 0x00),
        ('@', 0x00),
        ('2', 0x00),
        ('[', 0x1b),
        ('3', 0x1b),
        ('\\', 0x1c),
        ('4', 0x1c),
        (']', 0x1d),
        ('5', 0x1d),
        ('^', 0x1e),
        ('6', 0x1e),
        ('_', 0x1f),
        ('7', 0x1f),
        ('/', 0x1f),
        ('?', 0x7f),
        ('8', 0x7f),
    ] {
        assert_eq!(
            key_bytes(false, Key::Char(c), ctrl),
            Some(vec![byte]),
            "Ctrl+{c:?}",
        );
    }
    assert_eq!(key_bytes(false, Key::Char('1'), ctrl), None);
    assert_eq!(key_bytes(false, Key::Char('9'), ctrl), None);
}

/// Char encodings: plain UTF-8 (multibyte preserved), shift folded into the
/// char, Alt as an ESC prefix, and Ctrl+letter folding to its C0 control.
#[test]
fn char_alt_and_ctrl_letters() {
    let none = Mods::default();
    assert_eq!(key_bytes(false, Key::Char('a'), none), Some(b"a".to_vec()));
    assert_eq!(
        key_bytes(false, Key::Char('é'), none),
        Some("é".as_bytes().to_vec()),
    );
    // Shift is already in the char; on its own it changes nothing.
    assert_eq!(
        key_bytes(false, Key::Char('A'), mods(true, false, false)),
        Some(b"A".to_vec()),
    );
    assert_eq!(
        key_bytes(false, Key::Char('x'), mods(false, true, false)),
        Some(b"\x1bx".to_vec()),
    );
    assert_eq!(
        key_bytes(false, Key::Char('a'), mods(false, false, true)),
        Some(vec![0x01]),
    );
    assert_eq!(
        key_bytes(false, Key::Char('C'), mods(false, false, true)),
        Some(vec![0x03]),
    );
    assert_eq!(
        key_bytes(false, Key::Char('z'), mods(false, false, true)),
        Some(vec![0x1a]),
    );
    assert_eq!(
        key_bytes(false, Key::Char('c'), mods(false, true, true)),
        Some(vec![0x1b, 0x03]),
    );
}

/// Named keys and their modifier forms: keys with no distinct modified
/// encoding ignore an unsupported Ctrl/Shift (base sequence, never dropped)
/// and take the ESC-prefix meta form under Alt.
#[test]
fn named_keys_and_meta_prefixes() {
    let none = Mods::default();
    assert_eq!(key_bytes(false, Key::Enter, none), Some(vec![0x0d]));
    assert_eq!(key_bytes(false, Key::Tab, none), Some(vec![0x09]));
    assert_eq!(
        key_bytes(false, Key::BackTab, none),
        Some(b"\x1b[Z".to_vec())
    );
    assert_eq!(key_bytes(false, Key::Backspace, none), Some(vec![0x7f]));
    assert_eq!(key_bytes(false, Key::Esc, none), Some(vec![0x1b]));
    // Shift or Alt Enter -> ESC CR; Alt+Backspace -> ESC DEL.
    assert_eq!(
        key_bytes(false, Key::Enter, mods(true, false, false)),
        Some(b"\x1b\r".to_vec()),
    );
    assert_eq!(
        key_bytes(false, Key::Enter, mods(false, true, false)),
        Some(b"\x1b\r".to_vec()),
    );
    assert_eq!(
        key_bytes(false, Key::Backspace, mods(false, true, false)),
        Some(b"\x1b\x7f".to_vec()),
    );
    // BackTab already is Shift+Tab: its inherent Shift is ignored; Alt
    // meta-prefixes the CSI Z sequence.
    assert_eq!(
        key_bytes(false, Key::BackTab, mods(true, false, false)),
        Some(b"\x1b[Z".to_vec()),
    );
    assert_eq!(
        key_bytes(false, Key::BackTab, mods(false, true, false)),
        Some(b"\x1b\x1b[Z".to_vec()),
    );
    // These keys have no distinct modified form: an unsupported Ctrl/Shift
    // is ignored (base sequence, never dropped), and Alt is the ESC-prefix
    // meta form.
    assert_eq!(
        key_bytes(false, Key::Enter, mods(false, false, true)),
        Some(vec![0x0d]),
        "Ctrl+Enter folds to CR",
    );
    assert_eq!(
        key_bytes(false, Key::Backspace, mods(false, false, true)),
        Some(vec![0x7f]),
        "Ctrl+Backspace folds to DEL",
    );
    assert_eq!(
        key_bytes(false, Key::Tab, mods(false, false, true)),
        Some(vec![0x09]),
        "Ctrl+Tab folds to HT",
    );
    assert_eq!(
        key_bytes(false, Key::Tab, mods(false, true, false)),
        Some(vec![0x1b, 0x09]),
        "Alt+Tab is ESC TAB",
    );
    assert_eq!(
        key_bytes(false, Key::Esc, mods(false, true, false)),
        Some(vec![0x1b, 0x1b]),
        "Alt+Esc is ESC ESC",
    );
    assert_eq!(
        key_bytes(false, Key::Esc, mods(false, false, true)),
        Some(vec![0x1b]),
        "Ctrl+Esc folds to ESC",
    );
}
