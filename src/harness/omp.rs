//! omp cannot pin a session ID at launch: it has no `--session-id` flag, and
//! `--resume` rejects an ID that does not already exist, so a pinned UUID would
//! name a session the resume command could never reach. Capture therefore has
//! to come from omp itself, through two channels. Live: `-e` loads an
//! extension module in omp's own process, and its `session_start` and
//! `session_switch` handlers write the ID to the capture file. At exit: the
//! hint omp prints, `Resume this session with omp --resume <uuid>`. A crash
//! repeats the same command inside a `[Recovery]` block as `Main: omp --resume
//! <uuid>`; both carry the command substring, so one matcher reads both.
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
//!
//! The bucket name is deliberately not reproduced. omp encodes a cwd through
//! three scopes — under `$HOME`, under `os.tmpdir()`, otherwise absolute —
//! after realpath-canonicalising cwd, home, and `$TMPDIR`, and the scheme
//! changed three times inside the 17.2.x line, each change shipping an on-disk
//! migration. Reimplementing it would mean tracking those revisions forever.
//! Correlation therefore enumerates the buckets and confirms the cwd from the
//! file's own header, unlike `grok.rs`, which computes its group name.

use std::{
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    time::SystemTime,
};

use super::{
    CAPTURE_ENV, CapturePaths, Harness, Invocation, SpawnPlan, is_uuid, last_hint, shell_quote,
    within_window_ms,
};

pub struct Omp;

impl Harness for Omp {
    /// The only variable that names the sessions root outright. The rest of
    /// omp's chain builds that path instead of naming it, so it lives in
    /// [`Omp::resolve_home`].
    fn home_env_var(&self) -> &'static str {
        "PI_CODING_AGENT_SESSION_DIR"
    }

    /// Three components: the store sits two levels below the config root.
    /// `home_root`'s `join` keeps all of them.
    fn home_dot_dir(&self) -> &'static str {
        ".omp/agent/sessions"
    }

    /// Resolve omp's **sessions root** — not its agent directory.
    /// `PI_CODING_AGENT_SESSION_DIR` names a sessions directory directly, so
    /// no single agent-dir value could express every outcome of this chain,
    /// and the sessions root is the only level all four branches agree on.
    ///
    /// Precedence: the session-dir override wins verbatim; otherwise the agent
    /// directory is `$HOME/<PI_CONFIG_DIR, default .omp>[/profiles/<profile>]
    /// /agent`, which `PI_CODING_AGENT_DIR` replaces unless a profile is
    /// selected; and an XDG data directory that already exists on disk
    /// redirects the still-default agent directory, flattening the `agent/`
    /// level away. Only `OMP_PROFILE` is read by presence — omp lets an empty
    /// `OMP_PROFILE` suppress `PI_PROFILE`. Every other variable is read the
    /// way JavaScript truthiness reads it: empty means unset.
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
        let profile = Some(profile).filter(|p| !p.as_os_str().is_empty());

        // `PI_CONFIG_DIR` is a directory *name*, so the relative case is the
        // only one omp documents, and there the two agree. An absolute value
        // diverges: node's `path.join` concatenates it under `$HOME`, while
        // `Path::join` lets it replace `$HOME` outright. Correlation then
        // reads a store omp never wrote and finds nothing, which is the safe
        // direction — resume falls back to the launch command rather than
        // reopening some other conversation.
        let config = env("HOME")?.join(set("PI_CONFIG_DIR").unwrap_or_else(|| ".omp".into()));
        let root = match &profile {
            Some(p) => config.join("profiles").join(p),
            None => config,
        };

        // A named profile ignores `PI_CODING_AGENT_DIR`, and an agent
        // directory named that way is never redirected by XDG.
        if let (None, Some(agent)) = (&profile, set("PI_CODING_AGENT_DIR")) {
            return Some(agent.join("sessions"));
        }

        // The XDG redirect is conditional on the directory already existing:
        // omp checks the filesystem, whatever its own doc comment claims.
        if let Some(xdg) = set("XDG_DATA_HOME") {
            let data = match &profile {
                Some(p) => xdg.join("omp").join("profiles").join(p),
                None => xdg.join("omp"),
            };
            if data.exists() {
                return Some(data.join("sessions"));
            }
        }
        Some(root.join("agent").join("sessions"))
    }

    fn shape(&self) -> (&'static str, &'static str) {
        ("omp", "--resume")
    }

    /// Load the capture extension with `-e`, which omp accepts on both shapes
    /// and applies silently: no trust prompt, and the module is *appended* to
    /// the user's own extensions. `--trusted-extension` would fit the same
    /// slot and must never be used — it is mutually exclusive with `-e` and
    /// replaces the user's entire extension discovery, omp's own bridges
    /// included.
    fn instrument(
        &self,
        // Both accepted shapes take the same injection: omp has no
        // `--session-id`, so neither can pin an ID and only the extension
        // reports one.
        _inv: &Invocation,
        capture: &CapturePaths,
        // The module is self-contained and reads nothing from the store.
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

    /// Read the ID out of the extension's payload. The module writes one JSON
    /// object per event; anything else on that path came from somewhere else
    /// and is discarded.
    fn parse_capture(&self, payload: &str) -> Option<String> {
        let v = jzon::parse(payload).ok()?;
        let id = v["sessionId"].as_str()?;
        is_uuid(id).then(|| id.to_string())
    }

    fn scrape_exit(&self, text: &str) -> Option<String> {
        // The last valid hint names the session at exit. The exit line and the
        // crash recovery block print the same command.
        last_hint(text, &["omp --resume "])
    }

    /// Scan every cwd bucket for the one in-window session whose header names
    /// `cwd`. The id is the filename text after the last `_`: omp parses it
    /// that way, and neither the leading timestamp nor the id contains `_`.
    ///
    /// A just-launched session may legitimately have no file at all — omp
    /// keeps a session in memory until it holds an assistant message, so
    /// nothing is written before the model replies. A session with no reply
    /// has nothing worth resuming, so an empty bucket is not an error.
    fn correlate_fs(&self, cwd: &Path, spawned: SystemTime, home: Option<&Path>) -> Option<String> {
        let sessions = self.home_root(home)?;
        let spawn_ms = spawned
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()?
            .as_millis();
        // The bucket is named from the canonical cwd while the header records
        // the resolved-but-uncanonicalised one, so both forms must match.
        let canon = cwd.canonicalize().ok();
        let mut survivors: Vec<String> = Vec::new();
        for bucket in fs::read_dir(sessions).ok()?.flatten() {
            let Ok(entries) = fs::read_dir(bucket.path()) else {
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
                // An id omp did not mint carries no creation instant. Skip it
                // rather than guess one from the filename or the metadata.
                let Some(ms) = v7_millis(id) else { continue };
                if !within_window_ms(u128::from(ms), spawn_ms) {
                    continue;
                }
                if !header_cwd_matches(&entry.path(), cwd, canon.as_deref()) {
                    continue;
                }
                survivors.push(id.to_string());
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
///
/// `codex.rs` carries the same six lines. The duplication is deliberate — the
/// two stores are unrelated and neither module should own the other's
/// helper — and is marked here so a later reconciliation pass can find both.
fn v7_millis(id: &str) -> Option<u64> {
    if id.as_bytes()[14] != b'7' {
        return None;
    }
    u64::from_str_radix(&format!("{}{}", &id[..8], &id[9..13]), 16).ok()
}

/// Whether the session header names `cwd`, in either the given or the
/// canonical form. Line 1 is a fixed-width mutable title slot in current omp
/// and the header itself in older files, so both lines are tried and nothing
/// past them is read: transcripts grow to megabytes and only the header
/// participates.
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
            header == cwd || Some(header) == canon
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
    /// The ID omp 17.3.4 reported through the extension on 2026-08-15.
    const CAPTURED: &str = "01a0077c-e18e-7000-ae0b-016f4834b6e9";

    fn spawned() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_millis(SPAWN_MS)
    }

    /// omp's line 1: one 256-byte record whose `pad` field absorbs the slack,
    /// so a retitle rewrites the line in place without moving the header.
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
        // A real transcript continues past the header; correlation must not.
        body.push_str("\n{\"type\":\"message\",\"role\":\"assistant\"}\n");
        fs::write(dir.join(file), body).unwrap();
    }

    /// Write a session under omp's own `<iso ts>_<id>.jsonl` naming.
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

    /// The extension's payload yields an ID only when it validates; every
    /// other payload on that path came from somewhere else.
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
        // Uppercase hex is not the canonical form omp writes.
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

        // A crash prints the same command indented under `[Recovery]`.
        let crash = format!("[Recovery]\n  Main: omp --resume {ID}\n");
        assert_eq!(Omp.scrape_exit(&crash).as_deref(), Some(ID));

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

    /// Files written before the title slot existed start at the header.
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

    /// Names omp did not write contribute nothing, and neither does a bucket
    /// whose session has not been persisted yet.
    #[test]
    fn correlate_fs_refuses_names_and_buckets_it_cannot_read() {
        let sessions = temp("omp_names");
        // A v4 id: omp never minted it, so it embeds no creation instant.
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
        // The bucket exists from launch; the file appears only once the model
        // replies, so an empty bucket is ordinary.
        fs::create_dir_all(sessions.join("empty")).unwrap();
        assert_eq!(
            Omp.correlate_fs(Path::new(CWD), spawned(), Some(&sessions)),
            None
        );

        // The same records under omp's naming correlate, and stay unique:
        // neither skipped file counts as a second candidate.
        write_session(&sessions, "good", &good, CWD, true);
        assert_eq!(
            Omp.correlate_fs(Path::new(CWD), spawned(), Some(&sessions))
                .as_deref(),
            Some(good.as_str())
        );
    }

    /// The bucket is named from the canonical cwd while the header keeps the
    /// resolved one, so a task launched through a symlink still matches.
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

        // `PI_CONFIG_DIR` renames the config root. omp documents it as a
        // directory name, and the relative case is where the two agree.
        assert_eq!(
            home(&[("HOME", "/h"), ("PI_CONFIG_DIR", ".alt")]),
            Some("/h/.alt/agent/sessions".into())
        );
        // An absolute value diverges by design: omp would concatenate it under
        // `$HOME` (`/h/abs`), `Path::join` lets it replace `$HOME`. Pinned so
        // the divergence is deliberate rather than discovered later — it
        // misses the store and correlation returns nothing, never the wrong
        // session.
        assert_eq!(
            home(&[("HOME", "/h"), ("PI_CONFIG_DIR", "/abs")]),
            Some("/abs/agent/sessions".into())
        );

        // Without `HOME` or an override there is nothing to build from, and
        // `home_root` applies its platform fallback instead.
        assert_eq!(home(&[]), None);
    }

    /// The XDG redirect is conditional on the directory already existing, and
    /// it drops the `agent/` level.
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

        // With a profile the profile directory is what must exist.
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
