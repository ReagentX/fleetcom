//! For eligible `claude` launches, this harness pins a v4 UUID with
//! `--session-id`, adds a `SessionStart` hook when the command has no
//! `--settings`, scans final terminal text for `claude --resume <uuid>`, and
//! correlates transcripts under
//! `<claude-home>/projects/<cwd-slug>/<uuid>.jsonl`.
//!
//! `fleetcom` does not pin launches that contain `--resume`, `--continue`,
//! `--fork-session`, or `--session-id`.

use std::{fs, path::Path, time::SystemTime};

use super::{
    CAPTURE_ENV, CapturePaths, Harness, Invocation, SpawnPlan, erase_start, is_uuid, leading_uuid,
    shell_quote, tokenize, uuid_v4, within_window,
};

/// Subcommands excluded from session capture.
const BLOCKLIST: &[&str] = &[
    "agents",
    "mcp",
    "doctor",
    "update",
    "install",
    "plugin",
    "config",
    "setup-token",
    "migrate-installer",
];

pub struct Claude;

impl Harness for Claude {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn home_env_var(&self) -> &'static str {
        "CLAUDE_CONFIG_DIR"
    }

    fn detect(&self, cmd: &str) -> Option<Invocation> {
        let words = tokenize(cmd)?;
        if Path::new(words.first()?.text.as_str())
            .file_name()?
            .to_str()?
            != "claude"
        {
            return None;
        }
        let mut known_id: Option<String> = None;
        let mut can_inject_id = true;
        let mut saw_positional = false;
        let mut i = 1;
        while i < words.len() {
            let t = words[i].text.as_str();
            // Consume values for flags this parser interprets so they cannot
            // be mistaken for a subcommand. Values of other flags are not
            // modeled; a blocklisted value makes the command opaque.
            if t == "--resume" || t == "-r" {
                can_inject_id = false;
                if let Some(next) = words.get(i + 1).map(|w| w.text.as_str())
                    && !next.starts_with('-')
                {
                    if is_uuid(next) && known_id.is_none() {
                        known_id = Some(next.to_string());
                    }
                    i += 2;
                    continue;
                }
            } else if let Some(v) = t
                .strip_prefix("--resume=")
                .or_else(|| t.strip_prefix("-r="))
            {
                can_inject_id = false;
                if is_uuid(v) && known_id.is_none() {
                    known_id = Some(v.to_string());
                }
            } else if t == "--continue" || t == "-c" || t == "--fork-session" {
                can_inject_id = false;
            } else if t == "--session-id" {
                // A user-pinned id seeds the known id like a resume does.
                can_inject_id = false;
                if let Some(next) = words.get(i + 1).map(|w| w.text.as_str())
                    && !next.starts_with('-')
                {
                    if is_uuid(next) && known_id.is_none() {
                        known_id = Some(next.to_string());
                    }
                    i += 2;
                    continue;
                }
            } else if let Some(v) = t.strip_prefix("--session-id=") {
                can_inject_id = false;
                if is_uuid(v) && known_id.is_none() {
                    known_id = Some(v.to_string());
                }
            } else if t == "--settings" {
                if words.get(i + 1).is_some_and(|w| !w.text.starts_with('-')) {
                    i += 2;
                    continue;
                }
            } else if !t.starts_with('-') && !saw_positional {
                // First positional: a subcommand or a prompt string.
                if BLOCKLIST.contains(&t) {
                    return None;
                }
                saw_positional = true;
            }
            i += 1;
        }
        Some(Invocation {
            tokens: words.into_iter().map(|w| w.text).collect(),
            known_id,
            can_inject_id,
        })
    }

    fn instrument(
        &self,
        inv: &Invocation,
        capture: &CapturePaths,
        // The settings overlay layers additively onto the user's own config,
        // wherever it lives: no home inspection needed.
        _home_override: Option<&Path>,
    ) -> SpawnPlan {
        let mut suffix = String::new();
        let mut injected_id = None;
        if inv.can_inject_id
            && let Some(id) = uuid_v4()
        {
            suffix.push_str(" --session-id ");
            suffix.push_str(&shell_quote(&id));
            injected_id = Some(id);
        }
        // Preserve a user-supplied settings source. Launch-time ids, exit
        // scraping, and filesystem correlation remain available.
        let has_settings = inv.tokens[1..]
            .iter()
            .any(|t| t == "--settings" || t.starts_with("--settings="));
        if !has_settings {
            suffix.push_str(" --settings ");
            suffix.push_str(&shell_quote(&capture.claude_settings.to_string_lossy()));
        }
        SpawnPlan {
            args_suffix: suffix,
            env: vec![(
                CAPTURE_ENV.into(),
                capture.capture_file.clone().into_os_string(),
            )],
            injected_id,
        }
    }

    fn parse_capture(&self, payload: &str) -> Option<String> {
        let v = jzon::parse(payload).ok()?;
        let id = v["session_id"].as_str()?;
        is_uuid(id).then(|| id.to_string())
    }

    fn scrape_exit(&self, text: &str) -> Option<String> {
        // The last valid hint names the conversation at exit.
        const HINT: &str = "claude --resume ";
        let mut last = None;
        for (i, _) in text.match_indices(HINT) {
            if let Some(id) = leading_uuid(&text[i + HINT.len()..]) {
                last = Some(id.to_string());
            }
        }
        last
    }

    fn correlate_fs(
        &self,
        cwd: &Path,
        spawned: SystemTime,
        home_override: Option<&Path>,
    ) -> Option<String> {
        let root = match home_override {
            Some(p) => p.to_path_buf(),
            None => dirs::home_dir()?.join(".claude"),
        };
        let dir = root.join("projects").join(slug(cwd)?);
        let mut candidates: Vec<String> = Vec::new();
        for entry in fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            // Entries without creation times cannot be correlated by window.
            let Ok(created) = entry.metadata().and_then(|m| m.created()) else {
                continue;
            };
            if !within_window(created, spawned) {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                candidates.push(stem.to_string());
            }
        }
        // Several in-window transcripts cannot be told apart; a stray
        // non-uuid stem still counts against uniqueness.
        match candidates.as_slice() {
            [only] if is_uuid(only) => Some(only.clone()),
            _ => None,
        }
    }

    fn resume_command(&self, cmd: &str, id: &str) -> String {
        if !is_uuid(id) {
            return cmd.to_string();
        }
        // Preserve commands whose shell syntax this module cannot parse.
        let Some(words) = tokenize(cmd) else {
            return cmd.to_string();
        };
        let mut edits: Vec<(usize, usize, String)> = Vec::new();
        let mut replaced = false;
        let mut i = 1;
        while i < words.len() {
            let t = words[i].text.as_str();
            if t == "--resume" || t == "-r" {
                if let Some(next) = words.get(i + 1)
                    && is_uuid(&next.text)
                {
                    edits.push((next.start, next.end, id.to_string()));
                    replaced = true;
                    i += 2;
                    continue;
                }
            } else if let Some((flag, v)) = t
                .split_once('=')
                .filter(|(f, _)| *f == "--resume" || *f == "-r")
            {
                if is_uuid(v) {
                    edits.push((words[i].start, words[i].end, format!("{flag}={id}")));
                    replaced = true;
                }
            } else if t == "--session-id" {
                // A resume command cannot retain a pinned session ID.
                let start = words[i].start;
                let end = match words.get(i + 1) {
                    Some(next) if !next.text.starts_with('-') => {
                        i += 1;
                        next.end
                    }
                    _ => words[i].end,
                };
                edits.push((erase_start(cmd, start), end, String::new()));
            } else if t.starts_with("--session-id=") {
                edits.push((
                    erase_start(cmd, words[i].start),
                    words[i].end,
                    String::new(),
                ));
            }
            i += 1;
        }
        let mut out = cmd.to_string();
        edits.sort_by_key(|&(start, _, _)| std::cmp::Reverse(start));
        for (start, end, replacement) in edits {
            out.replace_range(start..end, &replacement);
        }
        if !replaced {
            out.push_str(" --resume ");
            out.push_str(&shell_quote(id));
        }
        out
    }
}

/// Claude's project slug: the absolute cwd with both `/` and `.` replaced by
/// `-` (`/a/b.c` → `-a-b-c`). Non-UTF-8 paths have no slug.
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
    use std::path::PathBuf;

    use super::*;
    use crate::emulator::Emulator;

    const ID: &str = "c8c4a5cc-0b32-4ba0-a6b4-6ed08c218e0d";
    const OTHER: &str = "11111111-2222-4333-8444-555555555555";

    fn paths() -> CapturePaths {
        CapturePaths {
            capture_file: PathBuf::from("/tmp/cap/session.json"),
            claude_settings: PathBuf::from("/tmp/Application Support/fleetcom.json"),
            codex_notify: PathBuf::from("/tmp/cap/notify.sh"),
        }
    }

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fleetcom_claude_test_{tag}"));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn detect_matches_on_the_basename_only() {
        assert!(Claude.detect("claude").is_some());
        assert!(Claude.detect("/usr/local/bin/claude 'do x'").is_some());
        assert!(Claude.detect("claudius").is_none());
        assert!(Claude.detect("codex").is_none());
        assert!(Claude.detect("").is_none());
    }

    #[test]
    fn detect_refuses_blocklisted_subcommands_and_shell_syntax() {
        for sub in BLOCKLIST {
            assert!(
                Claude.detect(&format!("claude {sub}")).is_none(),
                "{sub} must be refused"
            );
        }
        assert!(Claude.detect("claude mcp list").is_none());
        assert!(Claude.detect("claude | tee log").is_none());
        // A prompt positional is not a subcommand.
        assert!(Claude.detect("claude 'fix the tests'").is_some());
        // A quoted blocklist word is still the same token text: opaque.
        assert!(Claude.detect("claude 'update'").is_none());
    }

    #[test]
    fn detect_classifies_fresh_and_resuming_launches() {
        let fresh = Claude.detect("claude 'add tests'").unwrap();
        assert_eq!(fresh.known_id, None);
        assert!(fresh.can_inject_id);

        for cmd in [
            format!("claude --resume {ID}"),
            format!("claude --resume={ID}"),
            format!("claude -r {ID}"),
            format!("claude --session-id {ID}"),
            format!("claude --session-id={ID}"),
        ] {
            let inv = Claude.detect(&cmd).unwrap();
            assert_eq!(inv.known_id.as_deref(), Some(ID), "{cmd}");
            assert!(!inv.can_inject_id, "{cmd}");
        }

        // Continue, fork, and non-UUID resume targets disable launch pinning.
        for cmd in [
            "claude --continue",
            "claude -c",
            "claude --fork-session",
            "claude --resume",
            "claude --resume not-a-uuid",
        ] {
            let inv = Claude.detect(cmd).unwrap();
            assert_eq!(inv.known_id, None, "{cmd}");
            assert!(!inv.can_inject_id, "{cmd}");
        }
    }

    #[test]
    fn instrument_pins_an_id_and_layers_settings_on_fresh_launches() {
        let inv = Claude.detect("claude").unwrap();
        let plan = Claude.instrument(&inv, &paths(), None);
        let id = plan.injected_id.expect("fresh launch pins an id");
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

    #[test]
    fn instrument_never_pins_alongside_resume_continue_or_a_user_id() {
        for cmd in [
            format!("claude --resume {ID}"),
            "claude --continue".to_string(),
            "claude --fork-session".to_string(),
            format!("claude --session-id {ID}"),
        ] {
            let inv = Claude.detect(&cmd).unwrap();
            let plan = Claude.instrument(&inv, &paths(), None);
            assert_eq!(plan.injected_id, None, "{cmd}");
            assert_eq!(
                plan.args_suffix, " --settings '/tmp/Application Support/fleetcom.json'",
                "{cmd}"
            );
        }
    }

    #[test]
    fn instrument_defers_to_a_user_supplied_settings_flag() {
        let inv = Claude.detect("claude --settings mine.json").unwrap();
        let plan = Claude.instrument(&inv, &paths(), None);
        assert!(!plan.args_suffix.contains("--settings"));
        assert!(plan.args_suffix.starts_with(" --session-id '"));

        let inv = Claude
            .detect(&format!("claude --settings=mine.json --resume {ID}"))
            .unwrap();
        let plan = Claude.instrument(&inv, &paths(), None);
        assert_eq!(plan.args_suffix, "");
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
        // A hint whose id fails the validator returns nothing.
        assert_eq!(Claude.scrape_exit("claude --resume NOT-A-UUID"), None);
        // A longer hex run is not an id.
        assert_eq!(Claude.scrape_exit(&format!("claude --resume {ID}ff")), None);
    }

    #[test]
    fn resume_command_appends_replaces_and_strips_session_id() {
        // Fresh command: append, quoting the id.
        assert_eq!(
            Claude.resume_command("claude", ID),
            format!("claude --resume '{ID}'")
        );
        // Prompt bytes, including quotes, survive untouched.
        assert_eq!(
            Claude.resume_command("claude 'fix the bug' --model opus", ID),
            format!("claude 'fix the bug' --model opus --resume '{ID}'")
        );
        // UUIDs passed through either resume flag are replaced in place.
        assert_eq!(
            Claude.resume_command(&format!("claude --resume {OTHER} -v"), ID),
            format!("claude --resume {ID} -v")
        );
        assert_eq!(
            Claude.resume_command(&format!("claude --resume={OTHER}"), ID),
            format!("claude --resume={ID}")
        );
        // A user-pinned session id conflicts with --resume: dropped.
        assert_eq!(
            Claude.resume_command(&format!("claude --session-id {OTHER} -v"), ID),
            format!("claude -v --resume '{ID}'")
        );
        assert_eq!(
            Claude.resume_command(&format!("claude --session-id={OTHER}"), ID),
            format!("claude --resume '{ID}'")
        );
        // Unparseable commands are returned unchanged.
        assert_eq!(
            Claude.resume_command("claude | tee log", ID),
            "claude | tee log"
        );
        // Invalid IDs leave the command unchanged.
        assert_eq!(Claude.resume_command("claude", "evil'"), "claude");
    }

    #[test]
    fn correlate_fs_requires_a_unique_in_window_transcript() {
        let home = temp("correlate");
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
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn correlate_fs_rejects_a_unique_non_uuid_stem() {
        let home = temp("nonuuid");
        let cwd = Path::new("/w");
        let dir = home.join("projects").join("-w");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("agent-notes.jsonl"), "{}").unwrap();
        assert_eq!(
            Claude.correlate_fs(cwd, SystemTime::now(), Some(&home)),
            None
        );
        let _ = fs::remove_dir_all(&home);
    }

    /// The scraper recovers the exit-hint ID from a recorded terminal stream.
    #[test]
    fn corpus_scrape_recovers_the_exit_hint_id() {
        let mut emu = Emulator::new(40, 120, 2000);
        emu.process(include_bytes!("../../tests/corpus/claude_resume.bin"));
        assert_eq!(
            Claude.scrape_exit(&emu.text_with_history()).as_deref(),
            Some("c8c4a5cc-0b32-4ba0-a6b4-6ed08c218e0d")
        );
    }
}
