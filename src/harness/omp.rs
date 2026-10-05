//! omp cannot pin a session ID at launch: it has no `--session-id` flag, and
//! `--resume` requires an existing session. Live capture therefore loads an
//! extension that reports the top-level session's ID on `session_start`,
//! `session_switch`, `session_branch`, and `agent_end`.
//!
//! The extension reports only an ID that `omp --resume` can open. It skips
//! sub-agent sessions: omp binds the same handlers to them, and their IDs are
//! not resumable. It waits for the session file, which omp creates after the
//! first assistant message. A bare `omp` task therefore has no ID until its
//! first turn ends, and never has one on an omp older than 18.3.2, which does
//! not tell the extension whether its session is a sub-agent's.
//!
//! omp's IDs are UUIDv7. [`is_uuid`](super::is_uuid) validates the 8-4-4-4-12
//! lowercase-hex shape and not the version field, so they pass unchanged.
//!
//! `-r`, `--session`, and `-c` resume as well, but detection stays on the
//! canonical pair: a command fleetcom cannot rewrite exactly is left verbatim.

use std::path::Path;

use super::{CAPTURE_ENV, CapturePaths, Harness, Invocation, SpawnPlan, capture_id, shell_quote};

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

    /// Return `sessionId` from a valid extension payload. The extension
    /// decides which sessions report: the payload does not carry the agent
    /// kind, so that gate cannot be repeated here.
    fn parse_capture(
        &self,
        payload: &str,
        _pid: Option<u32>,
        _home: Option<&Path>,
    ) -> Option<String> {
        capture_id(&jzon::parse(payload).ok()?, "sessionId")
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::harness::fixtures::{ID, assert_all_opaque, paths};

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

    /// Load the extension and specify the capture file for both accepted shapes. Pin no
    /// ID: omp has no `--session-id`, so a pinned UUID could not be resumed. Include a
    /// space in the fixture's asset path to verify quoting as one word.
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

    /// Extract only a validated ID from the extension payload.
    #[test]
    fn parse_capture_returns_only_strict_ids() {
        let parse = |payload: &str| Omp.parse_capture(payload, None, None);
        for reason in [
            "session_start",
            "session_switch",
            "session_branch",
            "agent_end",
        ] {
            let payload = format!(
                r#"{{"reason":"{reason}","sessionId":"{CAPTURED}","sessionFile":"/s/2026-08-15T22-13-39-854Z_{CAPTURED}.jsonl","cwd":"/work/proj"}}"#
            );
            assert_eq!(parse(&payload).as_deref(), Some(CAPTURED));
        }

        assert_eq!(parse("not json"), None);
        assert_eq!(parse("{}"), None);
        assert_eq!(parse(r#"{"sessionId":"my session"}"#), None);
        assert_eq!(parse(r#"{"sessionId":"x'; rm -rf ~'"}"#), None);
        // Reject uppercase hex during UUID validation.
        assert_eq!(
            parse(&format!(r#"{{"sessionId":"{}"}}"#, CAPTURED.to_uppercase())),
            None
        );
        assert_eq!(parse(""), None);
    }
}
