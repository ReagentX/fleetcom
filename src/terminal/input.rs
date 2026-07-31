//! The input-direction mirror of `ansi`: protocol events in, VT byte
//! sequences out. Encoding only: no PTY writes and no `Task` state;
//! callers read the child's negotiated modes under their own locks and
//! pass them in.

use crate::{
    emulator::Emulator,
    protocol::{Key, Mods, MouseKind},
};

/// The bracketed-paste terminator. Stripped from paste *content* before
/// wrapping: a clipboard that contains this sequence would otherwise end the
/// paste early and smuggle the remainder in as live keystrokes.
const PASTE_END: &[u8] = b"\x1b[201~";

/// Encode a clipboard paste using the child's DECSET 2004 state. Bracketed
/// mode wraps content and strips embedded terminators; unbracketed mode omits
/// the markers and converts CRLF and LF line endings to CR.
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

/// Encode a mouse action under the child's current terminal mode. The selected
/// protocol determines which actions are valid and how they are encoded. With
/// no mouse protocol, wheel actions become alternate-scroll arrows when the
/// alternate screen and DECSET 1007 are both active. DECSET 1007 defaults on;
/// see [`Emulator::alternate_scroll`]. Unsupported actions return `None`.
pub fn mouse_bytes(emu: &Emulator, kind: MouseKind, col: u16, row: u16) -> Option<Vec<u8>> {
    use crate::emulator::{MouseProtocolEncoding, MouseProtocolMode};
    let mode = emu.mouse_protocol_mode();
    if mode != MouseProtocolMode::None {
        // Every supported mode reports presses, releases, and wheel events;
        // only motion modes 1002 and 1003 report drags.
        if matches!(kind, MouseKind::Drag(_))
            && !matches!(
                mode,
                MouseProtocolMode::ButtonMotion | MouseProtocolMode::AnyMotion
            )
        {
            return None;
        }
        // xterm button codes: wheel 64/65; drag adds 32.
        let code: u16 = match kind {
            MouseKind::WheelUp => 64,
            MouseKind::WheelDown => 65,
            MouseKind::Press(b) | MouseKind::Release(b) => b as u16,
            MouseKind::Drag(b) => 32 + b as u16,
        };
        let release = matches!(kind, MouseKind::Release(_));
        return Some(match emu.mouse_protocol_encoding() {
            // SGR releases use the `m` suffix.
            MouseProtocolEncoding::Sgr => {
                let suffix = if release { 'm' } else { 'M' };
                format!("\x1b[<{};{};{}{}", code, col + 1, row + 1, suffix).into_bytes()
            }
            // UTF-8 fields encode `32 + value` up to 2047; releases use code 3.
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
            // Default fields are single bytes capped at 255; releases use code 3.
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
        let up = match kind {
            MouseKind::WheelUp => true,
            MouseKind::WheelDown => false,
            // Only wheel actions map to alternate-scroll arrows.
            _ => return None,
        };
        let arrow: &[u8] = match (emu.application_cursor(), up) {
            (true, true) => b"\x1bOA",
            (true, false) => b"\x1bOB",
            (false, true) => b"\x1b[A",
            (false, false) => b"\x1b[B",
        };
        return Some(arrow.repeat(3));
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

/// Encode F1–F4 as SS3 when unmodified and CSI when modified. F5–F12 use their
/// CSI numeric forms. Numbers outside `1..=12` encode to nothing.
fn f_bytes(n: u8, m: Option<u8>) -> Option<Vec<u8>> {
    if let Some(letter) = match n {
        1 => Some('P'),
        2 => Some('Q'),
        3 => Some('R'),
        4 => Some('S'),
        _ => None,
    } {
        return Some(match m {
            None => format!("\x1bO{letter}").into_bytes(),
            Some(m) => format!("\x1b[1;{m}{letter}").into_bytes(),
        });
    }
    let code = match n {
        5 => 15,
        6 => 17,
        7 => 18,
        8 => 19,
        9 => 20,
        10 => 21,
        11 => 23,
        12 => 24,
        _ => return None,
    };
    Some(match m {
        None => format!("\x1b[{code}~").into_bytes(),
        Some(m) => format!("\x1b[{code};{m}~").into_bytes(),
    })
}

/// ESC-prefix `base` when `meta` holds: the meta convention for keys with no
/// CSI modifier form. Enter takes the prefix on Shift as well as Alt, so the
/// caller passes the condition rather than `Mods`; BackTab's base is the
/// three-byte CSI Z, so `base` is a slice rather than a byte.
fn meta_bytes(meta: bool, base: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(base.len() + 1);
    if meta {
        out.push(0x1b);
    }
    out.extend_from_slice(base);
    out
}

/// Encode a key for the child. Application-cursor mode selects SS3 for
/// unmodified cursor and Home/End keys; their modified forms use CSI.
/// Unsupported key combinations return `None`.
pub fn key_bytes(app_cursor: bool, code: Key, mods: Mods) -> Option<Vec<u8>> {
    let m = mods.param();
    match code {
        Key::Char(c) => char_bytes(c, mods),
        Key::F(n) => f_bytes(n, m),
        Key::Up | Key::Down | Key::Left | Key::Right | Key::Home | Key::End => {
            let letter = match code {
                Key::Up => 'A',
                Key::Down => 'B',
                Key::Right => 'C',
                Key::Left => 'D',
                Key::Home => 'H',
                Key::End => 'F',
                _ => unreachable!(),
            };
            Some(match m {
                None if app_cursor => format!("\x1bO{letter}").into_bytes(),
                None => format!("\x1b[{letter}").into_bytes(),
                Some(m) => format!("\x1b[1;{m}{letter}").into_bytes(),
            })
        }
        Key::Insert | Key::Delete | Key::PageUp | Key::PageDown => {
            // Navigation-cluster keys always use CSI `<n>~`.
            let n = match code {
                Key::Insert => 2,
                Key::Delete => 3,
                Key::PageUp => 5,
                Key::PageDown => 6,
                _ => unreachable!(),
            };
            Some(match m {
                None => format!("\x1b[{n}~").into_bytes(),
                Some(m) => format!("\x1b[{n};{m}~").into_bytes(),
            })
        }
        // Enter uses ESC CR for Shift or Alt; Control does not change plain CR.
        Key::Enter => Some(meta_bytes(mods.shift || mods.alt, b"\x0d")),
        // Alt prefixes Tab with ESC; Control and Shift do not change HT.
        Key::Tab => Some(meta_bytes(mods.alt, b"\x09")),
        // Alt prefixes BackTab's CSI Z sequence; Control and Shift are ignored.
        Key::BackTab => Some(meta_bytes(mods.alt, b"\x1b[Z")),
        // Backspace is DEL; Alt prefixes ESC, and Control/Shift leave it unchanged.
        Key::Backspace => Some(meta_bytes(mods.alt, b"\x7f")),
        // Alt+Esc is the ESC-ESC meta form; Ctrl/Shift fold into a plain ESC.
        Key::Esc => Some(meta_bytes(mods.alt, b"\x1b")),
    }
}

#[cfg(test)]
#[path = "input_tests.rs"]
mod tests;
