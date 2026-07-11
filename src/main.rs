#![forbid(unsafe_code)]

//! `multi` — a fleet-view supervisor for arbitrary shell commands. Each task is
//! a command in its own PTY; the dashboard groups them by status, and you can
//! peek at, attach to, and background any of them.

mod app;
mod format;
mod session;
mod task;
mod ui;

use std::io;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crossterm::{
    cursor::{Hide, Show},
    execute,
    terminal::{
        Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode, size,
    },
};

use app::App;

fn main() -> io::Result<()> {
    install_panic_hook();

    let mut out = io::stdout();
    enable_raw_mode()?;
    execute!(out, EnterAlternateScreen, Clear(ClearType::All), Hide)?;

    let (cols, rows) = size()?;
    let mut app = App::new(rows, cols);
    // `multi <session>` loads that session at startup; the result shows in the
    // status line. `-`-prefixed args are reserved for future flags.
    if let Some(name) = std::env::args().nth(1).filter(|a| !a.starts_with('-')) {
        app.load_session(&name);
    }
    install_signal_handlers(app.signal_flag())?;
    let result = app.run(&mut out);

    // Best-effort restore: on SIGHUP the terminal is already gone, so don't let
    // a failed escape write short-circuit `disable_raw_mode`.
    let _ = execute!(out, Show, LeaveAlternateScreen);
    let _ = disable_raw_mode();
    result
}

/// Route external termination signals into the app's quit flag so the loop
/// runs its normal teardown (kill jobs, restore terminal) instead of dying in
/// raw mode. `flag::register` only stores into an atomic, so it stays within
/// `#![forbid(unsafe_code)]`.
fn install_signal_handlers(flag: Arc<AtomicBool>) -> io::Result<()> {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    signal_hook::flag::register(SIGTERM, Arc::clone(&flag))?;
    signal_hook::flag::register(SIGHUP, Arc::clone(&flag))?;
    signal_hook::flag::register(SIGINT, flag)?;
    Ok(())
}

/// Restore the terminal on panic — otherwise a crash leaves the user in raw
/// mode on the alternate screen with no cursor.
fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), Show, LeaveAlternateScreen);
        default(info);
    }));
}
