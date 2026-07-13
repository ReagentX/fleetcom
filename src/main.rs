#![forbid(unsafe_code)]

//! `fleetcom`: a fleet-view supervisor for arbitrary shell commands. Each task is
//! a command in its own PTY; the dashboard groups them by status, and you can
//! peek at, attach to, and background any of them.

mod app;
mod core;
mod daemon;
mod emulator;
mod format;
mod frame;
// Differential golden suites over the PTY corpus (migration step 3).
#[cfg(test)]
mod golden;
mod path;
mod protocol;
mod serialize;
mod session;
mod supervisor;
mod task;
mod transport;
mod ui;

use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

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

/// Whether `PushKeyboardEnhancementFlags` actually executed, so restore pops
/// only what setup pushed. A static atomic because the panic hook is installed
/// before the terminal is touched and can fire on any thread; the hook and
/// `TerminalGuard` read the same source of truth. Only atomicity matters here
/// (the flag orders nothing else), so `Relaxed` suffices.
static KITTY_PUSHED: AtomicBool = AtomicBool::new(false);

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
    // Armed the moment raw mode is on: every exit past this point (the `?`s
    // below, a panic unwind, the normal return) must restore the terminal,
    // or the shell is left in raw mode with a hidden cursor. The panic hook
    // covers panics only, not `Err` returns.
    let guard = TerminalGuard;
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
        // Recorded only after the execute succeeds: restore mirrors what
        // setup actually did, not what it attempted.
        KITTY_PUSHED.store(true, Ordering::Relaxed);
    }
    execute!(out, EnableBracketedPaste, Print("\x1b[?1007s\x1b[?1007h"))?;
    // `fleetcom [--foreground] <session>` loads that session at startup; the
    // result shows in the status line.
    if let Some(name) = &session {
        app.load_session(name);
    }
    install_signal_handlers(app.signal_flag())?;
    let result = app.run(&mut out);

    // Consuming the guard restores here, at the same point the happy path
    // always restored; the drop-on-unwind path exists for the `?`s above.
    drop(guard);
    result
}

/// Restores the terminal when dropped. Constructed only after
/// `enable_raw_mode` succeeds: before that there is nothing to undo.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal(&mut io::stdout());
    }
}

/// Restore terminal modes changed by the application. Cleanup is best-effort
/// so a failed terminal write does not prevent raw mode from being disabled.
///
/// May run twice: on a panic the hook restores first (so the message prints
/// on the normal screen), then `TerminalGuard`'s drop restores again during
/// unwind. Every step tolerates the repeat (leaving the alternate screen
/// twice, disabling raw mode twice, and popping past an empty kitty stack are
/// all no-ops or ignored), so don't "fix" the double restore by dropping one.
fn restore_terminal(out: &mut io::Stdout) {
    let _ = emit_restore_sequences(out, KITTY_PUSHED.load(Ordering::Relaxed));
    let _ = disable_raw_mode();
}

/// Emit the escape sequences that undo terminal setup, mirroring what setup
/// actually did: the keyboard-enhancement pop only if the flags were pushed.
/// `disable_raw_mode` lives in `restore_terminal`, not here: it mutates
/// process-global tty state, and this function stays a pure emission so tests
/// can drive it against a buffer.
fn emit_restore_sequences(out: &mut impl io::Write, kitty_pushed: bool) -> io::Result<()> {
    if kitty_pushed {
        execute!(out, PopKeyboardEnhancementFlags)?;
    }
    execute!(
        out,
        DisableMouseCapture,
        DisableBracketedPaste,
        Print("\x1b[?1007r"),
        Show,
        LeaveAlternateScreen
    )
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
/// mode on the alternate screen with no cursor. Restoring *before* the
/// default hook is what puts the panic message on the normal screen instead
/// of the vanishing alternate screen.
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

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    /// Render a single crossterm command to bytes, so the assertions below
    /// track crossterm's actual encoding instead of hardcoding it.
    fn encode(cmd: impl crossterm::Command) -> Vec<u8> {
        let mut buf = Vec::new();
        execute!(buf, cmd).unwrap();
        buf
    }

    /// Restore must mirror setup: the keyboard-enhancement pop is emitted
    /// only when the flags were pushed, and its absence must not take the
    /// rest of the restore down with it.
    #[test]
    fn restore_pops_kitty_flags_only_when_pushed() {
        let pop = encode(PopKeyboardEnhancementFlags);

        let mut pushed = Vec::new();
        emit_restore_sequences(&mut pushed, true).unwrap();
        let mut unpushed = Vec::new();
        emit_restore_sequences(&mut unpushed, false).unwrap();

        assert!(contains(&pushed, &pop));
        assert!(!contains(&unpushed, &pop));

        // Both variants still emit the full remaining restore.
        let leave = encode(LeaveAlternateScreen);
        let show = encode(Show);
        for out in [&pushed, &unpushed] {
            assert!(contains(out, &leave));
            assert!(contains(out, &show));
            // Alternate-scroll restore is a raw Print, not a crossterm command.
            assert!(contains(out, b"\x1b[?1007r"));
        }
    }
}
