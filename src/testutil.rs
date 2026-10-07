//! Test helpers shared by in-source modules: scratch directories, deadline
//! polling, child processes and launch environments, stub executables (resident
//! shells and fake notifiers), Codex rollout and Claude hook fixtures, neutral
//! `ScreenView` values, and row builders. Compile this module only under
//! `#[cfg(test)]` in `main.rs` so these helpers stay out of builds.

use std::{
    ffi::OsString,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Once,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use alacritty_terminal::{
    Term,
    event::VoidListener,
    term::{Config, test::TermSize},
    vte::ansi::Processor,
};

use crate::{
    emulator::Emulator,
    protocol::ScreenView,
    task::{pid_is_dead, positive_pid},
};

/// Versioned prefix for scratch directories eligible for sweeping.
const SCRATCH_PREFIX: &str = "fleetcom_test2_";

/// Scratch directory removed on drop. A panic preserves it for inspection;
/// later runs reclaim it after the owner exits.
pub struct Scratch(PathBuf);

impl std::ops::Deref for Scratch {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Create an empty `<prefix><tag>_<pid>_<seq>` directory under the system temp
/// directory. The PID separates processes; the sequence separates calls.
pub fn temp(tag: &str) -> Scratch {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    sweep_dead_scratch();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!(
        "{SCRATCH_PREFIX}{tag}_{}_{seq}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    Scratch(d)
}

/// Once per process, remove scratch directories owned by dead processes.
fn sweep_dead_scratch() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let Ok(entries) = fs::read_dir(std::env::temp_dir()) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name
                .to_str()
                .and_then(scratch_pid_of)
                .is_some_and(pid_is_dead)
            {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    });
}

/// Parse the owner PID from a scratch name in the current namespace.
fn scratch_pid_of(name: &str) -> Option<i32> {
    scratch_pid(name.strip_prefix(SCRATCH_PREFIX)?)
}

/// Parse the `<pid>` from a `<tag>_<pid>_<seq>` scratch suffix. Tags contain
/// underscores, so both trailing fields are read from the right.
fn scratch_pid(suffix: &str) -> Option<i32> {
    let (rest, seq) = suffix.rsplit_once('_')?;
    let (_, pid) = rest.rsplit_once('_')?;
    if seq.is_empty() || !seq.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    positive_pid(pid)
}

/// Parse valid suffixes and reject malformed PID or sequence fields.
#[test]
fn scratch_pid_reads_the_pid_field() {
    assert_eq!(scratch_pid("tag_with_underscores_123_4"), Some(123));
    assert_eq!(scratch_pid("session_mode_7_0"), Some(7));
    // Both trailing fields must be decimal integers.
    assert_eq!(scratch_pid("tag_with_underscores_x_4"), None);
    assert_eq!(scratch_pid("tag_123_x"), None);
    // A suffix without all three components is invalid.
    assert_eq!(scratch_pid("tag_123"), None);
    assert_eq!(scratch_pid("tag_0_4"), None);
    assert_eq!(scratch_pid("tag_+123_4"), None);
}

/// Names outside the current namespace are ineligible for sweeping.
#[test]
fn legacy_scratch_names_are_rejected_by_the_prefix() {
    assert_eq!(scratch_pid_of("fleetcom_test_app_1007_456"), None);
    assert_eq!(scratch_pid_of("fleetcom_test_tag_123"), None);
    // Parse the PID field even when the tag ends in digits.
    assert_eq!(scratch_pid_of("fleetcom_test2_app_1007_456_0"), Some(456));
    assert_eq!(scratch_pid_of("unrelated_dir_123_4"), None);
}

/// Poll `pred` until it holds or `budget` elapses; returns the final answer.
/// `pred` always runs at least once.
pub fn wait_until(budget: Duration, mut pred: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        if pred() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Read a pid a test task wrote, waiting for the write to land.
pub fn read_pid(path: &Path) -> nix::unistd::Pid {
    let mut pid = None;
    wait_until(Duration::from_secs(5), || {
        pid = fs::read_to_string(path)
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok());
        pid.is_some()
    });
    nix::unistd::Pid::from_raw(pid.expect("pid file never appeared"))
}

/// Return the PID of a child process after reaping it.
pub fn dead_pid() -> u32 {
    let mut child = Command::new("sh").arg("-c").arg("exit 0").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

/// Return this process's working directory.
pub fn here() -> PathBuf {
    std::env::current_dir().unwrap()
}

/// Snapshot this process's environment for a launch context.
pub fn env_here() -> Vec<(OsString, OsString)> {
    std::env::vars_os().collect()
}

/// Return `env` with inherited `SHELL` entries removed and `shell` as the sole
/// value passed to the launch.
pub fn with_shell(
    mut env: Vec<(OsString, OsString)>,
    shell: impl Into<OsString>,
) -> Vec<(OsString, OsString)> {
    env.retain(|(k, _)| k != "SHELL");
    env.push(("SHELL".into(), shell.into()));
    env
}

/// Return this process's environment with `SHELL=/bin/sh` for predictable
/// background-job behavior in process-group tests.
pub fn sh_env() -> Vec<(OsString, OsString)> {
    with_shell(env_here(), "/bin/sh")
}

/// Write an executable `#!/bin/sh` script at `path`. The parent must exist.
pub fn write_executable(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

/// Install a resident shell at `<dir>/resident-sh` and return its path: it runs its `-c`
/// text in a child `sh` and stays the task leader, as tcsh and csh do. The trailing `:`
/// keeps it resident: `sh` execs a script's last simple command in its own place, which
/// would hand the child the leader PID. Set it as `SHELL` to verify that a managed launch
/// bypasses the shell.
pub fn install_resident_shell(dir: &Path) -> PathBuf {
    let shell = dir.join("resident-sh");
    write_executable(&shell, "[ \"$1\" = -c ] || exit 2\n/bin/sh -c \"$2\"\n:");
    shell
}

/// Install a fake notifier at `path` that records its argv, one token
/// per line, into `record`.
pub fn install_fake_notifier(path: &Path, record: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    write_executable(
        path,
        &format!("printf '%s\\n' \"$@\" > '{}'", record.display()),
    );
}

/// First line of a Codex rollout: the `session_meta` envelope around
/// `payload`, newline-terminated like every rollout line.
pub fn codex_session_meta(payload: &str) -> String {
    format!(
        r#"{{"timestamp":"2026-10-04T17:49:56.012Z","ordinal":0,"type":"session_meta","payload":{payload}}}"#
    ) + "\n"
}

/// Write `contents` as the rollout of `thread` under the Codex `home` and
/// return its path.
pub fn install_codex_rollout(home: &Path, thread: &str, contents: impl AsRef<[u8]>) -> PathBuf {
    let day = home.join("sessions/2026/10/04");
    fs::create_dir_all(&day).unwrap();
    let path = day.join(format!("rollout-2026-10-04T13-49-56-{thread}.jsonl"));
    fs::write(&path, contents).unwrap();
    path
}

/// Install the rollout header of a root thread: `session_id` equals `id` and
/// `source` is a string. Include only the fields used for capture validation;
/// other fields are present in a real header.
pub fn install_codex_root(home: &Path, thread: &str) -> PathBuf {
    install_codex_rollout(
        home,
        thread,
        codex_session_meta(&format!(
            r#"{{"id":"{thread}","session_id":"{thread}","source":"cli"}}"#
        )),
    )
}

/// Build the JSON object Claude's `SessionStart` hook receives for session `id`
/// started from `source`. Omit the newline present in hook stdin; callers that need
/// the complete capture payload can append it.
pub fn hook_json(id: &str, source: &str) -> String {
    format!(
        r#"{{"session_id":"{id}","transcript_path":"/t/x.jsonl","cwd":"/w","hook_event_name":"SessionStart","source":"{source}"}}"#
    )
}

/// Build owned rows from string literals.
pub fn rows(spec: &[&str]) -> Vec<String> {
    spec.iter().map(|s| s.to_string()).collect()
}

/// Build an empty screen for task `id` with the cursor shown at `(0, 0)`,
/// mouse reporting and alternate-screen modes off, and no scrollback offset.
/// Use struct update syntax to override only the fields a test exercises.
pub fn screen(id: u64) -> ScreenView {
    ScreenView {
        id,
        lines: Vec::new(),
        formatted: Vec::new(),
        cursor: (0, 0),
        hide_cursor: false,
        wants_mouse: false,
        alt_screen: false,
        alt_scroll: false,
        scrollback: 0,
    }
}

/// Corpus geometry: the fixture recordings in `tests/corpus/` were captured
/// under a 40-row, 120-column PTY (tests/corpus/README.md).
pub const CORPUS_LINES: usize = 40;
pub const CORPUS_COLS: usize = 120;

/// A raw backend `Term` of `lines`×`cols` with `bytes` parsed into it: the
/// reference grid for tests that compare the wrapper or the serializer
/// against the backend without going through `Emulator`.
pub fn parse_term(bytes: &[u8], lines: usize, cols: usize) -> Term<VoidListener> {
    let mut term = Term::new(Config::default(), &TermSize::new(cols, lines), VoidListener);
    let mut parser: Processor = Processor::new();
    parser.advance(&mut term, bytes);
    term
}

/// An emulator sized for corpus replay: corpus geometry plus enough
/// scrollback (2000 rows) to retain every fixture's history.
pub fn corpus_emulator() -> Emulator {
    Emulator::new(CORPUS_LINES as u16, CORPUS_COLS as u16, 2000)
}
