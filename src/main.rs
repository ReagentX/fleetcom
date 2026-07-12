#![forbid(unsafe_code)]

//! `fleetcom`: a fleet-view supervisor for arbitrary shell commands. Each task is
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
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    style::Print,
    terminal::{
        Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode, size, supports_keyboard_enhancement,
    },
};

use app::App;

const USAGE: &str = "\
fleetcom - a fleet-view supervisor for arbitrary shell commands

Usage:
  fleetcom [<session>]               connect to the daemon (autostarting it),
                                     optionally loading a saved session
  fleetcom --foreground [<session>]  run without a daemon; jobs die on quit
  fleetcom --kill                    kill the daemon and every job it owns
  fleetcom --daemon                  run the daemon (internal; the first
                                     fleetcom starts it automatically)

Options:
  -h, --help     print this help
  -V, --version  print the version
";

/// What a command line asks for, one variant per mutually-exclusive mode.
/// Encoding the modes as variants (not independent bools) makes conflicting
/// flags unrepresentable past the parser.
#[derive(Debug, PartialEq, Eq)]
enum Invocation {
    Help,
    Version,
    Daemon,
    Kill,
    Client {
        foreground: bool,
        session: Option<String>,
    },
}

/// Parse the command line. Errors on an unrecognized flag, a second session
/// name, or a flag combination with no coherent meaning. `--help`/`--version`
/// win over everything else, per convention.
fn parse_args(args: &[String]) -> Result<Invocation, String> {
    let (mut daemon, mut kill, mut foreground) = (false, false, false);
    let mut session: Option<String> = None;
    for a in args {
        match a.as_str() {
            "-h" | "--help" => return Ok(Invocation::Help),
            "-V" | "--version" => return Ok(Invocation::Version),
            "--daemon" => daemon = true,
            "--kill" => kill = true,
            "--foreground" => foreground = true,
            f if f.starts_with('-') => return Err(format!("unrecognized flag '{f}'")),
            name => {
                if session.is_some() {
                    return Err(format!(
                        "unexpected argument '{name}' (one session name max)"
                    ));
                }
                session = Some(name.to_string());
            }
        }
    }
    if daemon && (kill || foreground || session.is_some()) {
        return Err("--daemon takes no other arguments".to_string());
    }
    if kill && (foreground || session.is_some()) {
        return Err("--kill takes no other arguments".to_string());
    }
    match (daemon, kill) {
        (true, _) => Ok(Invocation::Daemon),
        (_, true) => Ok(Invocation::Kill),
        _ => Ok(Invocation::Client {
            foreground,
            session,
        }),
    }
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (foreground, session) = match parse_args(&args) {
        Ok(Invocation::Help) => {
            print!("{USAGE}");
            return Ok(());
        }
        Ok(Invocation::Version) => {
            println!("fleetcom {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        // Daemon mode is headless: no terminal setup, just serve the socket.
        Ok(Invocation::Daemon) => return daemon::run_daemon(),
        // `fleetcom --kill`: tell a running daemon to kill everything and exit.
        Ok(Invocation::Kill) => return daemon::run_kill(),
        Ok(Invocation::Client {
            foreground,
            session,
        }) => (foreground, session),
        Err(e) => {
            eprintln!("fleetcom: {e}");
            eprintln!("try 'fleetcom --help'");
            std::process::exit(2);
        }
    };

    install_panic_hook();

    let (cols, rows) = size()?;
    // Connect *before* raw mode / alternate screen: the handshake can wait for
    // another client to detach, and the plain terminal is where its waiting
    // notice prints readably and Ctrl-C still aborts. Failures report without
    // any restore dance. `--foreground` runs the core in-process instead.
    let mut app = if foreground {
        App::new_foreground(rows, cols)
    } else {
        match App::connect(rows, cols) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("fleetcom: could not reach the daemon: {e}");
                std::process::exit(1);
            }
        }
    };

    let mut out = io::stdout();
    enable_raw_mode()?;
    // Probe for the kitty keyboard protocol before entering the alternate
    // screen: the query round-trips through the tty, and raw mode (just
    // enabled) is what keeps the reply out of the line discipline. `false` on
    // any error: degrade to plain Enter, never to broken input.
    let kitty = supports_keyboard_enhancement().unwrap_or(false);
    execute!(out, EnterAlternateScreen, Clear(ClearType::All), Hide)?;
    // Keyboard enhancement distinguishes modified Enter; bracketed paste
    // delivers the clipboard as one event. Keyboard flags are screen-specific,
    // so enable them after entering the alternate screen. Mouse capture is
    // managed by `App::sync_input_modes`. Save and enable alternate scroll;
    // restoration occurs in `restore_terminal`.
    if kitty {
        execute!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
    }
    execute!(out, EnableBracketedPaste, Print("\x1b[?1007s\x1b[?1007h"))?;
    // `fleetcom [--foreground] <session>` loads that session at startup; the
    // result shows in the status line.
    if let Some(name) = &session {
        app.load_session(name);
    }
    install_signal_handlers(app.signal_flag())?;
    let result = app.run(&mut out);

    restore_terminal(&mut out);
    result
}

/// Restore terminal modes changed by the application. Cleanup is best-effort
/// so a failed terminal write does not prevent raw mode from being disabled.
fn restore_terminal(out: &mut io::Stdout) {
    let _ = execute!(
        out,
        PopKeyboardEnhancementFlags,
        DisableMouseCapture,
        DisableBracketedPaste,
        Print("\x1b[?1007r"),
        Show,
        LeaveAlternateScreen
    );
    let _ = disable_raw_mode();
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

/// Restore the terminal on panic. Otherwise a crash leaves the user in raw
/// mode on the alternate screen with no cursor.
fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal(&mut io::stdout());
        default(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Invocation, String> {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        parse_args(&owned)
    }

    #[test]
    fn plain_and_session_invocations() {
        assert_eq!(
            parse(&[]),
            Ok(Invocation::Client {
                foreground: false,
                session: None
            })
        );
        assert_eq!(
            parse(&["work"]),
            Ok(Invocation::Client {
                foreground: false,
                session: Some("work".into())
            })
        );
        assert_eq!(
            parse(&["--foreground", "work"]),
            Ok(Invocation::Client {
                foreground: true,
                session: Some("work".into())
            })
        );
    }

    #[test]
    fn modes_parse() {
        assert_eq!(parse(&["--daemon"]), Ok(Invocation::Daemon));
        assert_eq!(parse(&["--kill"]), Ok(Invocation::Kill));
        assert_eq!(parse(&["-h"]), Ok(Invocation::Help));
        assert_eq!(parse(&["--help"]), Ok(Invocation::Help));
        assert_eq!(parse(&["-V"]), Ok(Invocation::Version));
        assert_eq!(parse(&["--version"]), Ok(Invocation::Version));
    }

    /// A flag typo must be an error, never silently ignored: `--foregroud`
    /// silently connecting to the daemon changes what `Q` kills.
    #[test]
    fn unknown_flags_are_rejected() {
        assert!(parse(&["--foregroud"]).is_err());
        assert!(parse(&["-x"]).is_err());
        assert!(parse(&["--daemonize"]).is_err());
    }

    /// Help/version win even alongside other (even invalid) mode flags.
    #[test]
    fn help_and_version_win() {
        assert_eq!(parse(&["--daemon", "--help"]), Ok(Invocation::Help));
        assert_eq!(parse(&["--kill", "-V"]), Ok(Invocation::Version));
    }

    #[test]
    fn conflicting_modes_are_rejected() {
        assert!(parse(&["--daemon", "--kill"]).is_err());
        assert!(parse(&["--daemon", "work"]).is_err());
        assert!(parse(&["--daemon", "--foreground"]).is_err());
        assert!(parse(&["--kill", "--foreground"]).is_err());
        assert!(parse(&["--kill", "work"]).is_err());
    }

    #[test]
    fn second_session_name_is_rejected() {
        assert!(parse(&["one", "two"]).is_err());
    }
}
