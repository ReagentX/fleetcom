//! An omp session ID cannot be pinned at launch: no `--session-id` flag is
//! available, and an existing session is required for `--resume`. For live
//! capture, load an extension to report the top-level ID on `session_start`,
//! `session_switch`, `session_branch`, and `agent_end`.
//!
//! Report only IDs usable with `omp --resume`. Skip sub-agent sessions:
//! the same handlers are registered for them, but their IDs are not resumable.
//! Wait until the session file is created after the first assistant message.
//! For a bare `omp` task, no ID is captured until the end of the first turn.
//! Capture is unavailable before omp 18.3.2: without the agent kind in the
//! extension context, top-level sessions cannot be distinguished from sub-agents.
//!
//! omp's IDs are UUIDv7. [`is_uuid`](super::is_uuid) validates the 8-4-4-4-12
//! lowercase-hex shape and not the version field, so they pass unchanged.
//!
//! `-r`, `--session`, and `-c` resume as well, but detection stays on the
//! canonical pair: a command fleetcom cannot rewrite exactly is left verbatim.

use std::path::Path;

use super::{CAPTURE_ENV, CapturePaths, Harness, SpawnPlan, capture_id};

pub struct Omp;

impl Harness for Omp {
    fn shape(&self) -> (&'static str, &'static str) {
        ("omp", "--resume")
    }

    /// Load the capture extension with `-e`. No session flag: omp has no
    /// `--session-id`, so a pinned UUID could not be resumed.
    fn overlay(&self, capture: &CapturePaths, _home: Option<&Path>) -> SpawnPlan {
        SpawnPlan {
            args: vec!["-e".into(), capture.omp_capture.clone().into_os_string()],
            env: vec![(
                CAPTURE_ENV.into(),
                capture.capture_file.clone().into_os_string(),
            )],
            ..SpawnPlan::default()
        }
    }

    /// Return `sessionId` from a valid extension payload. Check the agent
    /// kind in the extension: without that field in the payload, the check
    /// cannot be repeated here.
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
    use crate::harness::{
        Intent,
        fixtures::{ID, argv, assert_all_opaque, paths},
        plan, shell_words,
    };

    /// Valid UUIDv7 used in capture payloads.
    const CAPTURED: &str = "01a0077c-e18e-7000-ae0b-016f4834b6e9";

    /// The extension path from [`paths`].
    const EXTENSION: &str = "/tmp/Application Support/omp-capture.js";

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

    /// Managed argv: a fresh launch loads the extension alone, since omp has
    /// no `--session-id` and a minted ID would be unresumable; a resume names
    /// its conversation first. Both name the capture file in the environment.
    #[test]
    fn managed_argv_loads_the_extension_and_pins_nothing() {
        let fresh = plan(&Omp, &Intent::Fresh, Some(ID), &paths(), None);
        assert_eq!(fresh.args, argv(&["-e", EXTENSION]));
        assert_eq!(fresh.resume_id, None, "the minted id is ignored");
        assert_eq!(
            fresh.env,
            vec![(
                CAPTURE_ENV.into(),
                PathBuf::from("/tmp/cap/session.json").into_os_string()
            )]
        );

        let resume = plan(&Omp, &Intent::Resume(CAPTURED.into()), None, &paths(), None);
        assert_eq!(resume.args, argv(&["--resume", CAPTURED, "-e", EXTENSION]));
        assert_eq!(resume.resume_id.as_deref(), Some(CAPTURED));
        assert_eq!(resume.env, fresh.env);
    }

    /// Both literal shapes get the extension as one quoted word (the
    /// fixture path has a space) and no pin.
    #[test]
    fn literal_suffix_loads_the_extension_for_either_accepted_shape() {
        let overlay = Omp.overlay(&paths(), None);
        assert_eq!(overlay.resume_id, None);
        assert_eq!(
            shell_words(&overlay.args),
            " -e '/tmp/Application Support/omp-capture.js'"
        );
        // The bare word's fresh intent part is empty, so its suffix is the
        // overlay's.
        let fresh = plan(&Omp, &Intent::Fresh, Some(ID), &paths(), None);
        assert_eq!(fresh.args, overlay.args);
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
