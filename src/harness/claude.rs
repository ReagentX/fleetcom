//! Claude session capture uses a launch-time `--session-id`, a `SessionStart`
//! hook, and the live session registry. A fresh launch pins a v4 UUID; every
//! launch installs the hook through `--settings`.
//! Live lookup reads `<claude-home>/sessions/<pid>.json`.
//!
//! With agent view enabled, a conversation can be moved into Claude's own
//! background daemon. In that daemon's processes, the inherited capture
//! environment and `--settings` flag are used to run the same hook with IDs
//! from other conversations. Disable agent view in the overlay to prevent
//! those handoffs. Stamp each payload with the parent Claude process's PID
//! and accept only captures from the task's own process.

use std::{
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

use super::summary::AWAITING_APPROVAL;
use super::{
    CAPTURE_ENV, CapturePaths, Harness, SpawnPlan, capture_id, home_root, is_uuid, resolve_home,
};

pub struct Claude;

impl Harness for Claude {
    fn resolve_home(&self, env: &dyn Fn(&str) -> Option<PathBuf>) -> Option<PathBuf> {
        resolve_home(env, "CLAUDE_CONFIG_DIR", ".claude")
    }

    fn shape(&self) -> (&'static str, &'static str) {
        ("claude", "--resume")
    }

    fn session_flag(&self) -> Option<&'static str> {
        Some("--session-id")
    }

    fn overlay(
        &self,
        capture: &CapturePaths,
        // The settings overlay does not depend on the Claude home path.
        _home: Option<&Path>,
    ) -> SpawnPlan {
        SpawnPlan {
            args: vec![
                "--settings".into(),
                capture.claude_settings.clone().into_os_string(),
            ],
            env: vec![(
                CAPTURE_ENV.into(),
                capture.capture_file.clone().into_os_string(),
            )],
            ..SpawnPlan::default()
        }
    }

    /// Accept a hook payload only from the task's own process. The first line
    /// is the PID of the Claude process that ran the hook and must be exactly
    /// the decimal form of `pid`; the remainder is the hook's JSON. Ownership
    /// cannot be determined from the JSON: `source: "fork"` is reported for
    /// both `/branch` in the task's process and a background fork.
    ///
    /// A managed launch runs the Claude binary directly, so the task leader
    /// is the Claude process, as [`record_for_pid`] also requires. A stamp
    /// from any other PID (a background fork, a wrapper that stayed resident)
    /// is rejected: fall back to the registry, then the spawn-time ID.
    /// Without a task PID, ownership cannot be checked.
    fn parse_capture(
        &self,
        payload: &str,
        pid: Option<u32>,
        // Ownership is decided by the stamp alone.
        _home: Option<&Path>,
    ) -> Option<String> {
        let json = payload.strip_prefix(&format!("{}\n", pid?))?;
        capture_id(&jzon::parse(json).ok()?, "session_id")
    }

    fn live_session_id(
        &self,
        pid: u32,
        cwd: &Path,
        spawned: SystemTime,
        home: Option<&Path>,
    ) -> Option<String> {
        Some(record_for_pid(pid, cwd, spawned, home)?.id)
    }

    fn live_blocked_status(
        &self,
        pid: u32,
        cwd: &Path,
        spawned: SystemTime,
        home: Option<&Path>,
    ) -> Option<(String, &'static str)> {
        let rec = record_for_pid(pid, cwd, spawned, home)?;
        // Override the screen-derived preview only for `waiting`.
        rec.waiting
            .then(|| waiting_preview(rec.waiting_for.as_deref()))
    }
}

/// Validated fields used to correlate a registry record with a task and render
/// its blocked status.
struct SessionRecord {
    /// `sessionId`, validated by [`is_uuid`].
    id: String,
    pid: i32,
    cwd: PathBuf,
    /// `startedAt`, in epoch milliseconds.
    started_at: u128,
    /// Whether `status` is `waiting`.
    waiting: bool,
    /// Optional `waitingFor` text.
    waiting_for: Option<String>,
}

/// Map a `waitingFor` reason to preview text and its matcher ID. Permission
/// prompts use the same text as the screen matcher; other non-empty reasons
/// remain verbatim. A missing reason falls back to `awaiting input`.
fn waiting_preview(reason: Option<&str>) -> (String, &'static str) {
    match reason.filter(|r| !r.is_empty()) {
        Some("permission prompt") => (AWAITING_APPROVAL.to_string(), "claude:registry-approval"),
        Some(other) => (other.to_string(), "claude:registry-waiting"),
        None => ("awaiting input".to_string(), "claude:registry-waiting"),
    }
}

/// Parse one interactive registry record. Malformed records and other `kind`
/// values return `None`; a missing or unrecognized status remains valid but
/// does not set `waiting`.
fn parse_record(text: &str) -> Option<SessionRecord> {
    let v = jzon::parse(text).ok()?;
    if v["kind"].as_str()? != "interactive" {
        return None;
    }
    let id = v["sessionId"].as_str().filter(|id| is_uuid(id))?;
    Some(SessionRecord {
        id: id.to_string(),
        pid: v["pid"].as_i32().filter(|p| *p > 0)?,
        cwd: PathBuf::from(v["cwd"].as_str()?),
        started_at: u128::from(v["startedAt"].as_u64()?),
        waiting: v["status"].as_str() == Some("waiting"),
        waiting_for: v["waitingFor"].as_str().map(str::to_string),
    })
}

/// Read `sessions/<pid>.json` and require its PID, working directory, and
/// process start to match the task. The unreaped task leader reserves its PID;
/// the directory and start-time checks reject stale records already present at
/// that path. The start time may differ by at most 30 seconds. A mismatch
/// returns `None` because the ID may enter a shell command.
fn record_for_pid(
    pid: u32,
    cwd: &Path,
    spawned: SystemTime,
    home: Option<&Path>,
) -> Option<SessionRecord> {
    let pid = i32::try_from(pid).ok()?;
    let dir = home_root(home, ".claude")?.join("sessions");
    let text = fs::read_to_string(dir.join(format!("{pid}.json"))).ok()?;
    let rec = parse_record(&text)?;
    let spawn_ms = spawned
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_millis();
    (rec.pid == pid
        && (rec.cwd == cwd || cwd.canonicalize().is_ok_and(|canon| rec.cwd == canon))
        && rec.started_at.abs_diff(spawn_ms) <= 30_000)
        .then_some(rec)
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use super::*;
    use crate::{
        harness::{
            Intent,
            fixtures::{ID, OTHER, argv, paths},
            plan,
        },
        testutil::temp,
    };

    /// The overlay path from [`paths`].
    const SETTINGS: &str = "/tmp/Application Support/fleetcom.json";

    /// Complete registry fixture with [`OTHER`] as its session ID.
    const LIVE_RECORD: &str = concat!(
        r#"{"pid":83849,"sessionId":"11111111-2222-4333-8444-555555555555","#,
        r#""cwd":"/private/tmp/.../scratchpad/live-claude","startedAt":1786834960302,"#,
        r#""procStart":"Sat Aug 15 23:02:39 2026","version":"2.1.233","peerProtocol":1,"#,
        r#""kind":"interactive","entrypoint":"cli","#,
        r#""messagingSocketPath":"/tmp/cc-socks/83849.sock","#,
        r#""name":"live-claude-66","nameSource":"derived","nameSince":1786834960303,"#,
        r#""status":"idle","updatedAt":1786834960352,"statusUpdatedAt":1786834960352}"#,
    );
    /// Identity fields in [`LIVE_RECORD`].
    const LIVE_PID: u32 = 83849;
    const LIVE_CWD: &str = "/private/tmp/.../scratchpad/live-claude";
    const LIVE_STARTED: u64 = 1_786_834_960_302;

    /// Build a registry record with optional raw JSON fields in `tail`.
    fn record(pid: i32, id: &str, cwd: &str, started: u64, kind: &str, tail: &str) -> String {
        format!(
            r#"{{"pid":{pid},"sessionId":"{id}","cwd":"{cwd}","startedAt":{started},"version":"2.1.233","kind":"{kind}","entrypoint":"cli"{tail}}}"#
        )
    }

    /// Write `body` to the registry path for `pid`.
    fn install_record(home: &Path, pid: i32, body: &str) {
        let dir = home.join("sessions");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{pid}.json")), body).unwrap();
    }

    /// Convert epoch milliseconds to [`SystemTime`].
    fn at_ms(ms: u64) -> SystemTime {
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms)
    }

    /// Managed argv: a fresh launch pins the minted ID, a resume names its
    /// conversation, and the settings overlay follows either. Both carry the
    /// capture file in the environment and report the ID they target.
    #[test]
    fn managed_argv_pins_or_resumes_then_layers_settings() {
        let fresh = plan(&Claude, &Intent::Fresh, Some(ID), &paths(), None);
        assert_eq!(
            fresh.args,
            argv(&["--session-id", ID, "--settings", SETTINGS])
        );
        assert_eq!(fresh.resume_id.as_deref(), Some(ID));
        assert_eq!(
            fresh.env,
            vec![(
                CAPTURE_ENV.into(),
                PathBuf::from("/tmp/cap/session.json").into_os_string()
            )]
        );
        assert_eq!(fresh.notice, None);

        let resume = plan(&Claude, &Intent::Resume(OTHER.into()), None, &paths(), None);
        assert_eq!(
            resume.args,
            argv(&["--resume", OTHER, "--settings", SETTINGS])
        );
        assert_eq!(resume.resume_id.as_deref(), Some(OTHER));
        assert_eq!(resume.env, fresh.env);

        // A mint failure launches unpinned: the overlay alone, no ID.
        let unpinned = plan(&Claude, &Intent::Fresh, None, &paths(), None);
        assert_eq!(unpinned.args, argv(&["--settings", SETTINGS]));
        assert_eq!(unpinned.resume_id, None);
    }

    /// Task leader PID used by the capture-gate cases.
    const OWNER: u32 = 4242;

    /// `SessionStart` JSON supplied to the hook: one object and a trailing
    /// newline.
    fn hook_json(id: &str, source: &str) -> String {
        format!(
            r#"{{"session_id":"{id}","transcript_path":"/t/x.jsonl","cwd":"/w","hook_event_name":"SessionStart","source":"{source}"}}"#
        ) + "\n"
    }

    /// Parse `payload` as the capture file of a task led by [`OWNER`].
    fn parse_owned(payload: &str) -> Option<String> {
        Claude.parse_capture(payload, Some(OWNER), None)
    }

    /// Accept the leader's stamp for every `source`, including `fork` after
    /// `/branch` in the task's own process.
    #[test]
    fn parse_capture_accepts_the_task_leaders_stamp() {
        for source in ["startup", "resume", "clear", "fork"] {
            let payload = format!("{OWNER}\n{}", hook_json(ID, source));
            assert_eq!(parse_owned(&payload).as_deref(), Some(ID), "{source}");
        }
    }

    /// Reject another process's PID, even with a shared decimal prefix.
    /// Also reject captures when the task has no PID.
    #[test]
    fn parse_capture_refuses_a_foreign_stamp() {
        let json = hook_json(ID, "startup");
        for foreign in [1, 424, 4243, 42420, 14242] {
            assert_eq!(
                parse_owned(&format!("{foreign}\n{json}")),
                None,
                "{foreign}"
            );
        }
        let owned = format!("{OWNER}\n{json}");
        assert_eq!(Claude.parse_capture(&owned, Some(424), None), None);
        assert_eq!(Claude.parse_capture(&owned, Some(42420), None), None);
        assert_eq!(Claude.parse_capture(&owned, None, None), None);
    }

    /// Require the leader's exact decimal PID on the first line. Reject the
    /// older, unstamped format, with JSON on the first line.
    #[test]
    fn parse_capture_refuses_a_missing_or_malformed_stamp() {
        let json = hook_json(ID, "startup");
        assert_eq!(parse_owned(&json), None, "bare JSON");
        assert_eq!(parse_owned(json.trim_end()), None, "bare JSON, no newline");
        for stamp in [
            "", "pid", "+4242", "-4242", "04242", " 4242", "4242 ", "4242\r", "0x1092", "4242.0",
        ] {
            assert_eq!(parse_owned(&format!("{stamp}\n{json}")), None, "{stamp:?}");
        }
    }

    /// Truncate the file, write the stamp, then copy the JSON. A read between
    /// writes can return any prefix of the complete capture.
    #[test]
    fn parse_capture_refuses_empty_and_torn_payloads() {
        let complete = format!("{OWNER}\n{}", hook_json(ID, "startup"));
        assert_eq!(parse_owned(&complete).as_deref(), Some(ID));
        // The object is complete without its trailing newline. For every
        // shorter prefix, the stamp, its newline, or the closing brace is missing.
        let whole = complete.trim_end().len();
        for cut in 0..whole {
            assert_eq!(
                parse_owned(&complete[..cut]),
                None,
                "{:?}",
                &complete[..cut]
            );
        }
        assert_eq!(parse_owned(&complete[..whole]).as_deref(), Some(ID));
    }

    /// Require a strict ID even with a valid stamp before shell insertion.
    #[test]
    fn parse_capture_returns_only_strict_ids_under_a_valid_stamp() {
        for json in [
            r#"{"session_id":"NOT-VALID"}"#,
            r#"{"session_id":"x'; rm -rf ~'"}"#,
            r#"{"session_id":"C8C4A5CC-0B32-4BA0-A6B4-6ED08C218E0D"}"#,
            r#"{"session_id":7}"#,
            "not json",
            "{}",
        ] {
            assert_eq!(parse_owned(&format!("{OWNER}\n{json}\n")), None, "{json}");
        }
    }

    /// A complete matching record exposes its validated session ID.
    #[test]
    fn record_for_pid_reads_a_live_record() {
        let home = temp("claude_registry");
        install_record(&home, LIVE_PID as i32, LIVE_RECORD);
        let cwd = Path::new(LIVE_CWD);
        let rec = record_for_pid(LIVE_PID, cwd, at_ms(LIVE_STARTED), Some(&home))
            .expect("the live record must parse");
        assert_eq!(rec.id, OTHER);
        assert!(!rec.waiting, "the record's status is `idle`");
        assert_eq!(rec.waiting_for, None);
        assert_eq!(
            Claude
                .live_session_id(LIVE_PID, cwd, at_ms(LIVE_STARTED), Some(&home))
                .as_deref(),
            Some(OTHER)
        );
    }

    /// The record must match its filename PID and the task directory.
    #[test]
    fn record_for_pid_requires_the_records_own_pid_and_cwd() {
        let home = temp("claude_registry_ident");
        let cwd = Path::new("/w");
        let spawned = at_ms(LIVE_STARTED);

        install_record(
            &home,
            4242,
            &record(4242, ID, "/w", LIVE_STARTED, "interactive", ""),
        );
        assert!(record_for_pid(4242, cwd, spawned, Some(&home)).is_some());

        // The filename and embedded PID must agree.
        install_record(
            &home,
            4242,
            &record(99, ID, "/w", LIVE_STARTED, "interactive", ""),
        );
        assert!(record_for_pid(4242, cwd, spawned, Some(&home)).is_none());

        install_record(
            &home,
            4242,
            &record(4242, ID, "/elsewhere", LIVE_STARTED, "interactive", ""),
        );
        assert!(record_for_pid(4242, cwd, spawned, Some(&home)).is_none());
    }

    /// Literal and canonical paths to the same directory both match.
    #[test]
    fn record_for_pid_accepts_a_symlinked_cwd_alias() {
        let tmp = temp("claude_registry_alias");
        let home = tmp.join("home");
        let real = tmp.join("real");
        let link = tmp.join("link");
        let other = tmp.join("other");
        for d in [&home, &real, &other] {
            fs::create_dir_all(d).unwrap();
        }
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let canonical = real.canonicalize().unwrap();
        install_record(
            &home,
            7,
            &record(
                7,
                ID,
                canonical.to_str().unwrap(),
                LIVE_STARTED,
                "interactive",
                "",
            ),
        );

        let spawned = at_ms(LIVE_STARTED);
        assert!(record_for_pid(7, &link, spawned, Some(&home)).is_some());
        // Different and nonexistent paths remain mismatches.
        assert!(record_for_pid(7, &other, spawned, Some(&home)).is_none());
        assert!(record_for_pid(7, &tmp.join("gone"), spawned, Some(&home)).is_none());
    }

    /// Reject a stale record with matching PID and CWD based on process start time.
    #[test]
    fn record_for_pid_rejects_a_recycled_pids_stale_record() {
        let home = temp("claude_registry_recycled");
        let cwd = Path::new("/w");
        install_record(
            &home,
            4242,
            &record(4242, ID, "/w", LIVE_STARTED, "interactive", ""),
        );

        // The start-time tolerance includes both endpoints.
        assert!(record_for_pid(4242, cwd, at_ms(LIVE_STARTED + 30_000), Some(&home)).is_some());
        assert!(record_for_pid(4242, cwd, at_ms(LIVE_STARTED - 30_000), Some(&home)).is_some());
        // One millisecond outside the window is stale.
        assert!(record_for_pid(4242, cwd, at_ms(LIVE_STARTED + 30_001), Some(&home)).is_none());
        assert!(record_for_pid(4242, cwd, at_ms(LIVE_STARTED + 600_000), Some(&home)).is_none());
    }

    /// Only interactive records with strict UUIDs are eligible.
    #[test]
    fn record_for_pid_requires_an_interactive_kind_and_a_strict_id() {
        let home = temp("claude_registry_kind");
        let cwd = Path::new("/w");
        let spawned = at_ms(LIVE_STARTED);
        for kind in ["bg", "daemon", "daemon-worker"] {
            install_record(&home, 7, &record(7, ID, "/w", LIVE_STARTED, kind, ""));
            assert!(
                record_for_pid(7, cwd, spawned, Some(&home)).is_none(),
                "{kind}"
            );
        }
        for id in ["NOT-A-UUID", "", "c8c4a5cc-0b32-4ba0-a6b4-6ed08c218e0dff"] {
            install_record(
                &home,
                7,
                &record(7, id, "/w", LIVE_STARTED, "interactive", ""),
            );
            assert!(
                record_for_pid(7, cwd, spawned, Some(&home)).is_none(),
                "{id:?}"
            );
        }
        // `kind` is required.
        install_record(
            &home,
            7,
            &format!(r#"{{"pid":7,"sessionId":"{ID}","cwd":"/w","startedAt":{LIVE_STARTED}}}"#),
        );
        assert!(record_for_pid(7, cwd, spawned, Some(&home)).is_none());
    }

    /// Malformed and absent records contribute no evidence.
    #[test]
    fn record_for_pid_tolerates_a_torn_file_and_a_missing_store() {
        let home = temp("claude_registry_torn");
        let cwd = Path::new(LIVE_CWD);
        let spawned = at_ms(LIVE_STARTED);
        for body in [&LIVE_RECORD[..LIVE_RECORD.len() / 2], "", "\0"] {
            install_record(&home, LIVE_PID as i32, body);
            assert!(
                record_for_pid(LIVE_PID, cwd, spawned, Some(&home)).is_none(),
                "{body:?}"
            );
        }
        // Missing file and missing directory follow the same path.
        assert!(record_for_pid(1, cwd, spawned, Some(&home)).is_none());
        let bare = temp("claude_registry_bare");
        assert!(record_for_pid(LIVE_PID, cwd, spawned, Some(&bare)).is_none());
    }

    /// Status values other than `waiting` do not invalidate the session ID.
    #[test]
    fn record_for_pid_keeps_the_id_under_an_unread_status() {
        let home = temp("claude_registry_status");
        let cwd = Path::new("/w");
        let spawned = at_ms(LIVE_STARTED);
        for tail in ["", r#","status":"hibernating""#, r#","status":"busy""#] {
            install_record(
                &home,
                7,
                &record(7, ID, "/w", LIVE_STARTED, "interactive", tail),
            );
            let rec = record_for_pid(7, cwd, spawned, Some(&home)).expect("the record must parse");
            assert_eq!(rec.id, ID, "{tail:?}");
            assert!(!rec.waiting, "{tail:?}");
        }
    }

    /// Permission prompts use the approval label, other reasons remain
    /// verbatim, and an absent reason falls back to `awaiting input`.
    #[test]
    fn live_blocked_status_maps_every_waiting_reason() {
        let home = temp("claude_blocked_reasons");
        let cwd = Path::new("/w");
        let spawned = at_ms(LIVE_STARTED);
        let probe = |tail: &str| {
            install_record(
                &home,
                7,
                &record(7, ID, "/w", LIVE_STARTED, "interactive", tail),
            );
            Claude.live_blocked_status(7, cwd, spawned, Some(&home))
        };

        assert_eq!(
            probe(r#","status":"waiting","waitingFor":"permission prompt""#),
            Some(("awaiting approval".to_string(), "claude:registry-approval"))
        );
        for reason in [
            "input needed",
            "dialog open",
            "sandbox request",
            "worker request",
            "quantum entanglement request",
        ] {
            assert_eq!(
                probe(&format!(r#","status":"waiting","waitingFor":"{reason}""#)),
                Some((reason.to_string(), "claude:registry-waiting")),
                "{reason}"
            );
        }
        for tail in [
            r#","status":"waiting""#,
            r#","status":"waiting","waitingFor":"""#,
        ] {
            assert_eq!(
                probe(tail),
                Some(("awaiting input".to_string(), "claude:registry-waiting")),
                "{tail:?}"
            );
        }
    }

    /// Only `waiting` produces a blocked-status preview.
    #[test]
    fn live_blocked_status_answers_for_waiting_alone() {
        let home = temp("claude_blocked_states");
        let cwd = Path::new("/w");
        let spawned = at_ms(LIVE_STARTED);
        let probe = |tail: &str| {
            install_record(
                &home,
                7,
                &record(7, ID, "/w", LIVE_STARTED, "interactive", tail),
            );
            Claude.live_blocked_status(7, cwd, spawned, Some(&home))
        };

        for tail in [
            r#","status":"busy""#,
            r#","status":"shell""#,
            r#","status":"idle""#,
            r#","status":"hibernating""#,
            "",
            // A reason without `status: waiting` is not blocked.
            r#","waitingFor":"permission prompt""#,
        ] {
            assert_eq!(probe(tail), None, "{tail:?}");
        }
        // A missing record contributes no status.
        assert_eq!(
            Claude.live_blocked_status(9, cwd, spawned, Some(&home)),
            None
        );
    }
}
