//! For eligible `codex` commands, this harness captures IDs through an injected
//! `notify` override and chains compatible configured notifiers. It also scans
//! both final resume-hint forms and correlates rollout files under
//! `<codex-home>/sessions/YYYY/MM/DD/rollout-<local-ts>-<uuid>.jsonl`.

use std::{
    fmt::Write as _,
    fs,
    io::{BufRead, BufReader, Read},
    path::Path,
    time::SystemTime,
};

use super::{
    CAPTURE_ENV, CapturePaths, FlagTable, Harness, Invocation, NOTIFY_CHAIN_ENV, Scan, SpawnPlan,
    is_uuid, leading_uuid, shell_quote, splice_insert, tokenize, within_window_ms,
};

/// Subcommands excluded from session capture.
const BLOCKLIST: &[&str] = &[
    "exec",
    "e", // alias of exec
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
    "a", // alias of apply
    "archive",
    "delete",
    "unarchive",
    "fork",
    "cloud",
    "exec-server",
    "features",
    "help",
];

/// Top-level `codex` flags that consume a separate value. `first_positional`
/// skips both tokens while locating the subcommand or prompt. Attached values
/// such as `--flag=value` are handled inline.
const VALUE_FLAGS: &[&str] = &[
    "-c",
    "--config",
    "-m",
    "--model",
    "-p",
    "--profile",
    "-i",
    "--image",
    "-s",
    "--sandbox",
    "-a",
    "--ask-for-approval",
    "-C",
    "--cd",
    "--add-dir",
    "--enable",
    "--disable",
    "--local-provider",
    "--remote",
    "--remote-auth-token-env",
];

/// Top-level `codex` flags that consume no value.
const BOOL_FLAGS: &[&str] = &[
    "--oss",
    "--search",
    "--no-alt-screen",
    "--strict-config",
    "--last",
    "--all",
    "--include-non-interactive",
    "--dangerously-bypass-approvals-and-sandbox",
    "--dangerously-bypass-hook-trust",
    "-h",
    "--help",
    "-V",
    "--version",
];

/// Codex has no optional-value or variadic top-level flags, so those tables
/// are empty. A flag in neither table makes the command opaque.
const TABLE: FlagTable = FlagTable {
    value: VALUE_FLAGS,
    optional: &[],
    variadic: &[],
    boolean: BOOL_FLAGS,
};

pub struct Codex;

impl Harness for Codex {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn home_env_var(&self) -> &'static str {
        "CODEX_HOME"
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
        match TABLE.first_positional(&words, 1) {
            // An unrecognized flag makes the command opaque: refuse rather
            // than risk misreading its value as the subcommand.
            Scan::Opaque => return None,
            Scan::Exhausted => {}
            Scan::Positional(si) => {
                let sub = words[si].text.as_str();
                if BLOCKLIST.contains(&sub) {
                    return None;
                }
                // `resume <uuid>` targets a known conversation; `resume
                // <name>` and bare `resume` leave the user's target
                // untouched. Any other positional is a prompt.
                if sub == "resume" {
                    match TABLE.first_positional(&words, si + 1) {
                        Scan::Opaque => return None,
                        Scan::Positional(ti) if is_uuid(&words[ti].text) => {
                            known_id = Some(words[ti].text.clone());
                        }
                        _ => {}
                    }
                }
            }
        }
        Some(Invocation {
            tokens: words.into_iter().map(|w| w.text).collect(),
            known_id,
            can_inject_id: false,
        })
    }

    fn instrument(
        &self,
        inv: &Invocation,
        capture: &CapturePaths,
        home_override: Option<&Path>,
    ) -> SpawnPlan {
        // A notify override in the command tokens is per-invocation intent;
        // never chain over it. Exit scraping and filesystem correlation
        // remain available without an injected notifier.
        if has_notify_override(&inv.tokens) {
            return SpawnPlan::default();
        }
        let chain = match config_notify_route(home_override, &inv.tokens) {
            NotifyRoute::Vacant => None,
            // A routed notifier rides along: the injected script execs this
            // argv, payload appended, after the capture write.
            NotifyRoute::Chain(argv) => Some(argv.join("\n")),
            // A route the chain cannot carry faithfully: leave the command
            // untouched rather than guess.
            NotifyRoute::Opaque => return SpawnPlan::default(),
        };
        let toml = format!(
            "notify=[\"{}\"]",
            toml_escape(&capture.codex_notify.to_string_lossy())
        );
        let mut env = vec![(
            CAPTURE_ENV.into(),
            capture.capture_file.clone().into_os_string(),
        )];
        if let Some(chain) = chain {
            env.push((NOTIFY_CHAIN_ENV.into(), chain.into()));
        }
        SpawnPlan {
            args_suffix: format!(" -c {}", shell_quote(&toml)),
            env,
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
            // Only the parenthesized id is trusted, never the name.
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
                // Correlate with the v7 id's embedded UTC instant; the
                // filename timestamp is local wall-clock time.
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
        let si = match TABLE.first_positional(&words, 1) {
            // Opaque: an unknown flag. detect already refused it, so this is
            // defensive; leave the command untouched.
            Scan::Opaque => return cmd.to_string(),
            // Flags only: the resume subcommand slots in after the program.
            Scan::Exhausted => {
                return splice_insert(cmd, words[0].end, &format!(" resume {}", shell_quote(id)));
            }
            Scan::Positional(si) => si,
        };
        let sub = words[si].text.as_str();
        if BLOCKLIST.contains(&sub) {
            return cmd.to_string();
        }
        if sub != "resume" {
            // Prompt positional: `resume <id>` precedes it; the prompt stays.
            return splice_insert(cmd, words[0].end, &format!(" resume {}", shell_quote(id)));
        }
        match TABLE.first_positional(&words, si + 1) {
            Scan::Opaque => cmd.to_string(),
            Scan::Positional(ti) if is_uuid(&words[ti].text) => {
                let mut out = cmd.to_string();
                out.replace_range(words[ti].start..words[ti].end, id);
                out
            }
            // A session name still resolves; the user's target stands.
            Scan::Positional(_) => cmd.to_string(),
            // Bare `resume` at the end of the command gains the target.
            Scan::Exhausted if si + 1 == words.len() => format!("{cmd} {}", shell_quote(id)),
            // Flags after `resume` (e.g. --last) pick their own target;
            // adding an id would fight them.
            Scan::Exhausted => cmd.to_string(),
        }
    }
}

/// Whether the command already routes notifications through `-c notify=…`,
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

/// How `instrument` must treat the user's configured notify route.
#[derive(Debug, PartialEq, Eq)]
enum NotifyRoute {
    /// No active `notify` assignment: inject the capture notifier alone.
    Vacant,
    /// One assignment the chain transport can carry: inject the capture
    /// notifier and hand it this argv to exec afterward.
    Chain(Vec<String>),
    /// An assignment the transport cannot carry faithfully: skip injection
    /// so the user's route keeps working untouched.
    Opaque,
}

/// Classify the `notify` route in `config.toml` and the effective profile
/// config. The profile file's assignment overrides the base file's. The last
/// command-line `-p`/`--profile` wins; otherwise the first line-based
/// `profile` assignment in `config.toml` selects the profile. Line-based
/// checks also match assignments inside TOML tables, so two `notify` lines
/// in one file are ambiguous and read as [`NotifyRoute::Opaque`].
fn config_notify_route(home: Option<&Path>, tokens: &[String]) -> NotifyRoute {
    let root = match home {
        Some(p) => p.to_path_buf(),
        None => match dirs::home_dir() {
            Some(h) => h.join(".codex"),
            None => return NotifyRoute::Vacant,
        },
    };
    let config_text = fs::read_to_string(root.join("config.toml")).unwrap_or_default();
    let profile_text = cli_profile(tokens)
        .or_else(|| config_profile(&config_text))
        .and_then(|p| fs::read_to_string(root.join(format!("{p}.config.toml"))).ok())
        .unwrap_or_default();
    for text in [&profile_text, &config_text] {
        let mut values = text.lines().filter_map(notify_value);
        let Some(value) = values.next() else { continue };
        if values.next().is_some() {
            return NotifyRoute::Opaque;
        }
        return route_for(value);
    }
    NotifyRoute::Vacant
}

/// Classify one assignment's value. Newlines are the chain encoding's
/// delimiter, and empty elements vanish in the script's field split (newline
/// is IFS whitespace, which collapses), so neither can travel; an empty
/// array routes no program at all.
fn route_for(value: &str) -> NotifyRoute {
    match parse_notify_array(value) {
        Some(argv)
            if !argv.is_empty() && argv.iter().all(|a| !a.is_empty() && !a.contains('\n')) =>
        {
            NotifyRoute::Chain(argv)
        }
        _ => NotifyRoute::Opaque,
    }
}

/// Effective profile named on the command line, or `None`. Accepts `-p x`,
/// `--profile x`, `--profile=x`, `-px`, and `-p=x`; the last occurrence wins.
fn cli_profile(tokens: &[String]) -> Option<String> {
    let mut profile = None;
    let mut i = 1;
    while i < tokens.len() {
        let t = tokens[i].as_str();
        if (t == "-p" || t == "--profile")
            && let Some(v) = tokens.get(i + 1)
        {
            profile = Some(v.clone());
            i += 2;
            continue;
        } else if let Some(v) = t.strip_prefix("--profile=") {
            profile = Some(v.to_string());
        } else if let Some(v) = t.strip_prefix("-p").filter(|_| t != "-p") {
            // `-px` or `-p=x`.
            profile = Some(v.strip_prefix('=').unwrap_or(v).to_string());
        }
        i += 1;
    }
    profile.filter(|p| !p.is_empty())
}

/// First line-based `profile = name` assignment in `config.toml`. Bare and
/// quoted values are accepted, and trailing comments are ignored.
fn config_profile(text: &str) -> Option<String> {
    for line in text.lines() {
        let Some(rest) = line.trim_start().strip_prefix("profile") else {
            continue;
        };
        let Some(rest) = rest.trim_start_matches([' ', '\t']).strip_prefix('=') else {
            continue;
        };
        let val = unquote_toml(rest.trim());
        if !val.is_empty() {
            return Some(val);
        }
    }
    None
}

/// Extract a quoted value or the first whitespace/`#`-delimited bare token.
fn unquote_toml(s: &str) -> String {
    for q in ['"', '\''] {
        if let Some(rest) = s.strip_prefix(q)
            && let Some(end) = rest.find(q)
        {
            return rest[..end].to_string();
        }
    }
    s.split([' ', '\t', '#']).next().unwrap_or("").to_string()
}

/// Value after `=` of an uncommented bare `notify` assignment, or `None`.
fn notify_value(line: &str) -> Option<&str> {
    let rest = line.trim_start().strip_prefix("notify")?;
    rest.trim_start_matches([' ', '\t']).strip_prefix('=')
}

/// Parse a one-line TOML array of basic strings into its elements. Anything
/// else returns `None`: literal strings, non-string elements, a multi-line
/// array (the line ends before `]`), or junk after the array. A trailing
/// comma and a trailing `#` comment are tolerated.
fn parse_notify_array(value: &str) -> Option<Vec<String>> {
    let mut rest = value.trim_start_matches([' ', '\t']).strip_prefix('[')?;
    let mut out = Vec::new();
    loop {
        rest = rest.trim_start_matches([' ', '\t']);
        if let Some(tail) = rest.strip_prefix(']') {
            let tail = tail.trim_start_matches([' ', '\t']);
            return (tail.is_empty() || tail.starts_with('#')).then_some(out);
        }
        let (elem, tail) = parse_basic_string(rest.strip_prefix('"')?)?;
        out.push(elem);
        rest = tail.trim_start_matches([' ', '\t']);
        if let Some(t) = rest.strip_prefix(',') {
            rest = t;
        } else if !rest.starts_with(']') {
            return None;
        }
    }
}

/// Decode a TOML basic string after its opening quote; return the text and
/// the remainder past the closing quote. The escapes are TOML's fixed set,
/// a superset of what [`toml_escape`] emits. An unknown escape, a malformed
/// `\u`/`\U`, or a missing closing quote returns `None`.
fn parse_basic_string(s: &str) -> Option<(String, &str)> {
    let mut out = String::new();
    let mut rest = s;
    loop {
        let i = rest.find(['"', '\\'])?;
        out.push_str(&rest[..i]);
        if rest.as_bytes()[i] == b'"' {
            return Some((out, &rest[i + 1..]));
        }
        let esc = rest[i + 1..].chars().next()?;
        rest = &rest[i + 1 + esc.len_utf8()..];
        match esc {
            'b' => out.push('\u{8}'),
            't' => out.push('\t'),
            'n' => out.push('\n'),
            'f' => out.push('\u{c}'),
            'r' => out.push('\r'),
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            'u' | 'U' => {
                let n = if esc == 'u' { 4 } else { 8 };
                let hex = rest
                    .get(..n)
                    .filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit()))?;
                out.push(char::from_u32(u32::from_str_radix(hex, 16).ok()?)?);
                rest = &rest[n..];
            }
            _ => return None,
        }
    }
}

/// Escape a path for a TOML basic string.
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

/// Return the millisecond instant in a validated v7 UUID. Other UUID versions
/// return `None`.
fn v7_millis(id: &str) -> Option<u64> {
    if id.as_bytes()[14] != b'7' {
        return None;
    }
    u64::from_str_radix(&format!("{}{}", &id[..8], &id[9..13]), 16).ok()
}

/// Whether the rollout's first record names `cwd`. The read is capped at
/// 64 KiB because later content is ignored.
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

/// Proleptic Gregorian date for a count of days since 1970-01-01.
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

    /// A v7-shaped ID whose embedded instant is `ms`, with a fixed tail.
    fn v7_at(ms: u64, tail: u32) -> String {
        format!(
            "{:08x}-{:04x}-7000-8000-0000000{:05x}",
            ms >> 16,
            ms & 0xffff,
            tail
        )
    }

    /// Write a rollout under the UTC day dir for `ms` with `cwd` in its
    /// `session_meta` line; returns the ID.
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

    /// Scratch home without a `config.toml`.
    fn no_config_home() -> PathBuf {
        temp("no_config_home")
    }

    #[test]
    fn instrument_installs_the_notify_override() {
        let inv = Codex.detect("codex").unwrap();
        let plan = Codex.instrument(&inv, &paths(), Some(&no_config_home()));
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
                Codex.instrument(&inv, &paths(), Some(&no_config_home())),
                SpawnPlan::default(),
                "{cmd}"
            );
        }
        // Unrelated -c overrides do not suppress injection.
        let inv = Codex
            .detect("codex -c model_reasoning_effort=high")
            .unwrap();
        assert!(
            !Codex
                .instrument(&inv, &paths(), Some(&no_config_home()))
                .args_suffix
                .is_empty()
        );
    }

    /// A parseable `notify` assignment is chained through
    /// [`NOTIFY_CHAIN_ENV`]. Comments, longer keys, and missing files leave
    /// plain injection unchanged.
    #[test]
    fn instrument_chains_a_config_toml_notify() {
        let home = temp("cfg_notify");
        let inv = Codex.detect("codex").unwrap();
        let chained = |plan: &SpawnPlan| {
            plan.env
                .iter()
                .find(|(k, _)| k == NOTIFY_CHAIN_ENV)
                .map(|(_, v)| v.clone())
        };

        // Missing file (and missing home dir): plain injection, no chain.
        let plan = Codex.instrument(&inv, &paths(), Some(&home));
        assert!(!plan.args_suffix.is_empty());
        assert_eq!(chained(&plan), None);

        fs::create_dir_all(&home).unwrap();
        let cfg = home.join("config.toml");
        for active in [
            "notify = [\"/my/thing\"]\n",
            "notify=[\"/my/thing\"]\n",
            "\tnotify\t= [\"/my/thing\"] # mine\n",
            "model = \"gpt-5\"\nnotify = [\"/my/thing\"]\n",
        ] {
            fs::write(&cfg, active).unwrap();
            let plan = Codex.instrument(&inv, &paths(), Some(&home));
            assert!(!plan.args_suffix.is_empty(), "{active:?}");
            assert_eq!(chained(&plan), Some("/my/thing".into()), "{active:?}");
            // The capture env still rides the chain case.
            assert!(plan.env.iter().any(|(k, _)| k == CAPTURE_ENV), "{active:?}");
        }
        for inert in [
            "# notify = [\"/my/thing\"]\n",
            "  # notify = [\"/my/thing\"]\n",
            "notify_extra = 1\n",
            "notify\n",
        ] {
            fs::write(&cfg, inert).unwrap();
            let plan = Codex.instrument(&inv, &paths(), Some(&home));
            assert!(!plan.args_suffix.is_empty(), "{inert:?}");
            assert_eq!(chained(&plan), None, "{inert:?}");
        }
        let _ = fs::remove_dir_all(&home);
    }

    /// A notifier path containing spaces and a fixed argument is preserved in
    /// the newline-joined chain.
    #[test]
    fn instrument_chains_the_vendor_desktop_entry() {
        let home = temp("vendor_notify");
        fs::create_dir_all(&home).unwrap();
        fs::write(
            home.join("config.toml"),
            "notify = [\"/Applications/Codex Computer Use.app/Contents/MacOS/SkyComputerUseClient\", \"turn-ended\"]\n",
        )
        .unwrap();
        let inv = Codex.detect("codex").unwrap();
        let plan = Codex.instrument(&inv, &paths(), Some(&home));
        assert_eq!(
            plan.args_suffix,
            r#" -c 'notify=["/tmp/Application Support/notify.sh"]'"#
        );
        assert!(plan.env.contains(&(
            NOTIFY_CHAIN_ENV.into(),
            "/Applications/Codex Computer Use.app/Contents/MacOS/SkyComputerUseClient\nturn-ended"
                .into()
        )));
        let _ = fs::remove_dir_all(&home);
    }

    /// An assignment the chain cannot carry skips injection entirely.
    #[test]
    fn instrument_skips_an_unrepresentable_config_notify() {
        let home = temp("opaque_notify");
        fs::create_dir_all(&home).unwrap();
        let cfg = home.join("config.toml");
        let inv = Codex.detect("codex").unwrap();
        for opaque in [
            // Multi-line array: the value ends mid-structure.
            "notify = [\n  \"/my/thing\",\n]\n",
            // Literal strings are unsupported.
            "notify = ['/my/thing']\n",
            // Empty array: notify is routed, yet no program to chain.
            "notify = []\n",
            // Empty element: the script's field split would drop it.
            "notify = [\"\"]\n",
            // Embedded newline: the chain encoding's delimiter.
            "notify = [\"a\\nb\"]\n",
            // Not an array.
            "notify = \"/my/thing\"\n",
            // Two assignment lines (e.g. one inside a table): ambiguous.
            "notify = [\"/a\"]\nnotify = [\"/b\"]\n",
        ] {
            fs::write(&cfg, opaque).unwrap();
            assert_eq!(
                Codex.instrument(&inv, &paths(), Some(&home)),
                SpawnPlan::default(),
                "{opaque:?}"
            );
        }
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn toml_escape_covers_quotes_backslashes_and_controls() {
        assert_eq!(toml_escape("/plain/path"), "/plain/path");
        assert_eq!(
            toml_escape(r#"/with space/and"quote\slash"#),
            r#"/with space/and\"quote\\slash"#
        );
        assert_eq!(toml_escape("a\tb"), "a\\u0009b");
        // Execute the suffix through a shell and inspect the resulting words.
        let paths = CapturePaths {
            capture_file: PathBuf::from("/c"),
            claude_settings: PathBuf::from("/s"),
            codex_notify: PathBuf::from(r#"/Odd Path/it's "here"\now"#),
        };
        let inv = Codex.detect("codex").unwrap();
        let plan = Codex.instrument(&inv, &paths, Some(&no_config_home()));
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
    fn parse_notify_array_decodes_escapes_and_structure() {
        assert_eq!(
            parse_notify_array(" [\"/bin/notify\"]").unwrap(),
            vec!["/bin/notify"]
        );
        // Spaces in the path and the second element are preserved.
        assert_eq!(
            parse_notify_array(
                r#" ["/Applications/Codex Computer Use.app/Contents/MacOS/SkyComputerUseClient", "turn-ended"]"#
            )
            .unwrap(),
            vec![
                "/Applications/Codex Computer Use.app/Contents/MacOS/SkyComputerUseClient",
                "turn-ended"
            ]
        );
        // Escapes: TOML's fixed set, mirroring what `toml_escape` emits.
        assert_eq!(
            parse_notify_array(r#"["a\"b\\c", "d\u0041\te"]"#).unwrap(),
            vec!["a\"b\\c", "d\u{41}\te"]
        );
        // Trailing comma and a trailing comment are tolerated.
        assert_eq!(
            parse_notify_array("[\"a\", \"b\",] # mine").unwrap(),
            vec!["a", "b"]
        );
        assert_eq!(parse_notify_array("[]").unwrap(), Vec::<String>::new());
        assert_eq!(parse_notify_array("[ ]").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn parse_notify_array_rejects_unsupported_shapes() {
        for bad in [
            // A multi-line array leaves the line mid-structure.
            "[",
            "[\"/my/thing\",",
            // Unterminated element.
            r#"["a"#,
            // Literal strings and non-string elements.
            "['a']",
            "[1]",
            // Missing comma, trailing junk, unknown escape, malformed \u.
            r#"["a" "b"]"#,
            r#"["a"] x"#,
            r#"["a\qb"]"#,
            r#"["a\u00gg"]"#,
            // Not an array at all.
            r#""a""#,
        ] {
            assert_eq!(parse_notify_array(bad), None, "{bad:?}");
        }
    }

    /// `route_for` refuses values the transport would corrupt even when the
    /// array itself parses.
    #[test]
    fn route_for_refuses_untransportable_argv() {
        assert_eq!(
            route_for(r#" ["/x", "y"]"#),
            NotifyRoute::Chain(vec!["/x".into(), "y".into()])
        );
        // Newline elements collide with the join delimiter; empty elements
        // are dropped by sh field splitting; an empty array has no program.
        assert_eq!(route_for(r#"["a\nb"]"#), NotifyRoute::Opaque);
        assert_eq!(route_for(r#"[""]"#), NotifyRoute::Opaque);
        assert_eq!(route_for("[]"), NotifyRoute::Opaque);
        assert_eq!(route_for("garbage"), NotifyRoute::Opaque);
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

        // Named form without an id yields nothing: a name is not spliceable.
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
        // A subcommand alias (`e` for exec, `a` for apply) is blocklisted too,
        // so it is never rewritten into `codex resume '<id>' e …`.
        assert_eq!(Codex.resume_command("codex e 'x'", ID), "codex e 'x'");
        assert_eq!(Codex.resume_command("codex a", ID), "codex a");
        assert_eq!(Codex.resume_command("codex; ls", ID), "codex; ls");
        assert_eq!(Codex.resume_command("codex", "not-an-id"), "codex");
    }

    #[test]
    fn detect_refuses_unknown_flags_and_skips_value_flags() {
        // A multibyte char after the dash must classify (as Unknown), not
        // panic on a byte-boundary slice.
        assert!(Codex.detect("codex -\u{e9}x").is_none());
        // A separate flag value is skipped before locating the subcommand.
        let inv = Codex
            .detect(&format!("codex --sandbox workspace-write resume {ID}"))
            .unwrap();
        assert_eq!(inv.known_id.as_deref(), Some(ID));

        // `exec` remains the blocklisted subcommand after a value flag.
        assert!(
            Codex
                .detect("codex --sandbox workspace-write exec 'do x'")
                .is_none()
        );

        // `--flag=value` and `-fvalue` spellings are self-contained.
        assert!(
            Codex
                .detect("codex --sandbox=workspace-write 'prompt'")
                .is_some()
        );
        assert!(Codex.detect("codex -sworkspace-write 'prompt'").is_some());

        // A flag in neither table makes the command opaque, anywhere it sits.
        assert!(Codex.detect("codex --made-up-flag x").is_none());
        assert!(
            Codex
                .detect(&format!("codex --made-up-flag x resume {ID}"))
                .is_none()
        );
    }

    #[test]
    fn resume_command_never_corrupts_after_a_value_flag() {
        // The value flag's argument is not treated as the subcommand.
        assert_eq!(
            Codex.resume_command("codex --sandbox workspace-write exec 'x'", ID),
            "codex --sandbox workspace-write exec 'x'"
        );

        // The existing UUID target is replaced in place.
        let cmd = format!("codex --sandbox workspace-write resume {OTHER}");
        let out = Codex.resume_command(&cmd, ID);
        assert_eq!(out, format!("codex --sandbox workspace-write resume {ID}"));
        assert_eq!(out.matches("resume").count(), 1);

        // An unknown flag leaves the command untouched.
        assert_eq!(
            Codex.resume_command("codex --made-up-flag x", ID),
            "codex --made-up-flag x"
        );
    }

    #[test]
    fn config_notify_route_resolves_profiles() {
        let home = temp("profile_notify");
        fs::create_dir_all(&home).unwrap();
        let cfg = home.join("config.toml");
        let team = home.join("team.config.toml");
        let team_route = NotifyRoute::Chain(vec!["/team/hook".to_string()]);

        // notify lives in the profile file; `-p team` on the CLI selects it.
        fs::write(&cfg, "model = \"gpt-5\"\n").unwrap();
        fs::write(&team, "notify = [\"/team/hook\"]\n").unwrap();
        let inv = Codex.detect("codex -p team").unwrap();
        assert_eq!(config_notify_route(Some(&home), &inv.tokens), team_route);
        let plan = Codex.instrument(&inv, &paths(), Some(&home));
        assert!(
            plan.env
                .contains(&(NOTIFY_CHAIN_ENV.into(), "/team/hook".into())),
            "{:?}",
            plan.env
        );

        // Profile selected by config.toml's own `profile` key, no `-p`.
        let bare = vec!["codex".to_string()];
        fs::write(&cfg, "profile = \"team\"\n").unwrap();
        assert_eq!(config_notify_route(Some(&home), &bare), team_route);
        // Bare (unquoted) value with a trailing comment resolves too.
        fs::write(&cfg, "profile = team # mine\n").unwrap();
        assert_eq!(config_notify_route(Some(&home), &bare), team_route);

        // The profile file's assignment overrides the base file's.
        fs::write(&cfg, "profile = \"team\"\nnotify = [\"/base/hook\"]\n").unwrap();
        assert_eq!(config_notify_route(Some(&home), &bare), team_route);

        // Commented out in the profile file: the base assignment stands.
        fs::write(&team, "# notify = [\"/team/hook\"]\n").unwrap();
        assert_eq!(
            config_notify_route(Some(&home), &bare),
            NotifyRoute::Chain(vec!["/base/hook".to_string()])
        );

        // No assignment anywhere: vacant, plain injection.
        fs::write(&cfg, "profile = \"team\"\n").unwrap();
        assert_eq!(config_notify_route(Some(&home), &bare), NotifyRoute::Vacant);

        // A missing profile file leaves only the base config.
        fs::write(&cfg, "profile = \"ghost\"\n").unwrap();
        assert_eq!(config_notify_route(Some(&home), &bare), NotifyRoute::Vacant);

        let _ = fs::remove_dir_all(&home);
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

    /// The ±2-day probe includes a rollout in the adjacent day directory.
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

    /// The scraper recovers an SGR-split exit hint from a recorded terminal
    /// stream after the emulator removes styling.
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
