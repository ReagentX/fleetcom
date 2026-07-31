//! Terminal reconstruction: each task's screen is rebuilt from raw PTY bytes and
//! serialized back to ANSI; `input` runs the reverse direction, encoding client
//! events into PTY bytes.

pub(crate) mod ansi;
pub(crate) mod emulator;
// Differential emulator tests over recorded PTY output.
#[cfg(test)]
mod golden;
pub(crate) mod input;
