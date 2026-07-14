//! The `codex` CLI. No launch-time id pinning exists, so capture leans on
//! the `notify` config override (`-c notify=["<program>"]`), whose program
//! receives an `agent-turn-complete` JSON payload as its final argv; the
//! exit hints `codex resume <uuid>` / `codex resume, then select <name>
//! (<uuid>)` scraped from final terminal text; and rollout files under
//! `<codex-home>/sessions/YYYY/MM/DD/rollout-<local-ts>-<uuid>.jsonl` for
//! filesystem correlation.
//!
//! Codex's own hooks are trust-gated; notify is the only injection channel.

use std::{
    fmt::Write as _,
    fs,
    io::{BufRead, BufReader, Read},
    path::Path,
    time::SystemTime,
};

use super::{
    CAPTURE_ENV, CapturePaths, Harness, Invocation, SpawnPlan, Word, is_uuid, leading_uuid,
    shell_quote, splice_insert, tokenize, within_window_ms,
};

/// Subcommands that never start a resumable interactive conversation.
const BLOCKLIST: &[&str] = &[
    "exec",
    "review",
    "login",
    "logout",
    "mcp",
    "plugin",
    "mcp-server",
    "app-server",
    "remote-control",
    "app",
    "completion",
    "update",
    "doctor",
    "sandbox",
    "debug",
    "apply",
    "archive",
    "delete",
    "unarchive",
    "fork",
    "cloud",
    "exec-server",
    "features",
    "help",
];

/// Flags whose value is a separate token, consumed while locating the
/// subcommand so the value is never misread as a positional. Unlisted
/// value-taking flags degrade to a misclassified positional, which at worst
/// refuses the command — never rewrites it wrongly.
const VALUE_FLAGS: &[&str] = &["-c", "--config", "-m", "--model", "-p", "--profile"];

pub struct Codex;

impl Harness for Codex {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn detect(&self, cmd: &str) -> Option<Invocation> {
        let words = tokenize(cmd)?;
        if Path::new(words.first()?.text.as_str())
            .file_name()?
            .to_str()?
            != "codex"
        {
            return None;
        }
        let mut known_id: Option<String> = None;
        if let Some(si) = first_positional(&words, 1) {
            let sub = words[si].text.as_str();
            if BLOCKLIST.contains(&sub) {
                return None;
            }
            // `resume <uuid>` targets a known conversation; `resume <name>`
            // and bare `resume` leave the user's target untouched. Any other
            // positional is a prompt.
            if sub == "resume"
                && let Some(ti) = first_positional(&words, si + 1)
                && is_uuid(&words[ti].text)
            {
                known_id = Some(words[ti].text.clone());
            }
        }
        Some(Invocation {
            tokens: words.into_iter().map(|w| w.text).collect(),
            known_id,
            can_inject_id: false,
        })
    }

    fn instrument(&self, inv: &Invocation, capture: &CapturePaths) -> SpawnPlan {
        // A user-configured notify would be clobbered by a second override;
        // scrape and correlation remain as capture channels.
        if has_notify_override(&inv.tokens) {
            return SpawnPlan::default();
        }
        let toml = format!(
            "notify=[\"{}\"]",
            toml_escape(&capture.codex_notify.to_string_lossy())
        );
        SpawnPlan {
            args_suffix: format!(" -c {}", shell_quote(&toml)),
            env: vec![(
                CAPTURE_ENV.into(),
                capture.capture_file.clone().into_os_string(),
            )],
            injected_id: None,
        }
    }

    fn parse_capture(&self, payload: &str) -> Option<String> {
        let v = jzon::parse(payload).ok()?;
        if v["type"].as_str() != Some("agent-turn-complete") {
            return None;
        }
        let id = v["thread-id"].as_str()?;
        is_uuid(id).then(|| id.to_string())
    }

    fn scrape_exit(&self, text: &str) -> Option<String> {
        let mut last = None;
        for line in text.lines() {
            // Plain hint: `... run codex resume <uuid>`.
            for (i, _) in line.match_indices("codex resume ") {
                if let Some(id) = leading_uuid(&line[i + "codex resume ".len()..]) {
                    last = Some(id.to_string());
                }
            }
            // Named-thread hint: `codex resume, then select <name> (<uuid>)`.
            // Only the parenthesized id is trusted — never the name.
            if line.contains("codex resume") && line.contains("then select") {
                for (i, _) in line.match_indices('(') {
                    let inner = &line[i + 1..];
                    if let Some(id) = leading_uuid(inner)
                        && inner.as_bytes().get(36) == Some(&b')')
                    {
                        last = Some(id.to_string());
                    }
                }
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
            None => dirs::home_dir()?.join(".codex"),
        };
        let spawn_ms = spawned
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()?
            .as_millis();
        // Day directories are named by LOCAL date, which std cannot compute
        // without a timezone database. The UTC date differs from it by at
        // most one day, so probing the UTC date ±2 covers local ±1.
        let spawn_days = (spawn_ms / 86_400_000) as i64;
        let mut survivors: Vec<String> = Vec::new();
        for day in (spawn_days - 2)..=(spawn_days + 2) {
            let (y, m, d) = civil_from_days(day);
            let dir = root
                .join("sessions")
                .join(format!("{y:04}"))
                .join(format!("{m:02}"))
                .join(format!("{d:02}"));
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(stem) = name
                    .to_str()
                    .and_then(|n| n.strip_prefix("rollout-"))
                    .and_then(|n| n.strip_suffix(".jsonl"))
                else {
                    continue;
                };
                let Some(id) = stem.get(stem.len().saturating_sub(36)..) else {
                    continue;
                };
                if !is_uuid(id) {
                    continue;
                }
                // The filename's own timestamp is local wall-clock time; the
                // id's embedded v7 instant is UTC and marks the same moment
                // (session start — rollouts are written lazily, so file
                // creation time can lag by however long the first prompt
                // took).
                let Some(ms) = v7_millis(id) else { continue };
                if !within_window_ms(u128::from(ms), spawn_ms) {
                    continue;
                }
                if !line1_cwd_matches(&entry.path(), cwd) {
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

    fn resume_command(&self, cmd: &str, id: &str) -> String {
        if !is_uuid(id) {
            return cmd.to_string();
        }
        let Some(words) = tokenize(cmd) else {
            return cmd.to_string();
        };
        if words.is_empty() {
            return cmd.to_string();
        }
        let Some(si) = first_positional(&words, 1) else {
            // Flags only: the resume subcommand slots in after the program.
            return splice_insert(cmd, words[0].end, &format!(" resume {}", shell_quote(id)));
        };
        let sub = words[si].text.as_str();
        if BLOCKLIST.contains(&sub) {
            return cmd.to_string();
        }
        if sub != "resume" {
            // Prompt positional: `resume <id>` precedes it; the prompt stays.
            return splice_insert(cmd, words[0].end, &format!(" resume {}", shell_quote(id)));
        }
        match first_positional(&words, si + 1) {
            Some(ti) if is_uuid(&words[ti].text) => {
                let mut out = cmd.to_string();
                out.replace_range(words[ti].start..words[ti].end, id);
                out
            }
            // A session name still resolves; the user's target stands.
            Some(_) => cmd.to_string(),
            // Bare `resume` at the end of the command gains the target.
            None if si + 1 == words.len() => format!("{cmd} {}", shell_quote(id)),
            // Flags after `resume` (e.g. --last) pick their own target;
            // adding an id would fight them.
            None => cmd.to_string(),
        }
    }
}

/// Index of the first non-flag token at or after `from`, skipping the values
/// of [`VALUE_FLAGS`].
fn first_positional(words: &[Word], mut from: usize) -> Option<usize> {
    while from < words.len() {
        let t = words[from].text.as_str();
        if VALUE_FLAGS.contains(&t) {
            from += 2;
        } else if t.starts_with('-') {
            from += 1;
        } else {
            return Some(from);
        }
    }
    None
}

/// Whether the user already routes notify somewhere: `-c notify=…`,
/// `-cnotify=…`, `-c=notify=…`, `--config notify=…`, or `--config=notify=…`.
fn has_notify_override(tokens: &[String]) -> bool {
    tokens.iter().enumerate().skip(1).any(|(i, t)| {
        if (t == "-c" || t == "--config")
            && tokens.get(i + 1).is_some_and(|v| v.starts_with("notify="))
        {
            return true;
        }
        t.strip_prefix("--config=")
            .or_else(|| t.strip_prefix("-c"))
            .is_some_and(|v| v.trim_start_matches('=').starts_with("notify="))
    })
}

/// Escape `s` for a TOML basic string. Backslash and double quote are the
/// only path bytes TOML would misread; control characters get `\u` escapes.
fn toml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// Millisecond instant embedded in a v7 uuid's first 48 bits. Codex thread
/// ids are `now_v7`, so this is the session's start. Non-v7 ids carry no
/// instant.
fn v7_millis(id: &str) -> Option<u64> {
    if id.as_bytes()[14] != b'7' {
        return None;
    }
    u64::from_str_radix(&format!("{}{}", &id[..8], &id[9..13]), 16).ok()
}

/// Whether the rollout's first line — its `session_meta` record — names
/// `cwd` as the session's working directory. The read is capped: only the
/// meta line is of interest, however large the rollout grew.
fn line1_cwd_matches(path: &Path, cwd: &Path) -> bool {
    let Ok(file) = fs::File::open(path) else {
        return false;
    };
    let mut line = String::new();
    if BufReader::new(file.take(64 * 1024))
        .read_line(&mut line)
        .is_err()
    {
        return false;
    }
    let Ok(meta) = jzon::parse(&line) else {
        return false;
    };
    meta["payload"]["cwd"]
        .as_str()
        .is_some_and(|c| Path::new(c) == cwd)
}

/// Proleptic-Gregorian date for a count of days since 1970-01-01 (Howard
/// Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::emulator::Emulator;

    const ID: &str = "019f5453-de22-7240-b2e5-0d32692aa6d9";
    const OTHER: &str = "11111111-2222-4333-8444-555555555555";

    fn paths() -> CapturePaths {
        CapturePaths {
            capture_file: PathBuf::from("/tmp/cap/session.json"),
            claude_settings: PathBuf::from("/tmp/cap/settings.json"),
            codex_notify: PathBuf::from("/tmp/Application Support/notify.sh"),
        }
    }

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fleetcom_codex_test_{tag}"));
        let _ = fs::remove_dir_all(&d);
        d
    }

    /// A v7-shaped id whose embedded instant is `ms`, with a fixed tail.
    fn v7_at(ms: u64, tail: u32) -> String {
        format!(
            "{:08x}-{:04x}-7000-8000-0000000{:05x}",
            ms >> 16,
            ms & 0xffff,
            tail
        )
    }

    /// Write a rollout under the UTC day dir for `ms` with `cwd` in its
    /// session_meta line; returns the id.
    fn write_rollout(home: &Path, ms: u64, tail: u32, cwd: &str) -> String {
        let id = v7_at(ms, tail);
        let (y, m, d) = civil_from_days((ms / 86_400_000) as i64);
        let dir = home
            .join("sessions")
            .join(format!("{y:04}"))
            .join(format!("{m:02}"))
            .join(format!("{d:02}"));
        fs::create_dir_all(&dir).unwrap();
        let meta = format!(
            r#"{{"timestamp":"x","type":"session_meta","payload":{{"id":"{id}","cwd":"{cwd}"}}}}"#
        );
        fs::write(
            dir.join(format!("rollout-2026-07-13T09-00-00-{id}.jsonl")),
            format!("{meta}\n{{}}\n"),
        )
        .unwrap();
        id
    }

    #[test]
    fn detect_matches_on_the_basename_only() {
        assert!(Codex.detect("codex").is_some());
        assert!(Codex.detect("/opt/bin/codex 'do x'").is_some());
        assert!(Codex.detect("codexx").is_none());
        assert!(Codex.detect("claude").is_none());
    }

    #[test]
    fn detect_refuses_blocklisted_subcommands_and_shell_syntax() {
        for sub in BLOCKLIST {
            assert!(
                Codex.detect(&format!("codex {sub}")).is_none(),
                "{sub} must be refused"
            );
        }
        assert!(Codex.detect("codex exec 'do x'").is_none());
        assert!(Codex.detect("codex; ls").is_none());
        // Value flags are skipped when locating the subcommand.
        assert!(Codex.detect("codex -m gpt-5 exec").is_none());
        // A prompt positional is allowed.
        assert!(Codex.detect("codex 'fix the tests'").is_some());
    }

    #[test]
    fn detect_reads_resume_targets() {
        let inv = Codex.detect(&format!("codex resume {ID}")).unwrap();
        assert_eq!(inv.known_id.as_deref(), Some(ID));
        assert!(!inv.can_inject_id, "codex cannot pin an id at launch");

        // Named target and bare resume: detected, target left to codex.
        for cmd in ["codex resume my-thread", "codex resume"] {
            let inv = Codex.detect(cmd).unwrap();
            assert_eq!(inv.known_id, None, "{cmd}");
        }
        let inv = Codex.detect("codex 'just a prompt'").unwrap();
        assert_eq!(inv.known_id, None);
    }

    #[test]
    fn instrument_installs_the_notify_override() {
        let inv = Codex.detect("codex").unwrap();
        let plan = Codex.instrument(&inv, &paths());
        assert_eq!(
            plan.args_suffix,
            r#" -c 'notify=["/tmp/Application Support/notify.sh"]'"#
        );
        assert_eq!(plan.injected_id, None);
        assert_eq!(
            plan.env,
            vec![(
                CAPTURE_ENV.into(),
                PathBuf::from("/tmp/cap/session.json").into_os_string()
            )]
        );
    }

    #[test]
    fn instrument_skips_a_user_notify_override_entirely() {
        for cmd in [
            r#"codex -c 'notify=["/my/hook"]'"#,
            r#"codex --config 'notify=["/my/hook"]'"#,
            r#"codex --config='notify=["/my/hook"]'"#,
            r#"codex '-cnotify=["/my/hook"]'"#,
        ] {
            let inv = Codex.detect(cmd).unwrap();
            assert_eq!(
                Codex.instrument(&inv, &paths()),
                SpawnPlan::default(),
                "{cmd}"
            );
        }
        // Unrelated -c overrides do not suppress injection.
        let inv = Codex
            .detect("codex -c model_reasoning_effort=high")
            .unwrap();
        assert!(!Codex.instrument(&inv, &paths()).args_suffix.is_empty());
    }

    #[test]
    fn toml_escape_covers_quotes_backslashes_and_controls() {
        assert_eq!(toml_escape("/plain/path"), "/plain/path");
        assert_eq!(
            toml_escape(r#"/with space/and"quote\slash"#),
            r#"/with space/and\"quote\\slash"#
        );
        assert_eq!(toml_escape("a\tb"), "a\\u0009b");
        // End to end through a real shell: the suffix words reach codex as
        // `-c` plus the exact TOML text, whatever the path contains.
        let paths = CapturePaths {
            capture_file: PathBuf::from("/c"),
            claude_settings: PathBuf::from("/s"),
            codex_notify: PathBuf::from(r#"/Odd Path/it's "here"\now"#),
        };
        let inv = Codex.detect("codex").unwrap();
        let plan = Codex.instrument(&inv, &paths);
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("printf '%s\\n'{}", plan.args_suffix))
            .output()
            .expect("sh must run");
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            format!("-c\n{}\n", r#"notify=["/Odd Path/it's \"here\"\\now"]"#)
        );
    }

    #[test]
    fn parse_capture_accepts_only_turn_complete_payloads() {
        let payload = format!(
            r#"{{"type":"agent-turn-complete","thread-id":"{ID}","turn-id":"t","cwd":"/w"}}"#
        );
        assert_eq!(Codex.parse_capture(&payload).as_deref(), Some(ID));

        let wrong_type = format!(r#"{{"type":"other","thread-id":"{ID}"}}"#);
        assert_eq!(Codex.parse_capture(&wrong_type), None);
        assert_eq!(
            Codex.parse_capture(r#"{"type":"agent-turn-complete","thread-id":"my session"}"#),
            None
        );
        assert_eq!(Codex.parse_capture("not json"), None);
    }

    #[test]
    fn scrape_exit_reads_both_hint_shapes_and_never_names() {
        let plain = format!("To continue this session, run codex resume {ID}");
        assert_eq!(Codex.scrape_exit(&plain).as_deref(), Some(ID));

        let named = format!("To continue this session, run codex resume, then select docs ({ID})");
        assert_eq!(Codex.scrape_exit(&named).as_deref(), Some(ID));

        // Named form without an id yields nothing — a name is not spliceable.
        assert_eq!(
            Codex.scrape_exit("run codex resume, then select my-thread"),
            None
        );
        assert_eq!(Codex.scrape_exit("codex resume my-thread"), None);

        // The last hint wins.
        let both = format!("run codex resume {OTHER}\n...\nrun codex resume, then select x ({ID})");
        assert_eq!(Codex.scrape_exit(&both).as_deref(), Some(ID));
    }

    #[test]
    fn resume_command_inserts_replaces_or_defers() {
        // Fresh launch: resume slots in directly after the program, flags
        // and prompt preserved byte for byte.
        assert_eq!(
            Codex.resume_command("codex", ID),
            format!("codex resume '{ID}'")
        );
        assert_eq!(
            Codex.resume_command("codex -m gpt-5 'do x'", ID),
            format!("codex resume '{ID}' -m gpt-5 'do x'")
        );
        // An existing uuid target is replaced in place.
        assert_eq!(
            Codex.resume_command(&format!("codex resume {OTHER} -m gpt-5"), ID),
            format!("codex resume {ID} -m gpt-5")
        );
        // A named target stands: the user chose it and it still resolves.
        assert_eq!(
            Codex.resume_command("codex resume my-thread", ID),
            "codex resume my-thread"
        );
        // Bare resume gains the captured target.
        assert_eq!(
            Codex.resume_command("codex resume", ID),
            format!("codex resume '{ID}'")
        );
        // `resume --last` picks its own target; leave it alone.
        assert_eq!(
            Codex.resume_command("codex resume --last", ID),
            "codex resume --last"
        );
        // Blocklisted and unparseable commands pass through unchanged.
        assert_eq!(Codex.resume_command("codex exec 'x'", ID), "codex exec 'x'");
        assert_eq!(Codex.resume_command("codex; ls", ID), "codex; ls");
        assert_eq!(Codex.resume_command("codex", "not-an-id"), "codex");
    }

    #[test]
    fn correlate_fs_requires_a_unique_cwd_matched_rollout() {
        let home = temp("correlate");
        let spawn_ms: u64 = 1_785_000_000_000; // 2026-07-25T02:40Z
        let spawned = SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(spawn_ms);

        let id = write_rollout(&home, spawn_ms + 4_000, 1, "/work/proj");
        assert_eq!(
            Codex
                .correlate_fs(Path::new("/work/proj"), spawned, Some(&home))
                .as_deref(),
            Some(id.as_str())
        );
        // A different task directory does not match this rollout.
        assert_eq!(
            Codex.correlate_fs(Path::new("/elsewhere"), spawned, Some(&home)),
            None
        );

        // Outside the ±30 s window: excluded.
        write_rollout(&home, spawn_ms + 90_000, 2, "/late/proj");
        assert_eq!(
            Codex.correlate_fs(Path::new("/late/proj"), spawned, Some(&home)),
            None
        );

        // Two in-window rollouts from the same directory are ambiguous.
        write_rollout(&home, spawn_ms + 8_000, 3, "/work/proj");
        assert_eq!(
            Codex.correlate_fs(Path::new("/work/proj"), spawned, Some(&home)),
            None
        );
        let _ = fs::remove_dir_all(&home);
    }

    /// A rollout filed under the previous day's directory (midnight or
    /// timezone skew) is still found: the day probe spans ±2 UTC days.
    #[test]
    fn correlate_fs_spans_adjacent_day_directories() {
        let home = temp("dayspan");
        let spawn_ms: u64 = 1_785_000_000_000;
        let spawned = SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(spawn_ms);

        let id = v7_at(spawn_ms + 2_000, 7);
        let (y, m, d) = civil_from_days((spawn_ms / 86_400_000) as i64 - 1);
        let dir = home
            .join("sessions")
            .join(format!("{y:04}"))
            .join(format!("{m:02}"))
            .join(format!("{d:02}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(format!("rollout-2026-07-24T19-40-02-{id}.jsonl")),
            format!(
                r#"{{"timestamp":"x","type":"session_meta","payload":{{"id":"{id}","cwd":"/w"}}}}"#
            ),
        )
        .unwrap();

        assert_eq!(
            Codex
                .correlate_fs(Path::new("/w"), spawned, Some(&home))
                .as_deref(),
            Some(id.as_str())
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1)); // leap year start
        assert_eq!(civil_from_days(19_782), (2024, 2, 29)); // leap day
        assert_eq!(civil_from_days(20_648), (2026, 7, 14));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    /// The recorded codex session ends with the real exit hint; the scrape
    /// must recover its id from the emulator's full retained text. The hint
    /// carries SGR mid-sentence, which the emulator strips.
    #[test]
    fn corpus_scrape_recovers_the_exit_hint_id() {
        let mut emu = Emulator::new(40, 120, 2000);
        emu.process(include_bytes!("../../tests/corpus/codex_resume.bin"));
        assert_eq!(
            Codex.scrape_exit(&emu.text_with_history()).as_deref(),
            Some("019f5453-de22-7240-b2e5-0d32692aa6d9")
        );
    }
}
