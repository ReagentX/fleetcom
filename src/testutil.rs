//! Test scaffolds shared by the in-src test modules: scratch directories,
//! deadline polling, and corpus fixtures. Test-only (`#[cfg(test)]`
//! at the declaration in `main.rs`), so nothing here ships.

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

use crate::{
    emulator::Emulator,
    task::{pid_is_dead, positive_pid},
};

/// Versioned prefix for scratch directories eligible for sweeping.
const SCRATCH_PREFIX: &str = "fleetcom_test2_";

/// Scratch directory removed on drop. A panic preserves it for inspection;
/// later runs reclaim it after the owner exits.
pub(crate) struct Scratch(PathBuf);

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
pub(crate) fn temp(tag: &str) -> Scratch {
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
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(seq) || !digits(pid) {
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
pub(crate) fn wait_until(budget: Duration, mut pred: impl FnMut() -> bool) -> bool {
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
pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Read a pid a test task wrote, waiting for the write to land.
pub(crate) fn read_pid(path: &Path) -> nix::unistd::Pid {
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
pub(crate) fn dead_pid() -> u32 {
    let mut child = Command::new("sh").arg("-c").arg("exit 0").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

/// Return this process's working directory.
pub(crate) fn here() -> PathBuf {
    std::env::current_dir().unwrap()
}

/// Snapshot this process's environment for a launch context.
pub(crate) fn env_here() -> Vec<(OsString, OsString)> {
    std::env::vars_os().collect()
}

/// `env_here` with `SHELL` pinned to `/bin/sh` for portable background-job
/// behavior in process-group tests.
pub(crate) fn sh_env() -> Vec<(OsString, OsString)> {
    let mut env = env_here();
    env.retain(|(k, _)| k != "SHELL");
    env.push(("SHELL".into(), "/bin/sh".into()));
    env
}

/// Write an executable `#!/bin/sh` script at `path`. The parent must exist.
pub(crate) fn write_executable(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

/// Install a fake notifier at `path` that records its argv, one token
/// per line, into `record`.
pub(crate) fn install_fake_notifier(path: &Path, record: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    write_executable(
        path,
        &format!("printf '%s\\n' \"$@\" > '{}'", record.display()),
    );
}

/// Corpus geometry: the fixture recordings in `tests/corpus/` were captured
/// under a 40-row, 120-column PTY (tests/corpus/README.md).
pub(crate) const CORPUS_LINES: usize = 40;
pub(crate) const CORPUS_COLS: usize = 120;

/// An emulator sized for corpus replay: corpus geometry plus enough
/// scrollback (2000 rows) to retain every fixture's history.
pub(crate) fn corpus_emulator() -> Emulator {
    Emulator::new(CORPUS_LINES as u16, CORPUS_COLS as u16, 2000)
}
