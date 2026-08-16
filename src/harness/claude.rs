//! Claude exposes four useful session signals: a launch-time `--session-id`, a
//! `SessionStart` hook, a live session registry, and an exit-time resume hint.
//! Bare launches pin a v4 UUID; every accepted launch receives the hook through
//! `--settings`. The CLI itself publishes one `<claude-home>/sessions/<pid>.json`
//! record per live session, with no instrumentation. The filesystem fallback
//! correlates `<claude-home>/projects/<cwd-slug>/<uuid>.jsonl` transcripts.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use super::{
    CAPTURE_ENV, CapturePaths, Harness, Invocation, SpawnPlan, is_uuid, last_hint, pin_plan,
    shell_quote, unique_in_window, within_window_ms,
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
        Some(record_for_pid(home, pid, cwd, spawned)?.id)
    }

    fn live_blocked_status(
        &self,
        pid: u32,
        cwd: &Path,
        spawned: SystemTime,
        home: Option<&Path>,
    ) -> Option<(String, &'static str)> {
        let rec = record_for_pid(home, pid, cwd, spawned)?;
        // The waiting state alone: see the trait doc. The registry beats the
        // screen to this one state by about a second and reports it at any
        // terminal width and for every dialog shape, including the ones
        // `ClaudeSummary`'s `❯ 1. `/`2. ` selector match does not cover.
        rec.waiting
            .then(|| waiting_preview(rec.waiting_for.as_deref()))
    }

    fn correlate_fs(&self, cwd: &Path, spawned: SystemTime, home: Option<&Path>) -> Option<String> {
        let dir = self.home_root(home)?.join("projects").join(slug(cwd)?);
        unique_in_window(dir, spawned, |entry| {
            // A transcript's stem is its session ID.
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                return None;
            }
            Some(path.file_stem()?.to_str()?.to_string())
        })
    }
}

/// One record from the live session registry. The CLI writes it on launch and
/// rewrites it in place as the session changes; it removes it on a clean exit
/// but leaves it behind when the process dies on a signal, so a record on disk
/// is a claim about a pid, not proof of a live session.
struct SessionRecord {
    /// `sessionId`, already through [`is_uuid`].
    id: String,
    /// `pid`, which also names the record's file.
    pid: i32,
    /// `cwd` the session runs in.
    cwd: PathBuf,
    /// `startedAt`: the process's start in epoch milliseconds. `procStart`
    /// names the same instant in human-readable form.
    started_at: u128,
    /// `status` reading `waiting`: the CLI blocked on the user. That is the
    /// only value any caller acts on, so the rest of the vocabulary, a value
    /// this reader predates, and the absent field non-interactive entrypoints
    /// write all collapse to `false` without invalidating the record.
    waiting: bool,
    /// `waitingFor`: why a waiting session waits. Present only while the CLI
    /// holds a dialog open.
    waiting_for: Option<String>,
}

/// Preview text and matcher ID for a `waiting` record's `waitingFor` reason.
/// The CLI's dialog-label map spells five reasons: `permission prompt` (its
/// default for any dialog), `input needed`, `dialog open`, `sandbox request`,
/// and `worker request`. Only the first is rewritten, to the string
/// `ClaudeSummary::claude_approval` already synthesizes for the same
/// condition; the rest are claude's own words and are kept verbatim, as is any
/// reason a later version adds. A record that reports `waiting` without a
/// reason still names a user-blocking state, so it renders as one.
fn waiting_preview(reason: Option<&str>) -> (String, &'static str) {
    match reason.filter(|r| !r.is_empty()) {
        Some("permission prompt") => ("awaiting approval".to_string(), "claude:registry-approval"),
        Some(other) => (other.to_string(), "claude:registry-waiting"),
        None => ("awaiting input".to_string(), "claude:registry-waiting"),
    }
}

/// Parse one registry record. The CLI rewrites the file in place with a plain
/// write rather than a temp-and-rename, so a reader can catch it truncated:
/// unparseable text yields `None` and the caller simply has no evidence this
/// time. `bg`, `daemon`, and `daemon-worker` records name conversations no user
/// is driving, so only `interactive` survives.
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

/// Read the record `pid` publishes, requiring it to name that pid, that `cwd`,
/// and a process started within [`super::CORRELATE_WINDOW`] of `spawned`.
///
/// A live task's pid cannot be reissued to a foreign `claude`:
/// [`crate::task::Task::poll_exit`] reaps with `WNOWAIT` and leaves the exited
/// leader a zombie, which holds the pid for the task's whole life. So
/// `sessions/<pid>.json` is this task's own record or nothing — that, not the
/// field checks, is what keeps a stranger out.
///
/// The `cwd` and `startedAt` guards close what the reservation cannot: a record
/// an *earlier* process at that pid left behind, before this task existed. The
/// CLI removes its record on a clean exit, but a signal-killed `claude` leaves
/// it and only the next `claude` launch sweeps it. `cwd` separates two
/// directories; `startedAt` separates two processes in one directory. Both cost
/// less than the read that produced the record and sit at a shell-command
/// boundary, so they stay. That window does not decay with session age, because
/// `startedAt` records the process start: `/clear` mints a fresh `sessionId` in
/// place and leaves `startedAt` untouched, so a session running for hours still
/// matches its original spawn instant.
///
/// Call-site details: `/cd` inside claude moves the session's `cwd` and fails
/// this check, which loses the record. Failing closed there is deliberate.
fn record_for_pid(
    home: Option<&Path>,
    pid: u32,
    cwd: &Path,
    spawned: SystemTime,
) -> Option<SessionRecord> {
    let pid = i32::try_from(pid).ok()?;
    let dir = Claude.home_root(home)?.join("sessions");
    let text = fs::read_to_string(dir.join(format!("{pid}.json"))).ok()?;
    let rec = parse_record(&text)?;
    let spawned_ms = spawned.duration_since(UNIX_EPOCH).ok()?.as_millis();
    // Same path through two aliases is one directory: claude records
    // `process.cwd()`, which is `getcwd(3)` and so symlink-resolved, while a
    // task carries the path it was spawned with. `canonicalize` is IO and fails
    // on a vanished directory, which leaves the verbatim comparison standing —
    // a cwd matching neither form is still refused.
    let same_cwd = rec.cwd == cwd || cwd.canonicalize().is_ok_and(|c| rec.cwd == c);
    (rec.pid == pid && same_cwd && within_window_ms(rec.started_at, spawned_ms)).then_some(rec)
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

    /// One record a live `claude` 2.1.233 published. Field order and spelling
    /// are as written; its `sessionId` is [`OTHER`], and the middle of `cwd` is
    /// elided, which the reader never inspects.
    const LIVE_RECORD: &str = concat!(
        r#"{"pid":83849,"sessionId":"11111111-2222-4333-8444-555555555555","#,
        r#""cwd":"/private/tmp/.../scratchpad/live-claude","startedAt":1786834960302,"#,
        r#""procStart":"Sat Aug 15 23:02:39 2026","version":"2.1.233","peerProtocol":1,"#,
        r#""kind":"interactive","entrypoint":"cli","#,
        r#""messagingSocketPath":"/tmp/cc-socks/83849.sock","#,
        r#""name":"live-claude-66","nameSource":"derived","nameSince":1786834960303,"#,
        r#""status":"idle","updatedAt":1786834960352,"statusUpdatedAt":1786834960352}"#,
    );
    /// The pid, directory, and process start [`LIVE_RECORD`] names.
    const LIVE_PID: u32 = 83849;
    const LIVE_CWD: &str = "/private/tmp/.../scratchpad/live-claude";
    const LIVE_STARTED: u64 = 1_786_834_960_302;

    /// A registry record carrying every field the reader validates. `tail`
    /// appends raw JSON for the optional status pair.
    fn record(pid: i32, id: &str, cwd: &str, started: u64, kind: &str, tail: &str) -> String {
        format!(
            r#"{{"pid":{pid},"sessionId":"{id}","cwd":"{cwd}","startedAt":{started},"version":"2.1.233","kind":"{kind}","entrypoint":"cli"{tail}}}"#
        )
    }

    /// File `body` as the registry record for `pid`, creating the store.
    fn install_record(home: &Path, pid: i32, body: &str) {
        let dir = home.join("sessions");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{pid}.json")), body).unwrap();
    }

    /// The instant `ms` epoch milliseconds names.
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

    /// The record a live session published parses whole, and the harness
    /// surfaces its ID through the trait.
    #[test]
    fn record_for_pid_reads_a_live_record() {
        let home = temp("claude_registry");
        install_record(&home, LIVE_PID as i32, LIVE_RECORD);
        let cwd = Path::new(LIVE_CWD);
        let rec = record_for_pid(Some(&home), LIVE_PID, cwd, at_ms(LIVE_STARTED))
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

    /// The record must claim the pid whose file it sits in and the directory
    /// the task runs in.
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
        assert!(record_for_pid(Some(&home), 4242, cwd, spawned).is_some());

        // A record filed under one pid while naming another is not this task's.
        install_record(
            &home,
            4242,
            &record(99, ID, "/w", LIVE_STARTED, "interactive", ""),
        );
        assert!(record_for_pid(Some(&home), 4242, cwd, spawned).is_none());

        install_record(
            &home,
            4242,
            &record(4242, ID, "/elsewhere", LIVE_STARTED, "interactive", ""),
        );
        assert!(record_for_pid(Some(&home), 4242, cwd, spawned).is_none());
    }

    /// Claude records `process.cwd()`, which `getcwd(3)` already resolved
    /// through every symlink; the task carries the path it was spawned with.
    /// One directory reached two ways still matches.
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
        assert!(record_for_pid(Some(&home), 7, &link, spawned).is_some());
        // A real directory that is not an alias of the record's is still
        // refused, as is one that no longer exists to canonicalize.
        assert!(record_for_pid(Some(&home), 7, &other, spawned).is_none());
        assert!(record_for_pid(Some(&home), 7, &tmp.join("gone"), spawned).is_none());
    }

    /// A `claude` killed by a signal leaves its record behind until the next
    /// launch sweeps it. A task later assigned that pid in the same directory
    /// satisfies both identity guards, so the process start is what rejects it.
    #[test]
    fn record_for_pid_rejects_a_recycled_pids_stale_record() {
        let home = temp("claude_registry_recycled");
        let cwd = Path::new("/w");
        install_record(
            &home,
            4242,
            &record(4242, ID, "/w", LIVE_STARTED, "interactive", ""),
        );

        // The same process: its start is inside the correlation window.
        assert!(record_for_pid(Some(&home), 4242, cwd, at_ms(LIVE_STARTED + 30_000)).is_some());
        assert!(record_for_pid(Some(&home), 4242, cwd, at_ms(LIVE_STARTED - 30_000)).is_some());
        // A later process under the recycled pid: minutes apart, or one
        // millisecond outside the window.
        assert!(record_for_pid(Some(&home), 4242, cwd, at_ms(LIVE_STARTED + 30_001)).is_none());
        assert!(record_for_pid(Some(&home), 4242, cwd, at_ms(LIVE_STARTED + 600_000)).is_none());
    }

    /// Only an `interactive` record names a conversation a user is driving,
    /// and only a strict UUID may leave the reader.
    #[test]
    fn record_for_pid_requires_an_interactive_kind_and_a_strict_id() {
        let home = temp("claude_registry_kind");
        let cwd = Path::new("/w");
        let spawned = at_ms(LIVE_STARTED);
        for kind in ["bg", "daemon", "daemon-worker"] {
            install_record(&home, 7, &record(7, ID, "/w", LIVE_STARTED, kind, ""));
            assert!(
                record_for_pid(Some(&home), 7, cwd, spawned).is_none(),
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
                record_for_pid(Some(&home), 7, cwd, spawned).is_none(),
                "{id:?}"
            );
        }
        // A record missing `kind` is unclassifiable.
        install_record(
            &home,
            7,
            &format!(r#"{{"pid":7,"sessionId":"{ID}","cwd":"/w","startedAt":{LIVE_STARTED}}}"#),
        );
        assert!(record_for_pid(Some(&home), 7, cwd, spawned).is_none());
    }

    /// The CLI rewrites the record in place rather than renaming a temporary,
    /// so a reader can catch it truncated. That, an absent record, and an
    /// absent store all mean no evidence this time.
    #[test]
    fn record_for_pid_tolerates_a_torn_file_and_a_missing_store() {
        let home = temp("claude_registry_torn");
        let cwd = Path::new(LIVE_CWD);
        let spawned = at_ms(LIVE_STARTED);
        for body in [&LIVE_RECORD[..LIVE_RECORD.len() / 2], "", "\0"] {
            install_record(&home, LIVE_PID as i32, body);
            assert!(
                record_for_pid(Some(&home), LIVE_PID, cwd, spawned).is_none(),
                "{body:?}"
            );
        }
        // No record for this pid, and no store at all.
        assert!(record_for_pid(Some(&home), 1, cwd, spawned).is_none());
        let bare = temp("claude_registry_bare");
        assert!(record_for_pid(Some(&bare), LIVE_PID, cwd, spawned).is_none());
    }

    /// Only `waiting` is read, so a status absent or from a vocabulary this
    /// reader predates leaves the record valid and its ID usable.
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
            let rec = record_for_pid(Some(&home), 7, cwd, spawned).expect("the record must parse");
            assert_eq!(rec.id, ID, "{tail:?}");
            assert!(!rec.waiting, "{tail:?}");
        }
    }

    /// The blocked-status probe speaks the `waitingFor` vocabulary the CLI's
    /// own dialog-label map defines. `permission prompt` is its default for
    /// any dialog and is the one value rewritten, to the string the screen
    /// scraper synthesizes for the same condition; every other reason is
    /// claude's wording and survives verbatim, a reason this reader predates
    /// included. A `waiting` record with no reason still blocks the user.
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
            // Not in today's map: a later CLI version's wording is still
            // claude's own and reads better than a synthesized stand-in.
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

    /// Only `waiting` answers. `busy` and `shell` resolve to a title carrying
    /// claude's own per-turn summary, and `idle` to whatever the screen shows;
    /// replacing either with the bare status word would lose information. An
    /// absent status, an unreadable one, and a record that fails the identity
    /// guards are all no evidence.
    #[test]
    fn live_blocked_status_answers_for_waiting_alone() {
        let home = temp("claude_blocked_states");
        let cwd = Path::new("/w");
        let spawned = at_ms(LIVE_STARTED);
        for tail in [
            r#","status":"busy""#,
            r#","status":"shell""#,
            r#","status":"idle""#,
            r#","status":"hibernating""#,
            "",
            // A reason without the status it belongs to is not a claim.
            r#","waitingFor":"permission prompt""#,
        ] {
            install_record(
                &home,
                7,
                &record(7, ID, "/w", LIVE_STARTED, "interactive", tail),
            );
            assert_eq!(
                Claude.live_blocked_status(7, cwd, spawned, Some(&home)),
                None,
                "{tail:?}"
            );
        }
        // No record for this pid at all.
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
