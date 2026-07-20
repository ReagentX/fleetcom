//! Terminal reconstruction: each task's screen is rebuilt from raw PTY bytes,
//! serialized back to ANSI, framed over the wire, and formatted for display.

pub(crate) mod ansi;
pub(crate) mod emulator;
pub(crate) mod format;
pub(crate) mod frame;
