//! Terminal reconstruction: each task's screen is rebuilt from raw PTY bytes,
//! serialized back to ANSI, and framed over the wire; `input` runs the reverse
//! direction, encoding client events into PTY bytes.

pub(crate) mod ansi;
pub(crate) mod emulator;
pub(crate) mod frame;
// Differential emulator tests over recorded PTY output.
#[cfg(test)]
mod golden;
pub(crate) mod input;
