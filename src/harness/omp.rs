//! omp cannot pin a session ID at launch: it has no `--session-id` flag, and
//! `--resume` rejects an ID that does not already exist, so a pinned UUID would
//! name a session the resume command could never reach. Capture therefore has
//! to come from omp itself — instrumentation in a later phase, and meanwhile
//! the hint it prints to stderr on exit, `Resume this session with omp
//! --resume <uuid>`. A crash repeats the same command inside a `[Recovery]`
//! block as `Main: omp --resume <uuid>`; both carry the command substring, so
//! one matcher reads both.
//!
//! omp's IDs are UUIDv7. [`is_uuid`](super::is_uuid) validates the 8-4-4-4-12
//! lowercase-hex shape and not the version field, so they pass unchanged.
//!
//! `-r`, `--session`, and `-c` resume as well, but detection stays on the
//! canonical pair: a command fleetcom cannot rewrite exactly is left verbatim.

use std::{path::Path, time::SystemTime};

use super::{CapturePaths, Harness, Invocation, SpawnPlan, last_hint};

pub struct Omp;

impl Harness for Omp {
    fn home_env_var(&self) -> &'static str {
        "PI_CODING_AGENT_DIR"
    }

    /// Two components: omp's session store sits one level below its config
    /// root. `home_root`'s `join` keeps both.
    fn home_dot_dir(&self) -> &'static str {
        ".omp/agent"
    }

    fn shape(&self) -> (&'static str, &'static str) {
        ("omp", "--resume")
    }

    fn instrument(
        &self,
        // Nothing distinguishes the two accepted shapes yet: omp cannot pin an
        // ID at launch, and no capture channel is injected.
        _inv: &Invocation,
        _capture: &CapturePaths,
        _home: Option<&Path>,
    ) -> SpawnPlan {
        // Capture injection lands in a later phase. Until then the command
        // runs unmodified and `scrape_exit` is the only channel.
        SpawnPlan::default()
    }

    /// No capture channel is injected yet, so no payload is ever trusted.
    fn parse_capture(&self, _payload: &str) -> Option<String> {
        None
    }

    fn scrape_exit(&self, text: &str) -> Option<String> {
        // The last valid hint names the session at exit. The exit line and the
        // crash recovery block print the same command.
        last_hint(text, &["omp --resume "])
    }

    fn correlate_fs(
        &self,
        _cwd: &Path,
        _spawned: SystemTime,
        _home: Option<&Path>,
    ) -> Option<String> {
        // The `<agent-dir>/sessions/<encoded-cwd>/` scan lands in a later
        // phase; refusing beats guessing until it exists.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::fixtures::{ID, OTHER, assert_all_opaque, assert_corpus_scrape, paths};

    /// omp-specific opaque shapes: the `-r`/`--session` resume aliases, the
    /// `-c`/`--continue` most-recent shortcut, a truncated ID, a prompt flag,
    /// and a neighbouring program word. The syntax shared by every harness is
    /// covered by the table test in `harness::tests`.
    #[test]
    fn everything_else_is_opaque_and_never_rewritten() {
        let opaque: Vec<String> = ["omp -c", "omp --continue", "omp --resume", "ompx"]
            .iter()
            .map(|s| s.to_string())
            .chain([
                format!("omp -r {ID}"),
                format!("omp --session {ID}"),
                format!("omp --resume {}", &ID[..8]),
                "omp -p 'fix the tests'".to_string(),
            ])
            .collect();
        assert_all_opaque(&Omp, ID, &opaque);
    }

    /// Neither accepted shape gains an ID: omp has no `--session-id`, so a
    /// pinned UUID would name a session `--resume` cannot reach.
    #[test]
    fn instrument_pins_no_id_for_either_accepted_shape() {
        for cmd in ["omp".to_string(), format!("omp --resume {ID}")] {
            let inv = Omp.detect(&cmd).unwrap();
            let plan = Omp.instrument(&inv, &paths(), None);
            assert!(plan.injected_id.is_none(), "{cmd}");
            assert_eq!(plan, SpawnPlan::default(), "{cmd}");
        }
    }

    #[test]
    fn parse_capture_is_unconditionally_none() {
        // No injected channel exists, so no payload is ever trusted.
        let payload = format!(r#"{{"session_id":"{ID}"}}"#);
        assert_eq!(Omp.parse_capture(&payload), None);
        assert_eq!(Omp.parse_capture(""), None);
    }

    #[test]
    fn scrape_exit_reads_the_hint_and_takes_the_last() {
        let hint = format!("Resume this session with omp --resume {ID}");
        assert_eq!(Omp.scrape_exit(&hint).as_deref(), Some(ID));

        // The last hint by position wins.
        let both = format!(
            "Resume this session with omp --resume {OTHER}\n...\n\
             Resume this session with omp --resume {ID}\n"
        );
        assert_eq!(Omp.scrape_exit(&both).as_deref(), Some(ID));

        // A crash prints the same command indented under `[Recovery]`.
        let crash = format!("[Recovery]\n  Main: omp --resume {ID}\n");
        assert_eq!(Omp.scrape_exit(&crash).as_deref(), Some(ID));

        assert_eq!(Omp.scrape_exit("no hint here"), None);
        // A hint whose ID fails validation returns nothing.
        assert_eq!(Omp.scrape_exit("omp --resume NOT-A-UUID"), None);
        // A longer hexadecimal run is not an ID.
        assert_eq!(Omp.scrape_exit(&format!("omp --resume {ID}ff")), None);
    }

    #[test]
    fn correlate_fs_is_unconditionally_none() {
        assert_eq!(
            Omp.correlate_fs(Path::new("/work"), SystemTime::now(), None),
            None
        );
    }

    /// The scraper recovers the exit-hint ID from the corpus terminal bytes.
    #[test]
    fn corpus_scrape_recovers_the_exit_hint_id() {
        assert_corpus_scrape(
            &Omp,
            include_bytes!("../../tests/corpus/omp_resume.bin"),
            "01a0078a-7714-7000-9927-f167df9b6476",
        );
    }
}
