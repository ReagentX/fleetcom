#![forbid(unsafe_code)]

//! `multi` — a fleet-view supervisor for arbitrary shell commands. Each task is
//! a command in its own PTY; the dashboard groups them by status, and you can
//! peek at, attach to, and background any of them.

mod app;
mod core;
mod daemon;
mod format;
mod frame;
mod path;
mod protocol;
mod session;
mod supervisor;
mod task;
mod transport;
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
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Daemon mode is headless — no terminal setup, just serve the socket.
    if args.iter().any(|a| a == "--daemon") {
        return daemon::run_daemon();
    }
    // `multi --kill`: tell a running daemon to kill everything and exit.
    if args.iter().any(|a| a == "--kill") {
        return daemon::run_kill();
    }

    install_panic_hook();

    let mut out = io::stdout();
    enable_raw_mode()?;
    execute!(out, EnterAlternateScreen, Clear(ClearType::All), Hide)?;

    let (cols, rows) = size()?;
    // Default connects to (or autostarts) the daemon so jobs outlive the UI;
    // `--foreground` runs the core in-process instead.
    let mut app = if args.iter().any(|a| a == "--foreground") {
        App::new_foreground(rows, cols)
    } else {
        match App::connect(rows, cols) {
            Ok(a) => a,
            Err(e) => {
                // Still in raw/alt-screen — restore before reporting the failure.
                let _ = execute!(out, Show, LeaveAlternateScreen);
                let _ = disable_raw_mode();
                eprintln!("multi: could not reach the daemon: {e}");
                return Err(e);
            }
        }
    };
    // `multi [--foreground] <session>` loads that session at startup; the result
    // shows in the status line. `-`-prefixed args are flags, skipped here.
    if let Some(name) = args.iter().find(|a| !a.starts_with('-')) {
        app.load_session(name);
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
