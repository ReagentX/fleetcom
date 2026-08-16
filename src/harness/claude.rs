//! Claude session capture uses a launch-time `--session-id`, a `SessionStart`
//! hook, the live session registry, and the exit-time resume hint. Bare launches
//! pin a v4 UUID; accepted launches install the hook through `--settings`.
//! Live lookup reads `<claude-home>/sessions/<pid>.json`; fallback correlation
//! reads project transcripts.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use super::summary::AWAITING_APPROVAL;
use super::{
    CAPTURE_ENV, CapturePaths, Harness, Invocation, SpawnPlan, is_uuid, last_hint, pin_plan,
    shell_quote, within_window, within_window_ms,
};

pub struct Claude;

impl Harness for Claude {
    fn home_env_var(&self) -> &'static str {
        "CLAUDE_CONFIG_DIR"
    }

    fn home_dot_dir(&self) -> &'static str {
        ".claude"
    }

    fn shape(&self) -> (&'static str, &'static str) {
        ("claude", "--resume")
    }

    fn instrument(
        &self,
        inv: &Invocation,
        capture: &CapturePaths,
        // The settings overlay does not depend on the Claude home path.
        _home: Option<&Path>,
    ) -> SpawnPlan {
        let mut plan = pin_plan(inv);
        plan.args_suffix.push_str(" --settings ");
        plan.args_suffix
            .push_str(&shell_quote(&capture.claude_settings.to_string_lossy()));
        plan.env = vec![(
            CAPTURE_ENV.into(),
            capture.capture_file.clone().into_os_string(),
        )];
        plan
    }

    fn parse_capture(&self, payload: &str) -> Option<String> {
        let v = jzon::parse(payload).ok()?;
        let id = v["session_id"].as_str()?;
        is_uuid(id).then(|| id.to_string())
    }

    fn scrape_exit(&self, text: &str) -> Option<String> {
        // The last valid hint names the conversation at exit.
        last_hint(text, &["claude --resume "])
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
        // Only `waiting` overrides the screen-derived preview.
        rec.waiting
            .then(|| waiting_preview(rec.waiting_for.as_deref()))
    }

    fn correlate_fs(&self, cwd: &Path, spawned: SystemTime, home: Option<&Path>) -> Option<String> {
        let dir = self.home_root(home)?.join("projects").join(slug(cwd)?);
        unique_in_window(dir, spawned)
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
/// that path. A mismatch returns `None` because the ID may enter a shell command.
fn record_for_pid(
    pid: u32,
    cwd: &Path,
    spawned: SystemTime,
    home: Option<&Path>,
) -> Option<SessionRecord> {
    let pid = i32::try_from(pid).ok()?;
    let dir = Claude.home_root(home)?.join("sessions");
    let text = fs::read_to_string(dir.join(format!("{pid}.json"))).ok()?;
    let rec = parse_record(&text)?;
    let spawned_ms = spawned.duration_since(UNIX_EPOCH).ok()?.as_millis();
    // Test literal equality before canonicalizing the task path: identical
    // nonexistent paths remain eligible.
    let same_cwd = rec.cwd == cwd || cwd.canonicalize().is_ok_and(|c| rec.cwd == c);
    (rec.pid == pid && same_cwd && within_window_ms(rec.started_at, spawned_ms)).then_some(rec)
}

/// Return the stem of the sole `.jsonl` transcript in `dir` created within
/// [`super::CORRELATE_WINDOW`] of `spawned`. Missing creation times, multiple
/// candidates, and a sole invalid UUID return `None`.
fn unique_in_window(dir: PathBuf, spawned: SystemTime) -> Option<String> {
    let mut candidates: Vec<String> = Vec::new();
    for entry in fs::read_dir(dir).ok()?.flatten() {
        // A transcript's stem is its session ID.
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(created) = entry.metadata().and_then(|m| m.created()) else {
            continue;
        };
        if !within_window(created, spawned) {
            continue;
        }
        candidates.push(name.to_string());
    }
    match candidates.as_slice() {
        [only] if is_uuid(only) => Some(only.clone()),
        _ => None,
    }
}

/// Convert an absolute working directory to Claude's project slug by replacing
/// `/` and `.` with `-` (`/a/b.c` becomes `-a-b-c`). Non-UTF-8 paths have no
/// representable slug.
fn slug(cwd: &Path) -> Option<String> {
    Some(
        cwd.to_str()?
            .chars()
            .map(|c| if c == '/' || c == '.' { '-' } else { c })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use super::*;
    use crate::{
        harness::fixtures::{ID, OTHER, assert_all_opaque, assert_corpus_scrape, paths},
        testutil::temp,
    };

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
        UNIX_EPOCH + std::time::Duration::from_millis(ms)
    }

    /// Claude-specific opaque shapes: flags, `--continue`/`-c`, subcommands,
    /// the short/`=` resume spellings, and `--session-id`. The syntax shared
    /// by every harness is covered by the table test in `harness::tests`.
    #[test]
    fn everything_else_is_opaque_and_never_rewritten() {
        let opaque: Vec<String> = [
            "claude --model opus",
            "claude --continue",
            "claude -c",
            "claude mcp list",
            "claudius",
        ]
        .iter()
        .map(|s| s.to_string())
        .chain([
            format!("claude -r {ID}"),
            format!("claude --resume {ID} --model opus"),
            format!("claude --session-id {ID}"),
        ])
        .collect();
        assert_all_opaque(&Claude, ID, &opaque);
    }

    #[test]
    fn instrument_pins_an_id_and_layers_settings_on_bare_launches() {
        let inv = Claude.detect("claude").unwrap();
        let plan = Claude.instrument(&inv, &paths(), None);
        let id = plan.injected_id.expect("a bare launch pins an id");
        assert!(is_uuid(&id));
        assert_eq!(
            plan.args_suffix,
            format!(" --session-id '{id}' --settings '/tmp/Application Support/fleetcom.json'")
        );
        assert_eq!(
            plan.env,
            vec![(
                CAPTURE_ENV.into(),
                PathBuf::from("/tmp/cap/session.json").into_os_string()
            )]
        );
    }

    /// A resume command already targets a conversation, so instrumentation adds
    /// the settings overlay without pinning another ID.
    #[test]
    fn instrument_adds_only_settings_to_the_resume_form() {
        let inv = Claude.detect(&format!("claude --resume {ID}")).unwrap();
        let plan = Claude.instrument(&inv, &paths(), None);
        assert_eq!(plan.injected_id, None);
        assert_eq!(
            plan.args_suffix,
            " --settings '/tmp/Application Support/fleetcom.json'"
        );
        assert_eq!(plan.env.len(), 1, "env still names the capture file");
    }

    #[test]
    fn parse_capture_returns_only_strict_ids() {
        let payload = format!(
            r#"{{"session_id":"{ID}","transcript_path":"/t/x.jsonl","cwd":"/w","hook_event_name":"SessionStart","source":"startup"}}"#
        );
        assert_eq!(Claude.parse_capture(&payload).as_deref(), Some(ID));

        assert_eq!(Claude.parse_capture(r#"{"session_id":"NOT-VALID"}"#), None);
        assert_eq!(
            Claude.parse_capture(r#"{"session_id":"x'; rm -rf ~'"}"#),
            None
        );
        assert_eq!(Claude.parse_capture("not json"), None);
        assert_eq!(Claude.parse_capture("{}"), None);
    }

    #[test]
    fn scrape_exit_takes_the_last_hint() {
        let text = format!(
            "Resume this session with:\nclaude --resume {OTHER}\n...\n\
             Resume this session with:\nclaude --resume {ID}\n"
        );
        assert_eq!(Claude.scrape_exit(&text).as_deref(), Some(ID));

        assert_eq!(Claude.scrape_exit("no hint here"), None);
        // A hint whose ID fails validation returns nothing.
        assert_eq!(Claude.scrape_exit("claude --resume NOT-A-UUID"), None);
        // A longer hexadecimal run is not an ID.
        assert_eq!(Claude.scrape_exit(&format!("claude --resume {ID}ff")), None);
    }

    #[test]
    fn correlate_fs_requires_a_unique_in_window_transcript() {
        let home = temp("claude_correlate");
        // Slug: `/` and `.` both become `-`.
        let cwd = Path::new("/a/b.c");
        let dir = home.join("projects").join("-a-b-c");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{ID}.jsonl")), "{}").unwrap();
        let now = SystemTime::now();

        assert_eq!(
            Claude.correlate_fs(cwd, now, Some(&home)).as_deref(),
            Some(ID)
        );
        // Outside the window: the transcript predates the spawn by minutes.
        let late = now + std::time::Duration::from_secs(120);
        assert_eq!(Claude.correlate_fs(cwd, late, Some(&home)), None);
        // Wrong project directory.
        assert_eq!(
            Claude.correlate_fs(Path::new("/other"), now, Some(&home)),
            None
        );

        // A second in-window transcript makes the match ambiguous.
        fs::write(dir.join(format!("{OTHER}.jsonl")), "{}").unwrap();
        assert_eq!(Claude.correlate_fs(cwd, now, Some(&home)), None);
    }

    #[test]
    fn correlate_fs_rejects_a_unique_non_uuid_stem() {
        let home = temp("claude_nonuuid");
        let cwd = Path::new("/w");
        let dir = home.join("projects").join("-w");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("agent-notes.jsonl"), "{}").unwrap();
        assert_eq!(
            Claude.correlate_fs(cwd, SystemTime::now(), Some(&home)),
            None
        );
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

    /// The process start rejects a stale record with a matching PID and CWD.
    #[test]
    fn record_for_pid_rejects_a_recycled_pids_stale_record() {
        let home = temp("claude_registry_recycled");
        let cwd = Path::new("/w");
        install_record(
            &home,
            4242,
            &record(4242, ID, "/w", LIVE_STARTED, "interactive", ""),
        );

        // The correlation window includes both endpoints.
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

    /// The scraper recovers the exit-hint ID from the corpus terminal bytes.
    #[test]
    fn corpus_scrape_recovers_the_exit_hint_id() {
        assert_corpus_scrape(
            &Claude,
            include_bytes!("../../tests/corpus/claude_resume.bin"),
            "c8c4a5cc-0b32-4ba0-a6b4-6ed08c218e0d",
        );
    }
}
