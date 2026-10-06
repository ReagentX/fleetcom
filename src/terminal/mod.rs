//! Terminal reconstruction: each task's screen is rebuilt from raw PTY bytes and
//! serialized back to ANSI; `input` runs the reverse direction, encoding client
//! events into PTY bytes.

pub(crate) mod ansi;
#[cfg(test)]
mod codex_tests;
pub(crate) mod emulator;
// Absolute pins over the recorded PTY corpus, plus the wrapper-vs-backend
// oracle.
#[cfg(test)]
mod golden;
pub(crate) mod input;
