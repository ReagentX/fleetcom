//! Claude exposes three useful session signals: a launch-time `--session-id`, a
//! `SessionStart` hook, and an exit-time resume hint. Bare launches pin a v4
//! UUID; every accepted launch receives the hook through `--settings`. The
//! filesystem fallback correlates
//! `<claude-home>/projects/<cwd-slug>/<uuid>.jsonl` transcripts.

use std::{fs, path::Path, time::SystemTime};

use super::{
    CAPTURE_ENV, CapturePaths, Harness, Invocation, SpawnPlan, detect_shape, is_uuid, leading_uuid,
    resume_shape, shell_quote, uuid_v4, within_window,
};

pub struct Claude;

impl Harness for Claude {
    fn home_env_var(&self) -> &'static str {
        "CLAUDE_CONFIG_DIR"
    }

    fn home_dot_dir(&self) -> &'static str {
        ".claude"
    }

    fn detect(&self, cmd: &str) -> Option<Invocation> {
        detect_shape(cmd, "claude", "--resume")
    }

    fn instrument(
        &self,
        inv: &Invocation,
        capture: &CapturePaths,
        // The settings overlay does not depend on the Claude home path.
        _home: Option<&Path>,
    ) -> SpawnPlan {
        let mut suffix = String::new();
        let mut injected_id = None;
        // The resume form already targets its conversation; only a bare
        // launch pins a fresh ID.
        if *inv == Invocation::Bare
            && let Some(id) = uuid_v4()
        {
            suffix.push_str(" --session-id ");
            suffix.push_str(&shell_quote(&id));
            injected_id = Some(id);
        }
        suffix.push_str(" --settings ");
        suffix.push_str(&shell_quote(&capture.claude_settings.to_string_lossy()));
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

    fn correlate_fs(&self, cwd: &Path, spawned: SystemTime, home: Option<&Path>) -> Option<String> {
        // Fall back to this process's home only when the launch environment
        // supplied neither the tool-specific override nor HOME.
        let root = match home {
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
        resume_shape(cmd, "claude", "--resume", id)
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
    fn detect_accepts_the_two_authored_shapes() {
        assert_eq!(Claude.detect("claude"), Some(Invocation::Bare));
        assert_eq!(
            Claude.detect("/usr/local/bin/claude"),
            Some(Invocation::Bare)
        );
        for cmd in [
            format!("claude --resume {ID}"),
            format!("claude --resume '{ID}'"),
            format!("/usr/local/bin/claude --resume '{ID}'"),
        ] {
            assert_eq!(
                Claude.detect(&cmd),
                Some(Invocation::Resume(ID.into())),
                "{cmd}"
            );
        }
    }

    /// Prompts, flags, alternate resume forms, subcommands, and shell syntax
    /// stay opaque and are never rewritten.
    #[test]
    fn everything_else_is_opaque_and_never_rewritten() {
        let opaque: Vec<String> = [
            "claude 'fix the tests'",
            "claude --model opus",
            "claude --continue",
            "claude -c",
            "claude --resume",
            "claude --resume not-a-uuid",
            "claude --resume $ID",
            "claude mcp list",
            "claude | tee log",
            "claude; ls",
            "FOO=bar claude",
            "claudius",
            "codex",
            "",
        ]
        .iter()
        .map(|s| s.to_string())
        .chain([
            format!("claude -r {ID}"),
            format!("claude --resume={ID}"),
            format!("claude --resume {ID} --model opus"),
            format!("claude --resume '{ID}' 'and do x'"),
            format!("claude --session-id {ID}"),
            format!("claude --resume {ID}ff"),
        ])
        .collect();
        for cmd in opaque {
            assert_eq!(Claude.detect(&cmd), None, "{cmd:?} must be opaque");
            assert_eq!(
                Claude.resume_command(&cmd, ID),
                cmd,
                "an opaque command must never be rewritten"
            );
        }
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

    /// Both accepted shapes produce the canonical resume form while preserving
    /// the program word as typed.
    #[test]
    fn resume_command_regenerates_the_canonical_form() {
        assert_eq!(
            Claude.resume_command("claude", ID),
            format!("claude --resume '{ID}'")
        );
        assert_eq!(
            Claude.resume_command("/usr/local/bin/claude", ID),
            format!("/usr/local/bin/claude --resume '{ID}'")
        );
        assert_eq!(
            Claude.resume_command(&format!("claude --resume '{OTHER}'"), ID),
            format!("claude --resume '{ID}'")
        );
        assert_eq!(
            Claude.resume_command(&format!("claude --resume {OTHER}"), ID),
            format!("claude --resume '{ID}'")
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

    /// The scraper recovers the exit-hint ID from the corpus terminal bytes.
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
