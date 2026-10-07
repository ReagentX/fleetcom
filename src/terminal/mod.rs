//! Terminal reconstruction: each task's screen is rebuilt from raw PTY bytes and
//! serialized back to ANSI; `input` runs the reverse direction, encoding client
//! events into PTY bytes.

pub mod ansi;
pub mod emulator;
// Absolute pins over the recorded PTY corpus, plus the wrapper-vs-backend
// oracle.
#[cfg(test)]
mod golden;
pub mod input;
