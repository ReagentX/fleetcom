//! omp cannot pin a session ID at launch: it has no `--session-id` flag, and
//! `--resume` requires an existing session. Live capture therefore loads an
//! extension whose `session_start` and `session_switch` handlers write the
//! current ID. Exit capture reads `omp --resume <uuid>` hints from ordinary
//! exit output and `[Recovery]` blocks.
//!
//! omp's IDs are UUIDv7. [`is_uuid`](super::is_uuid) validates the 8-4-4-4-12
//! lowercase-hex shape and not the version field, so they pass unchanged.
//!
//! `-r`, `--session`, and `-c` resume as well, but detection stays on the
//! canonical pair: a command fleetcom cannot rewrite exactly is left verbatim.

use std::path::Path;

use super::{
    CAPTURE_ENV, CapturePaths, Harness, Invocation, SpawnPlan, capture_id, leading_uuid,
    shell_quote,
};

/// Command fragment shared by ordinary exit and recovery hints.
const RESUME_HINT: &str = "omp --resume ";

/// Label identifying the resumable session in a recovery block.
const MAIN_LABEL: &str = "Main";

pub struct Omp;

impl Harness for Omp {
    fn shape(&self) -> (&'static str, &'static str) {
        ("omp", "--resume")
    }

    /// Append the capture extension with `-e` for both accepted command shapes.
    fn instrument(
        &self,
        _inv: &Invocation,
        capture: &CapturePaths,
        _home: Option<&Path>,
    ) -> SpawnPlan {
        SpawnPlan {
            args_suffix: format!(
                " -e {}",
                shell_quote(&capture.omp_capture.to_string_lossy())
            ),
            env: vec![(
                CAPTURE_ENV.into(),
                capture.capture_file.clone().into_os_string(),
            )],
            injected_id: None,
        }
    }

    /// Return `sessionId` from a valid extension payload.
    fn parse_capture(&self, payload: &str) -> Option<String> {
        capture_id(&jzon::parse(payload).ok()?, "sessionId")
    }

    /// Return the last trusted exit hint. Unlabelled hints are ordinary exit
    /// lines. Labelled recovery hints count only when their label is `Main`;
    /// other labels identify subagent sessions that `omp --resume` cannot open.
    fn scrape_exit(&self, text: &str) -> Option<String> {
        let mut last: Option<String> = None;
        for (i, _) in text.match_indices(RESUME_HINT) {
            let Some(id) = leading_uuid(&text[i + RESUME_HINT.len()..]) else {
                continue;
            };
            let head = &text[..i];
            let head = &head[head.rfind(['\n', '\r']).map_or(0, |n| n + 1)..];
            // A trailing `": "` marks a labelled recovery entry. Reject
            // unknown labels because exit evidence outranks the capture file.
            if let Some(label) = head.strip_suffix(": ")
                && label.trim() != MAIN_LABEL
            {
                continue;
            }
            last = Some(id.to_string());
        }
        last
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::harness::fixtures::{ID, OTHER, assert_all_opaque, assert_corpus_scrape, paths};

    /// Valid UUIDv7 used in capture payloads.
    const CAPTURED: &str = "01a0077c-e18e-7000-ae0b-016f4834b6e9";

    /// omp-specific aliases, shortcuts, prompts, and malformed resume forms
    /// remain opaque.
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

    /// Both accepted shapes load the extension and name the capture file, and
    /// neither gains an ID: omp has no `--session-id`, so a pinned UUID would
    /// name a session `--resume` cannot reach. The fixture's asset path
    /// carries a space, so the quoting has to hold it to one word.
    #[test]
    fn instrument_loads_the_extension_for_either_accepted_shape() {
        for cmd in ["omp".to_string(), format!("omp --resume {ID}")] {
            let inv = Omp.detect(&cmd).unwrap();
            let plan = Omp.instrument(&inv, &paths(), None);
            assert_eq!(plan.injected_id, None, "{cmd}");
            assert_eq!(
                plan.args_suffix, " -e '/tmp/Application Support/omp-capture.js'",
                "{cmd}"
            );
            assert_eq!(
                plan.env,
                vec![(
                    CAPTURE_ENV.into(),
                    PathBuf::from("/tmp/cap/session.json").into_os_string()
                )],
                "{cmd}"
            );
        }
    }

    /// The extension payload yields only a validated ID.
    #[test]
    fn parse_capture_returns_only_strict_ids() {
        for reason in ["session_start", "session_switch"] {
            let payload = format!(
                r#"{{"reason":"{reason}","sessionId":"{CAPTURED}","sessionFile":"/s/2026-08-15T22-13-39-854Z_{CAPTURED}.jsonl","cwd":"/work/proj"}}"#
            );
            assert_eq!(Omp.parse_capture(&payload).as_deref(), Some(CAPTURED));
        }

        assert_eq!(Omp.parse_capture("not json"), None);
        assert_eq!(Omp.parse_capture("{}"), None);
        assert_eq!(Omp.parse_capture(r#"{"sessionId":"my session"}"#), None);
        assert_eq!(Omp.parse_capture(r#"{"sessionId":"x'; rm -rf ~'"}"#), None);
        // UUID validation rejects uppercase hex.
        assert_eq!(
            Omp.parse_capture(&format!(r#"{{"sessionId":"{}"}}"#, CAPTURED.to_uppercase())),
            None
        );
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

        // A labelled recovery hint names the main session.
        let crash = format!("[Recovery]\n  Main: omp --resume {ID}\n");
        assert_eq!(Omp.scrape_exit(&crash).as_deref(), Some(ID));

        // Subagent labels do not displace the main session.
        let sub_a = "22222222-3333-4444-8555-666666666666";
        let sub_b = "33333333-4444-4555-8666-777777777777";
        let subagents =
            format!("  agent-1: omp --resume {sub_a}\n  agent-2: omp --resume {sub_b}\n");
        let swarm = format!("[Recovery]\n  Main: omp --resume {ID}\n{subagents}");
        assert_eq!(Omp.scrape_exit(&swarm).as_deref(), Some(ID));

        // A recovery block containing only subagents yields no exit evidence.
        let orphans = format!("[Recovery]\n{subagents}");
        assert_eq!(Omp.scrape_exit(&orphans), None);

        // A later unlabelled exit hint remains eligible.
        let recovered = format!("{orphans}...\nResume this session with omp --resume {ID}\n");
        assert_eq!(Omp.scrape_exit(&recovered).as_deref(), Some(ID));

        assert_eq!(Omp.scrape_exit("no hint here"), None);
        // A hint whose ID fails validation returns nothing.
        assert_eq!(Omp.scrape_exit("omp --resume NOT-A-UUID"), None);
        // A longer hexadecimal run is not an ID.
        assert_eq!(Omp.scrape_exit(&format!("omp --resume {ID}ff")), None);
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
