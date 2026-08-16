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
//!
//! # The store
//!
//! Sessions live at `<sessions root>/<encoded cwd>/<iso ts>_<uuidv7>.jsonl`.
//! The harness home *is* the sessions root: `PI_CODING_AGENT_SESSION_DIR`
//! names a sessions directory outright, so no agent-dir value can express it.
//! That override also flattens the store — it is passed straight through as the
//! session file's parent and the bucket level is never computed — so
//! correlation scans the root and one level below it.
//!
//! Correlation does not derive bucket names. It enumerates the root and its
//! immediate subdirectories, then verifies the working directory from each
//! session header.

use std::{
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    time::SystemTime,
};

use super::{
    CAPTURE_ENV, CapturePaths, Harness, Invocation, SpawnPlan, is_uuid, leading_uuid, shell_quote,
    within_window_ms,
};

/// Command fragment shared by ordinary exit and recovery hints.
const RESUME_HINT: &str = "omp --resume ";

/// Label identifying the resumable session in a recovery block.
const MAIN_LABEL: &str = "Main";

pub struct Omp;

impl Harness for Omp {
    /// The only variable that names the sessions root directly.
    fn home_env_var(&self) -> &'static str {
        "PI_CODING_AGENT_SESSION_DIR"
    }

    /// Default sessions path relative to `$HOME`.
    fn home_dot_dir(&self) -> &'static str {
        ".omp/agent/sessions"
    }

    /// Resolve omp's sessions root, not its agent directory.
    /// `PI_CODING_AGENT_SESSION_DIR` names the root directly; other inputs name
    /// or construct its parent directories.
    ///
    /// Precedence: `PI_CODING_AGENT_SESSION_DIR`; an unprofiled
    /// `PI_CODING_AGENT_DIR`; an existing XDG store; then the config path under
    /// `$HOME`. `OMP_PROFILE` is selected by presence, so an empty value still
    /// suppresses `PI_PROFILE`. Empty directory overrides are treated as unset.
    fn resolve_home(&self, env: &dyn Fn(&str) -> Option<PathBuf>) -> Option<PathBuf> {
        let set = |key: &str| env(key).filter(|p| !p.as_os_str().is_empty());
        if let Some(sessions) = set(self.home_env_var()) {
            return Some(sessions);
        }

        // Presence of `OMP_PROFILE` decides; an empty value selects no profile
        // and still shadows `PI_PROFILE`.
        let profile = match env("OMP_PROFILE") {
            Some(p) => p,
            None => env("PI_PROFILE").unwrap_or_default(),
        };
        // Trim profile names; empty and `default` select the unprofiled store.
        let profile = profile
            .to_str()
            .map(str::trim)
            .filter(|p| !p.is_empty() && *p != "default")
            .map(PathBuf::from);

        // Named profiles ignore `PI_CODING_AGENT_DIR`.
        if let (None, Some(agent)) = (&profile, set("PI_CODING_AGENT_DIR")) {
            return Some(agent.join("sessions"));
        }

        // XDG redirects only when the target path already exists.
        if let Some(xdg) = set("XDG_DATA_HOME") {
            let data = match &profile {
                Some(p) => xdg.join("omp").join("profiles").join(p),
                None => xdg.join("omp"),
            };
            if data.exists() {
                return Some(data.join("sessions"));
            }
        }

        // Only the config-path branch requires `$HOME`; earlier overrides are
        // complete paths. An absolute `PI_CONFIG_DIR` replaces `$HOME` under
        // `Path::join`.
        let config = env("HOME")?.join(set("PI_CONFIG_DIR").unwrap_or_else(|| ".omp".into()));
        let root = match &profile {
            Some(p) => config.join("profiles").join(p),
            None => config,
        };
        Some(root.join("agent").join("sessions"))
    }

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
        let v = jzon::parse(payload).ok()?;
        let id = v["sessionId"].as_str()?;
        is_uuid(id).then(|| id.to_string())
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

    /// Return the sole in-window session whose header names `cwd`. Scan the
    /// sessions root and each immediate subdirectory to cover flat and bucketed
    /// stores. The ID follows the last `_` in the filename.
    fn correlate_fs(&self, cwd: &Path, spawned: SystemTime, home: Option<&Path>) -> Option<String> {
        let sessions = self.home_root(home)?;
        let spawn_ms = spawned
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()?
            .as_millis();
        // Session headers may record either the supplied or canonical path.
        let canon = cwd.canonicalize().ok();

        // The default store is bucketed; `PI_CODING_AGENT_SESSION_DIR` is flat.
        let mut dirs = vec![sessions.clone()];
        dirs.extend(
            fs::read_dir(&sessions)
                .ok()?
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| e.path()),
        );

        let mut survivors: Vec<String> = Vec::new();
        for dir in dirs {
            let Ok(entries) = fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(id) = name
                    .to_str()
                    .and_then(|n| n.strip_suffix(".jsonl"))
                    .and_then(|stem| stem.rsplit_once('_'))
                    .map(|(_, id)| id)
                    .filter(|id| is_uuid(id))
                else {
                    continue;
                };
                // Only UUIDv7 provides the creation instant used for matching.
                let Some(ms) = v7_millis(id) else { continue };
                if !within_window_ms(u128::from(ms), spawn_ms) {
                    continue;
                }
                if !header_cwd_matches(&entry.path(), cwd, canon.as_deref()) {
                    continue;
                }
                // The same session may appear in multiple buckets; count its
                // UUID once.
                if !survivors.iter().any(|s| s == id) {
                    survivors.push(id.to_string());
                }
            }
        }
        match survivors.as_slice() {
            [only] => Some(only.clone()),
            _ => None,
        }
    }
}

/// Milliseconds embedded in the first 48 bits of a UUIDv7: the session's
/// creation instant. `None` when `id` is not v7. `id` must already satisfy
/// [`is_uuid`], which fixes its length and alphabet.
fn v7_millis(id: &str) -> Option<u64> {
    if id.as_bytes()[14] != b'7' {
        return None;
    }
    u64::from_str_radix(&format!("{}{}", &id[..8], &id[9..13]), 16).ok()
}

/// Whether either of the first two records is a session header naming `cwd`,
/// its canonical form, or a path with the same canonical target. The optional
/// first record is a fixed-width title slot. Nothing later can affect
/// correlation, so transcripts are not read beyond the header.
fn header_cwd_matches(path: &Path, cwd: &Path, canon: Option<&Path>) -> bool {
    let Ok(file) = fs::File::open(path) else {
        return false;
    };
    let mut reader = BufReader::new(file.take(64 * 1024));
    let mut line = String::new();
    for _ in 0..2 {
        line.clear();
        if !matches!(reader.read_line(&mut line), Ok(n) if n > 0) {
            return false;
        }
        let Ok(record) = jzon::parse(&line) else {
            continue;
        };
        if record["type"].as_str() != Some("session") {
            continue;
        }
        return record["cwd"].as_str().is_some_and(|c| {
            let header = Path::new(c);
            header == cwd
                || Some(header) == canon
                || canon.is_some_and(|canon| header.canonicalize().is_ok_and(|h| h == canon))
        });
    }
    false
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::{
        harness::fixtures::{ID, OTHER, assert_all_opaque, assert_corpus_scrape, paths},
        testutil::{temp, v7_at},
    };

    /// Spawn instant shared by the correlation tests: 2026-08-15T22:13:20Z.
    const SPAWN_MS: u64 = 1_786_000_000_000;
    /// Working directory recorded in the generated headers.
    const CWD: &str = "/work/proj";
    /// Valid UUIDv7 used in capture payloads.
    const CAPTURED: &str = "01a0077c-e18e-7000-ae0b-016f4834b6e9";

    fn spawned() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_millis(SPAWN_MS)
    }

    /// Fixed-width title record preceding a session header.
    fn title_slot() -> String {
        let head = concat!(
            r#"{"type":"title","v":1,"title":"Run ls -la","source":"auto","#,
            r#""updatedAt":"2026-08-15T22:19:51.048Z","pad":""#
        );
        let tail = r#""}"#;
        format!("{head}{}{tail}", " ".repeat(256 - head.len() - tail.len()))
    }

    /// Write a session file under `<sessions>/<bucket>/<file>`, optionally
    /// behind the title slot.
    fn write_named(sessions: &Path, bucket: &str, file: &str, id: &str, cwd: &str, slot: bool) {
        let dir = sessions.join(bucket);
        fs::create_dir_all(&dir).unwrap();
        let mut body = String::new();
        if slot {
            body.push_str(&title_slot());
            body.push('\n');
        }
        body.push_str(&format!(
            r#"{{"type":"session","version":3,"id":"{id}","timestamp":"2026-08-15T22:19:51.048Z","cwd":"{cwd}","title":"Run ls -la"}}"#
        ));
        // Transcript content after the header does not participate.
        body.push_str("\n{\"type\":\"message\",\"role\":\"assistant\"}\n");
        fs::write(dir.join(file), body).unwrap();
    }

    /// Write a session under `<iso ts>_<id>.jsonl`.
    fn write_session(sessions: &Path, bucket: &str, id: &str, cwd: &str, slot: bool) {
        let file = format!("2026-08-15T22-19-51-048Z_{id}.jsonl");
        write_named(sessions, bucket, &file, id, cwd, slot);
    }

    /// Resolve omp's sessions root against a synthetic launch environment.
    fn home(env: &[(&str, &str)]) -> Option<PathBuf> {
        Omp.resolve_home(&|key| {
            env.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| PathBuf::from(value))
        })
    }

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

    /// One in-window session whose header names the task's cwd correlates;
    /// another directory, another window, and a second candidate do not.
    #[test]
    fn correlate_fs_requires_a_unique_in_window_session_for_the_cwd() {
        let sessions = temp("omp_correlate");
        let id = v7_at(SPAWN_MS + 4_000, 1);
        write_session(&sessions, "bucket", &id, CWD, true);
        assert_eq!(
            Omp.correlate_fs(Path::new(CWD), spawned(), Some(&sessions))
                .as_deref(),
            Some(id.as_str())
        );

        // The header decides the directory, so another cwd matches nothing.
        assert_eq!(
            Omp.correlate_fs(Path::new("/elsewhere"), spawned(), Some(&sessions)),
            None
        );

        // A session minted 90 s later falls outside the window.
        let late = v7_at(SPAWN_MS + 90_000, 2);
        write_session(&sessions, "late", &late, "/late/proj", true);
        assert_eq!(
            Omp.correlate_fs(Path::new("/late/proj"), spawned(), Some(&sessions)),
            None
        );

        // Two in-window sessions for one directory cannot be told apart.
        write_session(&sessions, "bucket", &v7_at(SPAWN_MS + 8_000, 3), CWD, true);
        assert_eq!(
            Omp.correlate_fs(Path::new(CWD), spawned(), Some(&sessions)),
            None
        );
    }

    /// `PI_CODING_AGENT_SESSION_DIR` becomes the session file's parent
    /// directly, so its store carries no bucket level. One scan covers both
    /// layouts: a file in the root and a file one level down, in the same
    /// root, each correlating for its own header cwd.
    #[test]
    fn correlate_fs_reads_a_flat_store_and_a_bucketed_one() {
        let sessions = temp("omp_layouts");
        // An empty bucket name writes into the root itself.
        let flat = v7_at(SPAWN_MS + 3_000, 8);
        write_session(&sessions, "", &flat, CWD, true);
        let nested = v7_at(SPAWN_MS + 5_000, 9);
        write_session(&sessions, "bucket", &nested, "/work/other", true);

        assert_eq!(
            Omp.correlate_fs(Path::new(CWD), spawned(), Some(&sessions))
                .as_deref(),
            Some(flat.as_str())
        );
        assert_eq!(
            Omp.correlate_fs(Path::new("/work/other"), spawned(), Some(&sessions))
                .as_deref(),
            Some(nested.as_str())
        );
        // The header still decides the directory in either layout.
        assert_eq!(
            Omp.correlate_fs(Path::new("/elsewhere"), spawned(), Some(&sessions)),
            None
        );
    }

    /// The same session ID in two buckets remains one candidate.
    #[test]
    fn correlate_fs_collapses_one_session_seen_in_two_buckets() {
        let sessions = temp("omp_dupe");
        let id = v7_at(SPAWN_MS + 6_000, 10);
        write_session(&sessions, "encoded", &id, CWD, true);
        write_session(&sessions, "--legacy--", &id, CWD, true);
        assert_eq!(
            Omp.correlate_fs(Path::new(CWD), spawned(), Some(&sessions))
                .as_deref(),
            Some(id.as_str())
        );
    }

    /// A header path alias matches the task's canonical working directory.
    #[test]
    fn correlate_fs_matches_a_header_holding_an_alias_of_the_cwd() {
        let tmp = temp("omp_alias");
        let (real, alias) = (tmp.join("real"), tmp.join("alias"));
        fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let physical = real.canonicalize().unwrap();
        let sessions = tmp.join("sessions");
        let id = v7_at(SPAWN_MS + 7_000, 11);
        write_session(&sessions, "bucket", &id, alias.to_str().unwrap(), true);
        assert_eq!(
            Omp.correlate_fs(&physical, spawned(), Some(&sessions))
                .as_deref(),
            Some(id.as_str())
        );
    }

    /// A session header may occupy the first line when no title record exists.
    #[test]
    fn correlate_fs_reads_a_legacy_file_whose_header_is_line_one() {
        let sessions = temp("omp_legacy");
        let id = v7_at(SPAWN_MS + 1_000, 5);
        write_session(&sessions, "bucket", &id, CWD, false);
        assert_eq!(
            Omp.correlate_fs(Path::new(CWD), spawned(), Some(&sessions))
                .as_deref(),
            Some(id.as_str())
        );
    }

    /// Malformed filenames, non-v7 IDs, and empty buckets contribute nothing.
    #[test]
    fn correlate_fs_refuses_names_and_buckets_it_cannot_read() {
        let sessions = temp("omp_names");
        // UUIDv4 embeds no creation instant.
        let v4 = format!("2026-08-15T22-19-51-048Z_{ID}.jsonl");
        write_named(&sessions, "v4", &v4, ID, CWD, true);
        // Without a `_` nothing marks where the id starts.
        let good = v7_at(SPAWN_MS + 1_000, 6);
        write_named(
            &sessions,
            "nosep",
            &format!("{good}.jsonl"),
            &good,
            CWD,
            true,
        );
        // An empty bucket contributes no candidate.
        fs::create_dir_all(sessions.join("empty")).unwrap();
        assert_eq!(
            Omp.correlate_fs(Path::new(CWD), spawned(), Some(&sessions)),
            None
        );

        // A valid filename remains the sole candidate.
        write_session(&sessions, "good", &good, CWD, true);
        assert_eq!(
            Omp.correlate_fs(Path::new(CWD), spawned(), Some(&sessions))
                .as_deref(),
            Some(good.as_str())
        );
    }

    /// Canonical cwd comparison lets a task launched through a symlink match.
    #[test]
    fn correlate_fs_matches_a_symlinked_cwd_through_its_canonical_form() {
        let tmp = temp("omp_canon");
        let (real, link) = (tmp.join("real"), tmp.join("link"));
        fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let canonical = real.canonicalize().unwrap();
        let sessions = tmp.join("sessions");
        let id = v7_at(SPAWN_MS + 2_000, 4);
        write_session(&sessions, "bucket", &id, canonical.to_str().unwrap(), true);
        assert_eq!(
            Omp.correlate_fs(&link, spawned(), Some(&sessions))
                .as_deref(),
            Some(id.as_str())
        );
    }

    /// Every branch of the chain resolves to a sessions root.
    #[test]
    fn resolve_home_walks_the_store_root_chain() {
        assert_eq!(
            home(&[("HOME", "/h")]),
            Some("/h/.omp/agent/sessions".into())
        );

        // The session dir names the store outright and bypasses the rest.
        assert_eq!(
            home(&[
                ("HOME", "/h"),
                ("PI_CONFIG_DIR", ".alt"),
                ("PI_CODING_AGENT_DIR", "/a"),
                ("PI_CODING_AGENT_SESSION_DIR", "/s"),
            ]),
            Some("/s".into())
        );
        // Set but empty is unset.
        assert_eq!(
            home(&[("HOME", "/h"), ("PI_CODING_AGENT_SESSION_DIR", "")]),
            Some("/h/.omp/agent/sessions".into())
        );

        // The agent dir replaces `<config root>/agent` whole.
        assert_eq!(
            home(&[("HOME", "/h"), ("PI_CODING_AGENT_DIR", "/a")]),
            Some("/a/sessions".into())
        );
        // A selected profile ignores it.
        assert_eq!(
            home(&[
                ("HOME", "/h"),
                ("PI_CODING_AGENT_DIR", "/a"),
                ("PI_PROFILE", "work"),
            ]),
            Some("/h/.omp/profiles/work/agent/sessions".into())
        );
        // Trimmed `default` selects the unprofiled store.
        assert_eq!(
            home(&[("HOME", "/h"), ("OMP_PROFILE", "default")]),
            Some("/h/.omp/agent/sessions".into())
        );
        assert_eq!(
            home(&[
                ("HOME", "/h"),
                ("PI_PROFILE", " default "),
                ("PI_CODING_AGENT_DIR", "/a"),
            ]),
            Some("/a/sessions".into())
        );
        assert_eq!(
            home(&[("HOME", "/h"), ("OMP_PROFILE", "  work  ")]),
            Some("/h/.omp/profiles/work/agent/sessions".into())
        );
        // Whitespace alone trims to nothing, which is no profile.
        assert_eq!(
            home(&[("HOME", "/h"), ("OMP_PROFILE", "   ")]),
            Some("/h/.omp/agent/sessions".into())
        );

        // `OMP_PROFILE` wins by presence: it selects when non-empty and
        // suppresses `PI_PROFILE` when empty.
        assert_eq!(
            home(&[("HOME", "/h"), ("OMP_PROFILE", "a"), ("PI_PROFILE", "b")]),
            Some("/h/.omp/profiles/a/agent/sessions".into())
        );
        assert_eq!(
            home(&[
                ("HOME", "/h"),
                ("OMP_PROFILE", ""),
                ("PI_PROFILE", "b"),
                ("PI_CODING_AGENT_DIR", "/a"),
            ]),
            Some("/a/sessions".into())
        );

        // `PI_CONFIG_DIR` replaces the default config-directory name.
        assert_eq!(
            home(&[("HOME", "/h"), ("PI_CONFIG_DIR", ".alt")]),
            Some("/h/.alt/agent/sessions".into())
        );
        // An absolute value replaces `$HOME` under `Path::join`.
        assert_eq!(
            home(&[("HOME", "/h"), ("PI_CONFIG_DIR", "/abs")]),
            Some("/abs/agent/sessions".into())
        );

        // A complete agent-directory override does not require `HOME`.
        assert_eq!(
            home(&[("PI_CODING_AGENT_DIR", "/a")]),
            Some("/a/sessions".into())
        );

        // `home_root` applies the platform fallback when no path resolves.
        assert_eq!(home(&[]), None);
    }

    /// The XDG redirect requires an existing target and drops `agent/`.
    #[test]
    fn resolve_home_redirects_to_xdg_only_when_that_directory_exists() {
        let dir = temp("omp_xdg");
        let xdg = dir.to_str().unwrap();

        assert_eq!(
            home(&[("HOME", "/h"), ("XDG_DATA_HOME", xdg)]),
            Some("/h/.omp/agent/sessions".into())
        );
        fs::create_dir_all(dir.join("omp")).unwrap();
        assert_eq!(
            home(&[("HOME", "/h"), ("XDG_DATA_HOME", xdg)]),
            Some(dir.join("omp/sessions"))
        );

        // With a profile, the profile target must exist.
        let profile = [
            ("HOME", "/h"),
            ("XDG_DATA_HOME", xdg),
            ("OMP_PROFILE", "work"),
        ];
        assert_eq!(
            home(&profile),
            Some("/h/.omp/profiles/work/agent/sessions".into())
        );
        fs::create_dir_all(dir.join("omp/profiles/work")).unwrap();
        assert_eq!(home(&profile), Some(dir.join("omp/profiles/work/sessions")));

        // An agent dir named outright is never redirected.
        assert_eq!(
            home(&[
                ("HOME", "/h"),
                ("XDG_DATA_HOME", xdg),
                ("PI_CODING_AGENT_DIR", "/a"),
            ]),
            Some("/a/sessions".into())
        );

        // The redirect target is absolute, so it too answers without `HOME`.
        assert_eq!(
            home(&[("XDG_DATA_HOME", xdg)]),
            Some(dir.join("omp/sessions"))
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
