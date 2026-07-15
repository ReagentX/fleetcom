//! This harness pins eligible fresh `grok` launches with a v4 UUID, scans final
//! terminal text for `grok -r <uuid>` or `grok --resume <uuid>`, and correlates
//! session directories under `<grok-home>/sessions/<encoded-cwd>/<uuid>/`.
//!
//! `fleetcom` does not pin launches that contain `--resume`, `--continue`,
//! `--fork-session`, or `--session-id`.

use std::{fs, path::Path, time::SystemTime};

use super::{
    CapturePaths, Harness, Invocation, SpawnPlan, erase_start, is_uuid, leading_uuid, shell_quote,
    tokenize, uuid_v4, within_window,
};

/// Subcommands excluded from session capture.
const BLOCKLIST: &[&str] = &[
    "agent",
    "completions",
    "dashboard",
    "export",
    "help",
    "import",
    "inspect",
    "leader",
    "login",
    "logout",
    "mcp",
    "memory",
    "models",
    "plugin",
    "sessions",
    "setup",
    "trace",
    "update",
    "version",
    "v",
    "wrap",
    "worktree",
];

pub struct Grok;

impl Harness for Grok {
    fn name(&self) -> &'static str {
        "grok"
    }

    fn home_env_var(&self) -> &'static str {
        "GROK_HOME"
    }

    fn detect(&self, cmd: &str) -> Option<Invocation> {
        let words = tokenize(cmd)?;
        if Path::new(words.first()?.text.as_str())
            .file_name()?
            .to_str()?
            != "grok"
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
                // The value is optional: bare `-r` resumes the most recent
                // session, so the target is known only to grok.
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
            } else if t == "--session-id" || t == "-s" {
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
            } else if let Some(v) = t
                .strip_prefix("--session-id=")
                .or_else(|| t.strip_prefix("-s="))
            {
                can_inject_id = false;
                if is_uuid(v) && known_id.is_none() {
                    known_id = Some(v.to_string());
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
        // Pinning is the only injection (see `parse_capture`), so neither
        // the capture paths nor the home root matter at spawn time.
        _capture: &CapturePaths,
        _home_override: Option<&Path>,
    ) -> SpawnPlan {
        let mut plan = SpawnPlan::default();
        if inv.can_inject_id
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

    fn correlate_fs(
        &self,
        cwd: &Path,
        spawned: SystemTime,
        home_override: Option<&Path>,
    ) -> Option<String> {
        let root = match home_override {
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
            } else if t == "--session-id" || t == "-s" {
                // Without `--fork-session`, grok rejects a pinned session ID
                // alongside `--resume`: dropped.
                let start = words[i].start;
                let end = match words.get(i + 1) {
                    Some(next) if !next.text.starts_with('-') => {
                        i += 1;
                        next.end
                    }
                    _ => words[i].end,
                };
                edits.push((erase_start(cmd, start), end, String::new()));
            } else if t.starts_with("--session-id=") || t.starts_with("-s=") {
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

/// Encode an absolute cwd as a Grok session-store key. Only `/` and `%` are
/// encoded; `%` must encode to keep the mapping injective. Non-UTF-8 paths
/// have no key.
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
    fn detect_matches_on_the_basename_only() {
        assert!(Grok.detect("grok").is_some());
        assert!(Grok.detect("/usr/local/bin/grok 'do x'").is_some());
        assert!(Grok.detect("grokk").is_none());
        assert!(Grok.detect("claude").is_none());
        assert!(Grok.detect("codex").is_none());
        assert!(Grok.detect("").is_none());
    }

    #[test]
    fn detect_refuses_blocklisted_subcommands_and_shell_syntax() {
        for sub in BLOCKLIST {
            assert!(
                Grok.detect(&format!("grok {sub}")).is_none(),
                "{sub} must be refused"
            );
        }
        assert!(Grok.detect("grok sessions list").is_none());
        assert!(Grok.detect("grok | tee log").is_none());
        // A prompt positional is not a subcommand.
        assert!(Grok.detect("grok 'fix the tests'").is_some());
        // A quoted blocklist word is still the same token text: opaque.
        assert!(Grok.detect("grok 'update'").is_none());
    }

    #[test]
    fn detect_classifies_fresh_and_resuming_launches() {
        let fresh = Grok.detect("grok 'add tests'").unwrap();
        assert_eq!(fresh.known_id, None);
        assert!(fresh.can_inject_id);

        for cmd in [
            format!("grok --resume {ID}"),
            format!("grok --resume={ID}"),
            format!("grok -r {ID}"),
            format!("grok -r={ID}"),
            format!("grok --session-id {ID}"),
            format!("grok --session-id={ID}"),
            format!("grok -s {ID}"),
            format!("grok -s={ID}"),
        ] {
            let inv = Grok.detect(&cmd).unwrap();
            assert_eq!(inv.known_id.as_deref(), Some(ID), "{cmd}");
            assert!(!inv.can_inject_id, "{cmd}");
        }

        // Continue, fork, bare resume (picker/most-recent), and non-UUID
        // resume targets disable launch pinning without a known id.
        for cmd in [
            "grok --continue",
            "grok -c",
            "grok --fork-session",
            "grok --resume",
            "grok -r",
            "grok --resume not-a-uuid",
        ] {
            let inv = Grok.detect(cmd).unwrap();
            assert_eq!(inv.known_id, None, "{cmd}");
            assert!(!inv.can_inject_id, "{cmd}");
        }
    }

    /// A fresh launch gains exactly the pinned ID: no settings overlay, no
    /// config override, and no capture environment (there is no channel to
    /// point it at).
    #[test]
    fn instrument_pins_an_id_and_nothing_else() {
        let inv = Grok.detect("grok").unwrap();
        let plan = Grok.instrument(&inv, &paths(), None);
        let id = plan.injected_id.expect("fresh launch pins an id");
        assert!(is_uuid(&id));
        assert_eq!(plan.args_suffix, format!(" --session-id '{id}'"));
        assert!(plan.env.is_empty(), "no capture channel, no capture env");
    }

    #[test]
    fn instrument_never_pins_alongside_resume_continue_or_a_user_id() {
        for cmd in [
            format!("grok --resume {ID}"),
            "grok --continue".to_string(),
            "grok -r".to_string(),
            "grok --fork-session".to_string(),
            format!("grok -s {ID}"),
        ] {
            let inv = Grok.detect(&cmd).unwrap();
            let plan = Grok.instrument(&inv, &paths(), None);
            assert_eq!(plan, SpawnPlan::default(), "{cmd}");
        }
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
        // A hint whose id fails the validator returns nothing.
        assert_eq!(Grok.scrape_exit("grok -r NOT-A-UUID"), None);
        // A longer hex run is not an id.
        assert_eq!(Grok.scrape_exit(&format!("grok -r {ID}ff")), None);
    }

    #[test]
    fn resume_command_appends_replaces_and_strips_session_id() {
        // Fresh command: append, quoting the id.
        assert_eq!(
            Grok.resume_command("grok", ID),
            format!("grok --resume '{ID}'")
        );
        // Prompt bytes, including quotes, survive untouched.
        assert_eq!(
            Grok.resume_command("grok 'fix the bug' -m grok-4", ID),
            format!("grok 'fix the bug' -m grok-4 --resume '{ID}'")
        );
        // UUIDs passed through either resume flag are replaced in place.
        assert_eq!(
            Grok.resume_command(&format!("grok --resume {OTHER} --debug"), ID),
            format!("grok --resume {ID} --debug")
        );
        assert_eq!(
            Grok.resume_command(&format!("grok -r {OTHER}"), ID),
            format!("grok -r {ID}")
        );
        assert_eq!(
            Grok.resume_command(&format!("grok --resume={OTHER}"), ID),
            format!("grok --resume={ID}")
        );
        // A user-pinned session id conflicts with --resume: dropped.
        assert_eq!(
            Grok.resume_command(&format!("grok --session-id {OTHER} --debug"), ID),
            format!("grok --debug --resume '{ID}'")
        );
        assert_eq!(
            Grok.resume_command(&format!("grok -s {OTHER}"), ID),
            format!("grok --resume '{ID}'")
        );
        assert_eq!(
            Grok.resume_command(&format!("grok -s={OTHER}"), ID),
            format!("grok --resume '{ID}'")
        );
        // Unparseable commands are returned unchanged.
        assert_eq!(Grok.resume_command("grok | tee log", ID), "grok | tee log");
        // Invalid IDs leave the command unchanged.
        assert_eq!(Grok.resume_command("grok", "evil'"), "grok");
    }

    /// Store keys encode slashes and percent signs while leaving dots literal.
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

    /// The scraper recovers the exit-hint ID from a recorded terminal stream.
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
