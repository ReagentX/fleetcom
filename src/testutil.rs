//! Test scaffolds shared by the in-src test modules: scratch directories,
//! deadline polling, and the corpus/rollout fixtures. Test-only (`#[cfg(test)]`
//! at the declaration in `main.rs`), so nothing here ships.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

use crate::{emulator::Emulator, harness::civil_from_days};

/// Fresh scratch directory under the system temp dir. Any leftover from a
/// previous run is removed first; the pid suffix isolates concurrent suites.
pub(crate) fn temp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fleetcom_test_{tag}_{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
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

/// Read a pid a test job wrote, waiting for the write to land.
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

/// Corpus geometry: the fixture recordings in `tests/corpus/` were captured
/// under a 40-row, 120-column PTY (tests/corpus/README.md).
pub(crate) const CORPUS_LINES: usize = 40;
pub(crate) const CORPUS_COLS: usize = 120;

/// An emulator sized for corpus replay: corpus geometry plus enough
/// scrollback (2000 rows) to retain every fixture's history.
pub(crate) fn corpus_emulator() -> Emulator {
    Emulator::new(CORPUS_LINES as u16, CORPUS_COLS as u16, 2000)
}

/// A v7-shaped ID whose embedded instant is `ms`, with a fixed tail.
pub(crate) fn v7_at(ms: u64, tail: u32) -> String {
    format!(
        "{:08x}-{:04x}-7000-8000-0000000{:05x}",
        ms >> 16,
        ms & 0xffff,
        tail
    )
}

/// Write a rollout under the UTC day dir for `ms` with `cwd` in its
/// `session_meta` line; returns the ID. The filename timestamp is inert:
/// correlation reads the v7 ID's embedded instant, never the name.
pub(crate) fn write_rollout(home: &Path, ms: u64, tail: u32, cwd: &Path) -> String {
    let id = v7_at(ms, tail);
    let (y, m, d) = civil_from_days((ms / 86_400_000) as i64);
    let dir = home
        .join("sessions")
        .join(format!("{y:04}"))
        .join(format!("{m:02}"))
        .join(format!("{d:02}"));
    fs::create_dir_all(&dir).unwrap();
    let meta = format!(
        r#"{{"timestamp":"x","type":"session_meta","payload":{{"id":"{id}","cwd":"{}"}}}}"#,
        cwd.display()
    );
    fs::write(
        dir.join(format!("rollout-2026-07-13T09-00-00-{id}.jsonl")),
        format!("{meta}\n{{}}\n"),
    )
    .unwrap();
    id
}
