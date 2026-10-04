//! The input-direction mirror of `ansi`: protocol events in, VT byte
//! sequences out. Encoding only: no PTY writes and no `Task` state.
//! Pass the child's negotiated modes as values to `key_bytes` and `paste_bytes`.
//! For `mouse_bytes`, hold the `Emulator` lock while reading its four mouse modes.

use crate::{
    emulator::{Emulator, MouseProtocolEncoding, MouseProtocolMode},
    protocol::{Key, Mods, MouseKind},
};

/// The bracketed-paste terminator. Stripped from paste *content* before wrapping: with
/// this sequence embedded in the clipboard text, the paste could end early and the
/// remainder be interpreted as live keystrokes.
const PASTE_END: &[u8] = b"\x1b[201~";

/// Encode a clipboard paste using the child's DECSET 2004 state. In bracketed mode,
/// wrap content and strip embedded terminators; in unbracketed mode, omit markers and
/// convert CRLF and LF line endings to CR.
pub fn paste_bytes(bracketed: bool, content: &[u8]) -> Vec<u8> {
    if bracketed {
        let mut out = Vec::with_capacity(content.len() + 2 * PASTE_END.len() + 6);
        out.extend_from_slice(b"\x1b[200~");
        let mut rest = content;
        while let Some(pos) = rest.windows(PASTE_END.len()).position(|w| w == PASTE_END) {
            out.extend_from_slice(&rest[..pos]);
            rest = &rest[pos + PASTE_END.len()..];
        }
        out.extend_from_slice(rest);
        out.extend_from_slice(PASTE_END);
        out
    } else {
        let mut out = Vec::with_capacity(content.len());
        let mut i = 0;
        while i < content.len() {
            if content[i] == b'\r' && content.get(i + 1) == Some(&b'\n') {
                out.push(b'\r');
                i += 2;
            } else if content[i] == b'\n' {
                out.push(b'\r');
                i += 1;
            } else {
                out.push(content[i]);
                i += 1;
            }
        }
        out
    }
}

/// Encode a mouse action under the child's current terminal mode. The selected protocol
/// determines which actions are valid and how they are encoded. With no mouse protocol,
/// wheel actions become alternate-scroll arrows when the alternate screen and DECSET
/// 1007 are both active. DECSET 1007 defaults on; see [`Emulator::alternate_scroll`].
/// Return `None` for unsupported actions.
pub fn mouse_bytes(emu: &Emulator, kind: MouseKind, col: u16, row: u16) -> Option<Vec<u8>> {
    let mode = emu.mouse_protocol_mode();
    if mode != MouseProtocolMode::None {
        // Report presses, releases, and wheel events in every supported mode; report
        // drags only in motion modes 1002 and 1003.
        if matches!(kind, MouseKind::Drag(_))
            && !matches!(
                mode,
                MouseProtocolMode::ButtonMotion | MouseProtocolMode::AnyMotion
            )
        {
            return None;
        }
        // xterm button codes: wheel 64/65; add 32 for drag.
        let code: u16 = match kind {
            MouseKind::WheelUp => 64,
            MouseKind::WheelDown => 65,
            MouseKind::Press(b) | MouseKind::Release(b) => b as u16,
            MouseKind::Drag(b) => 32 + b as u16,
        };
        let release = matches!(kind, MouseKind::Release(_));
        return Some(match emu.mouse_protocol_encoding() {
            // Use the `m` suffix for SGR releases.
            MouseProtocolEncoding::Sgr => {
                let suffix = if release { 'm' } else { 'M' };
                format!("\x1b[<{};{};{}{}", code, col + 1, row + 1, suffix).into_bytes()
            }
            // UTF-8 fields encode `32 + value` up to 2047; use code 3 for releases.
            MouseProtocolEncoding::Utf8 => {
                let code = if release { 3 } else { code };
                let mut out = b"\x1b[M".to_vec();
                for v in [32 + code, 33 + col.min(2014), 33 + row.min(2014)] {
                    let mut buf = [0u8; 4];
                    // Values are bounded to valid UTF-8 scalar values.
                    let c = char::from_u32(u32::from(v)).unwrap_or(' ');
                    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
                out
            }
            // Default fields are single bytes capped at 255; use code 3 for releases.
            MouseProtocolEncoding::Default => {
                let code = if release { 3 } else { code };
                vec![
                    0x1b,
                    b'[',
                    b'M',
                    32 + code as u8,
                    (33 + col.min(222)) as u8,
                    (33 + row.min(222)) as u8,
                ]
            }
        });
    }
    if emu.alternate_scroll() {
        let key = match kind {
            MouseKind::WheelUp => Key::Up,
            MouseKind::WheelDown => Key::Down,
            // Only wheel actions map to alternate-scroll arrows.
            _ => return None,
        };
        return key_bytes(emu.application_cursor(), key, Mods::default()).map(|b| b.repeat(3));
    }
    None
}

/// Return the control byte for a supported `Ctrl`+key combination. ASCII
/// letters and the standard symbol/digit aliases map to C0 control bytes.
fn ctrl_byte(c: char) -> Option<u8> {
    if c.is_ascii_alphabetic() {
        return Some((c.to_ascii_uppercase() as u8) & 0x1f);
    }
    Some(match c {
        ' ' | '@' | '2' => 0x00,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' => 0x1e,
        '_' | '7' | '/' => 0x1f,
        '?' | '8' => 0x7f,
        _ => return None,
    })
}

/// Encode a printable key. Shift is already folded into `c` by the client, so
/// it is ignored here; only `ctrl` (control byte) and `alt` (ESC prefix, the
/// meta convention) change the bytes.
fn char_bytes(c: char, mods: Mods) -> Option<Vec<u8>> {
    let mut out = if mods.ctrl {
        vec![ctrl_byte(c)?]
    } else {
        let mut buf = [0u8; 4];
        c.encode_utf8(&mut buf).as_bytes().to_vec()
    };
    if mods.alt {
        out.insert(0, 0x1b);
    }
    Some(out)
}

/// Encode cursor keys, Home/End, and F1–F4 with a final letter. For unmodified
/// keys, use SS3 when `ss3` is true and CSI otherwise. With modifiers, use
/// CSI `1;{m}{letter}`.
fn letter_bytes(ss3: bool, letter: char, m: Option<u8>) -> Vec<u8> {
    match m {
        None if ss3 => format!("\x1bO{letter}"),
        None => format!("\x1b[{letter}"),
        Some(m) => format!("\x1b[1;{m}{letter}"),
    }
    .into_bytes()
}

/// Encode the navigation cluster and F5–F12 as CSI `n~`, or CSI `n;m~`
/// with modifiers.
fn tilde_bytes(n: u8, m: Option<u8>) -> Vec<u8> {
    match m {
        None => format!("\x1b[{n}~"),
        Some(m) => format!("\x1b[{n};{m}~"),
    }
    .into_bytes()
}

/// Encode F1–F4 as SS3 when unmodified and CSI when modified. F5–F12 use their CSI
/// numeric forms. Emit nothing for numbers outside `1..=12`.
fn f_bytes(n: u8, m: Option<u8>) -> Option<Vec<u8>> {
    Some(match n {
        1 => letter_bytes(true, 'P', m),
        2 => letter_bytes(true, 'Q', m),
        3 => letter_bytes(true, 'R', m),
        4 => letter_bytes(true, 'S', m),
        5 => tilde_bytes(15, m),
        6 => tilde_bytes(17, m),
        7 => tilde_bytes(18, m),
        8 => tilde_bytes(19, m),
        9 => tilde_bytes(20, m),
        10 => tilde_bytes(21, m),
        11 => tilde_bytes(23, m),
        12 => tilde_bytes(24, m),
        _ => return None,
    })
}

/// Prefix `base` with ESC when `meta` is set. `base` may be a multibyte
/// sequence, such as BackTab's CSI Z.
fn meta_bytes(meta: bool, base: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(base.len() + 1);
    if meta {
        out.push(0x1b);
    }
    out.extend_from_slice(base);
    out
}

/// Return xterm's modifier parameter `1 + shift + 2·alt + 4·ctrl`, or
/// `None` when no modifier is held.
fn mod_param(mods: Mods) -> Option<u8> {
    let bits = mods.shift as u8 + 2 * mods.alt as u8 + 4 * mods.ctrl as u8;
    (bits != 0).then_some(1 + bits)
}

/// Encode a key for the child. In application-cursor mode, use SS3 for unmodified
/// cursor and Home/End keys and CSI for modified forms. Return `None` for unsupported
/// key combinations.
pub fn key_bytes(app_cursor: bool, code: Key, mods: Mods) -> Option<Vec<u8>> {
    let m = mod_param(mods);
    match code {
        Key::Char(c) => char_bytes(c, mods),
        Key::F(n) => f_bytes(n, m),
        Key::Up => Some(letter_bytes(app_cursor, 'A', m)),
        Key::Down => Some(letter_bytes(app_cursor, 'B', m)),
        Key::Right => Some(letter_bytes(app_cursor, 'C', m)),
        Key::Left => Some(letter_bytes(app_cursor, 'D', m)),
        Key::Home => Some(letter_bytes(app_cursor, 'H', m)),
        Key::End => Some(letter_bytes(app_cursor, 'F', m)),
        // Encode navigation-cluster keys with CSI `<n>~`.
        Key::Insert => Some(tilde_bytes(2, m)),
        Key::Delete => Some(tilde_bytes(3, m)),
        Key::PageUp => Some(tilde_bytes(5, m)),
        Key::PageDown => Some(tilde_bytes(6, m)),
        // Encode Enter as ESC CR with Shift or Alt; keep plain CR with Control.
        Key::Enter => Some(meta_bytes(mods.shift || mods.alt, b"\x0d")),
        // Prefix Tab with ESC for Alt; keep HT with Control or Shift.
        Key::Tab => Some(meta_bytes(mods.alt, b"\x09")),
        // Prefix BackTab's CSI Z sequence with ESC for Alt; ignore Control and Shift.
        Key::BackTab => Some(meta_bytes(mods.alt, b"\x1b[Z")),
        // Encode Backspace as DEL; prefix ESC for Alt and ignore Control/Shift.
        Key::Backspace => Some(meta_bytes(mods.alt, b"\x7f")),
        // Encode Alt+Esc as ESC-ESC; use plain ESC with Ctrl/Shift.
        Key::Esc => Some(meta_bytes(mods.alt, b"\x1b")),
    }
}

#[cfg(test)]
#[path = "input_tests.rs"]
mod tests;
