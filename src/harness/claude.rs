//! Claude exposes three useful session signals: a launch-time `--session-id`, a
//! `SessionStart` hook, and an exit-time resume hint. Bare launches pin a v4
//! UUID; every accepted launch receives the hook through `--settings`. The
//! filesystem fallback correlates
//! `<claude-home>/projects/<cwd-slug>/<uuid>.jsonl` transcripts.

use std::{path::Path, time::SystemTime};

use super::{
    CAPTURE_ENV, CapturePaths, Harness, Invocation, SpawnPlan, detect_shape, is_uuid, last_hint,
    pin_plan, resume_shape, shell_quote, unique_in_window,
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
    use std::{fs, path::PathBuf};

    use super::super::testutil::{ID, OTHER, assert_all_opaque, assert_corpus_scrape, paths};
    use super::*;
    use crate::testutil::temp;

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
        let _ = fs::remove_dir_all(&home);
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
        let _ = fs::remove_dir_all(&home);
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
