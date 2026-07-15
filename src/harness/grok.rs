//! Grok has no injectable live-capture channel. Bare launches instead pin a v4
//! UUID, and completed tasks expose either `grok -r <uuid>` or
//! `grok --resume <uuid>` in terminal output. The filesystem fallback
//! correlates `<grok-home>/sessions/<encoded-cwd>/<uuid>/` directories.

use std::{fs, path::Path, time::SystemTime};

use super::{
    CapturePaths, Harness, Invocation, SpawnPlan, detect_shape, is_uuid, leading_uuid,
    resume_shape, shell_quote, uuid_v4, within_window,
};

pub struct Grok;

impl Harness for Grok {
    fn name(&self) -> &'static str {
        "grok"
    }

    fn home_env_var(&self) -> &'static str {
        "GROK_HOME"
    }

    fn home_dot_dir(&self) -> &'static str {
        ".grok"
    }

    fn detect(&self, cmd: &str) -> Option<Invocation> {
        detect_shape(cmd, "grok", "--resume")
    }

    fn instrument(
        &self,
        inv: &Invocation,
        // Grok instrumentation does not use capture paths or home config.
        _capture: &CapturePaths,
        _home: Option<&Path>,
    ) -> SpawnPlan {
        let mut plan = SpawnPlan::default();
        // The resume form already targets its conversation; only a bare
        // launch pins a fresh ID.
        if *inv == Invocation::Bare
            && let Some(id) = uuid_v4()
        {
            plan.args_suffix = format!(" --session-id {}", shell_quote(&id));
            plan.injected_id = Some(id);
        }
        plan
    }

    /// Grok has no injected live capture channel.
    fn parse_capture(&self, _payload: &str) -> Option<String> {
        None
    }

    fn scrape_exit(&self, text: &str) -> Option<String> {
        // The last valid short or long resume hint names the conversation.
        let mut last: Option<(usize, String)> = None;
        for hint in ["grok -r ", "grok --resume "] {
            for (i, _) in text.match_indices(hint) {
                if let Some(id) = leading_uuid(&text[i + hint.len()..])
                    && last.as_ref().is_none_or(|(j, _)| i > *j)
                {
                    last = Some((i, id.to_string()));
                }
            }
        }
        last.map(|(_, id)| id)
    }

    fn correlate_fs(&self, cwd: &Path, spawned: SystemTime, home: Option<&Path>) -> Option<String> {
        // Fall back to this process's home only when the launch environment
        // supplied neither the tool-specific override nor HOME.
        let root = match home {
            Some(p) => p.to_path_buf(),
            None => dirs::home_dir()?.join(".grok"),
        };
        let dir = root.join("sessions").join(encode_cwd(cwd)?);
        let mut candidates: Vec<String> = Vec::new();
        for entry in fs::read_dir(dir).ok()?.flatten() {
            // One directory per session, named by its uuid. Files such as
            // the `prompt_history.jsonl` sibling are not sessions.
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            // Entries without creation times cannot be correlated by window.
            let Ok(created) = entry.metadata().and_then(|m| m.created()) else {
                continue;
            };
            if !within_window(created, spawned) {
                continue;
            }
            if let Some(name) = entry.file_name().to_str() {
                candidates.push(name.to_string());
            }
        }
        // Several in-window sessions cannot be told apart; a stray non-uuid
        // directory still counts against uniqueness.
        match candidates.as_slice() {
            [only] if is_uuid(only) => Some(only.clone()),
            _ => None,
        }
    }

    fn resume_command(&self, cmd: &str, id: &str) -> String {
        resume_shape(cmd, "grok", "--resume", id)
    }
}

/// Encode an absolute working directory as a Grok session-store key. `/`
/// becomes `%2F`, and `%` becomes `%25` to keep the mapping injective.
/// Non-UTF-8 paths have no representable key.
fn encode_cwd(cwd: &Path) -> Option<String> {
    let s = cwd.to_str()?;
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '/' => out.push_str("%2F"),
            '%' => out.push_str("%25"),
            c => out.push(c),
        }
    }
    Some(out)
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
            claude_settings: PathBuf::from("/tmp/cap/settings.json"),
            codex_notify: PathBuf::from("/tmp/cap/notify.sh"),
        }
    }

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fleetcom_grok_test_{tag}"));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn detect_accepts_the_two_authored_shapes() {
        assert_eq!(Grok.detect("grok"), Some(Invocation::Bare));
        assert_eq!(Grok.detect("/usr/local/bin/grok"), Some(Invocation::Bare));
        for cmd in [
            format!("grok --resume {ID}"),
            format!("grok --resume '{ID}'"),
            format!("/usr/local/bin/grok --resume '{ID}'"),
        ] {
            assert_eq!(
                Grok.detect(&cmd),
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
            "grok 'fix the tests'",
            "grok --model grok-4",
            "grok --continue",
            "grok -r",
            "grok --resume",
            "grok --resume not-a-uuid",
            "grok -r my-session",
            "grok sessions list",
            "grok | tee log",
            "grokk",
            "claude",
            "",
        ]
        .iter()
        .map(|s| s.to_string())
        .chain([
            format!("grok -r {ID}"),
            format!("grok --resume={ID}"),
            format!("grok -s {ID}"),
            format!("grok --resume {ID} --debug"),
            format!("grok --resume '{ID}' 'and do x'"),
            format!("grok --resume {ID}ff"),
        ])
        .collect();
        for cmd in opaque {
            assert_eq!(Grok.detect(&cmd), None, "{cmd:?} must be opaque");
            assert_eq!(
                Grok.resume_command(&cmd, ID),
                cmd,
                "an opaque command must never be rewritten"
            );
        }
    }

    /// A bare launch gains only the pinned ID because Grok exposes no live
    /// capture channel.
    #[test]
    fn instrument_pins_an_id_and_nothing_else() {
        let inv = Grok.detect("grok").unwrap();
        let plan = Grok.instrument(&inv, &paths(), None);
        let id = plan.injected_id.expect("a bare launch pins an id");
        assert!(is_uuid(&id));
        assert_eq!(plan.args_suffix, format!(" --session-id '{id}'"));
        assert!(plan.env.is_empty(), "no capture channel, no capture env");
    }

    /// A resume command needs no pin, overlay, or environment change.
    #[test]
    fn instrument_leaves_the_resume_form_untouched() {
        let inv = Grok.detect(&format!("grok --resume {ID}")).unwrap();
        assert_eq!(Grok.instrument(&inv, &paths(), None), SpawnPlan::default());
    }

    #[test]
    fn parse_capture_is_unconditionally_none() {
        // No injected channel exists, so no payload is ever trusted.
        let payload = format!(r#"{{"session_id":"{ID}"}}"#);
        assert_eq!(Grok.parse_capture(&payload), None);
        assert_eq!(Grok.parse_capture(""), None);
    }

    #[test]
    fn scrape_exit_reads_both_spellings_and_takes_the_last() {
        let short = format!("Resume with: grok -r {ID}");
        assert_eq!(Grok.scrape_exit(&short).as_deref(), Some(ID));
        let long = format!("Resume with: grok --resume {ID}");
        assert_eq!(Grok.scrape_exit(&long).as_deref(), Some(ID));

        // The last hint by position wins across spellings, either order.
        let both = format!("grok -r {OTHER}\n...\ngrok --resume {ID}\n");
        assert_eq!(Grok.scrape_exit(&both).as_deref(), Some(ID));
        let both = format!("grok --resume {OTHER}\n...\ngrok -r {ID}\n");
        assert_eq!(Grok.scrape_exit(&both).as_deref(), Some(ID));

        assert_eq!(Grok.scrape_exit("no hint here"), None);
        // A hint whose ID fails validation returns nothing.
        assert_eq!(Grok.scrape_exit("grok -r NOT-A-UUID"), None);
        // A longer hexadecimal run is not an ID.
        assert_eq!(Grok.scrape_exit(&format!("grok -r {ID}ff")), None);
    }

    /// Both accepted shapes produce the canonical resume form while preserving
    /// the program word as typed.
    #[test]
    fn resume_command_regenerates_the_canonical_form() {
        assert_eq!(
            Grok.resume_command("grok", ID),
            format!("grok --resume '{ID}'")
        );
        assert_eq!(
            Grok.resume_command("/usr/local/bin/grok", ID),
            format!("/usr/local/bin/grok --resume '{ID}'")
        );
        assert_eq!(
            Grok.resume_command(&format!("grok --resume '{OTHER}'"), ID),
            format!("grok --resume '{ID}'")
        );
        assert_eq!(
            Grok.resume_command(&format!("grok --resume {OTHER}"), ID),
            format!("grok --resume '{ID}'")
        );
        // Invalid IDs leave the command unchanged.
        assert_eq!(Grok.resume_command("grok", "evil'"), "grok");
    }

    /// Store keys encode slashes and percent signs while preserving dots.
    #[test]
    fn encode_cwd_matches_the_observed_store_names() {
        assert_eq!(
            encode_cwd(Path::new("/Users/chris/Documents/Code/Apple/turret")).as_deref(),
            Some("%2FUsers%2Fchris%2FDocuments%2FCode%2FApple%2Fturret")
        );
        // Dots stay literal.
        assert_eq!(
            encode_cwd(Path::new("/Users/chris/.claude/jobs/ac6e4777/tmp/groktest")).as_deref(),
            Some("%2FUsers%2Fchris%2F.claude%2Fjobs%2Fac6e4777%2Ftmp%2Fgroktest")
        );
        // A literal `%` must encode for the mapping to stay injective.
        assert_eq!(encode_cwd(Path::new("/a%b")).as_deref(), Some("%2Fa%25b"));
    }

    #[test]
    fn correlate_fs_requires_a_unique_in_window_session_dir() {
        let home = temp("correlate");
        let cwd = Path::new("/work/proj.rs");
        let dir = home.join("sessions").join("%2Fwork%2Fproj.rs");
        fs::create_dir_all(dir.join(ID)).unwrap();
        // The prompt-history sibling is a file, not a session.
        fs::write(dir.join("prompt_history.jsonl"), "{}").unwrap();
        let now = SystemTime::now();

        assert_eq!(
            Grok.correlate_fs(cwd, now, Some(&home)).as_deref(),
            Some(ID)
        );
        // Outside the window: the session predates the spawn by minutes.
        let late = now + std::time::Duration::from_secs(120);
        assert_eq!(Grok.correlate_fs(cwd, late, Some(&home)), None);
        // Wrong working directory.
        assert_eq!(
            Grok.correlate_fs(Path::new("/other"), now, Some(&home)),
            None
        );

        // A second in-window session makes the match ambiguous.
        fs::create_dir_all(dir.join(OTHER)).unwrap();
        assert_eq!(Grok.correlate_fs(cwd, now, Some(&home)), None);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn correlate_fs_rejects_a_unique_non_uuid_dir() {
        let home = temp("nonuuid");
        let cwd = Path::new("/w");
        let dir = home.join("sessions").join("%2Fw");
        fs::create_dir_all(dir.join("not-a-session")).unwrap();
        assert_eq!(Grok.correlate_fs(cwd, SystemTime::now(), Some(&home)), None);
        let _ = fs::remove_dir_all(&home);
    }

    /// The scraper recovers the exit-hint ID from the corpus terminal bytes.
    #[test]
    fn corpus_scrape_recovers_the_exit_hint_id() {
        let mut emu = Emulator::new(40, 120, 2000);
        emu.process(include_bytes!("../../tests/corpus/grok_resume.bin"));
        assert_eq!(
            Grok.scrape_exit(&emu.text_with_history()).as_deref(),
            Some("17ac97af-8cfc-46a7-9599-8cea45a687a6")
        );
    }
}
