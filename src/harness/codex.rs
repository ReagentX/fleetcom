//! Codex does not let the caller select an ID at launch. This harness instead
//! injects a `notify` override, chains compatible configured notifiers, and
//! scans every exit line that carries an ID. When neither channel yields an ID,
//! it correlates rollout files under
//! `<codex-home>/sessions/YYYY/MM/DD/rollout-<local-ts>-<uuid>.jsonl`.
//!
//! Those files are created lazily: `RolloutRecorder::new` precomputes the path
//! and defers creation until the first persisted item, so a session that never
//! received a prompt leaves no rollout at all and [`Codex::correlate_fs`]
//! returns `None`. That is the right answer — there is no conversation to
//! resume. Creation opens the path `O_APPEND|O_CREAT` in place rather than
//! writing a temp file and renaming it, so a zero-byte or half-written first
//! line is briefly observable; the `jzon::parse` guard in [`line1_admits`]
//! rejects one.

use std::{
    fmt::Write as _,
    fs,
    io::{BufRead, BufReader, Read},
    path::Path,
    time::SystemTime,
};

use super::{
    CAPTURE_ENV, CapturePaths, Harness, Invocation, NOTIFY_CHAIN_ENV, SpawnPlan, is_uuid,
    last_hint, leading_uuid, shell_quote, within_window_ms,
};

pub struct Codex;

impl Harness for Codex {
    fn home_env_var(&self) -> &'static str {
        "CODEX_HOME"
    }

    fn home_dot_dir(&self) -> &'static str {
        ".codex"
    }

    fn shape(&self) -> (&'static str, &'static str) {
        ("codex", "resume")
    }

    fn instrument(
        &self,
        // Both accepted shapes take the same injection; codex cannot pin an
        // ID at launch either way.
        _inv: &Invocation,
        capture: &CapturePaths,
        home: Option<&Path>,
    ) -> SpawnPlan {
        let chain = match config_notify_route(home) {
            // An explicit empty value prevents an inherited chain from
            // reaching the injected script.
            NotifyRoute::Vacant => String::new(),
            // A routed notifier rides along: the injected script execs this
            // argv, payload appended, after the capture write.
            NotifyRoute::Chain(argv) => argv.join("\n"),
            // Skip injection when the configured route cannot be encoded.
            NotifyRoute::Opaque => return SpawnPlan::default(),
        };
        let toml = format!(
            "notify=[\"{}\"]",
            toml_escape(&capture.codex_notify.to_string_lossy())
        );
        SpawnPlan {
            args_suffix: format!(" -c {}", shell_quote(&toml)),
            env: vec![
                (
                    CAPTURE_ENV.into(),
                    capture.capture_file.clone().into_os_string(),
                ),
                (NOTIFY_CHAIN_ENV.into(), chain.into()),
            ],
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
            // Fatal exit with no hint: codex `println!`s a bare
            // `Session ID: <uuid>`, the last channel left once the notify
            // hook has not fired and the rollout may be empty. The phrase
            // carries no program word to narrow it, so only a whole row at
            // offset 0 counts: rendered model output always carries a `• `
            // head or a two-space continuation indent and never reaches it.
            if let Some(rest) = line.strip_prefix("Session ID: ")
                && let Some(id) = leading_uuid(rest)
            {
                last = Some(id.to_string());
            }
            // Plain hint: `... run codex resume <uuid>`.
            if let Some(id) = last_hint(line, &["codex resume "]) {
                last = Some(id);
            }
            // Named-thread hint: `codex resume, then select <name> (<uuid>)`.
            // Only the parenthesized ID is trusted, never the name.
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

    fn correlate_fs(&self, cwd: &Path, spawned: SystemTime, home: Option<&Path>) -> Option<String> {
        let root = self.home_root(home)?;
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
            let (y, m, d) = crate::format::civil_from_days(day);
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
                // The stem opens with 19 timestamp characters
                // (`YYYY-MM-DDTHH-MM-SS`) and one `-`; the thread ID runs from
                // there to the first `_` or to the end. Reading the trailing 36
                // instead would return the rollout ID of a reverted thread's
                // `rollout-<ts>-<thread_id>_<rollout_id>.jsonl` — a valid UUID
                // naming the wrong conversation. 0.147.0 writes no such name;
                // codex main's `thread/revert` does.
                let Some(ids) = stem.get(20..) else {
                    continue;
                };
                let id = ids.split_once('_').map_or(ids, |(thread, _)| thread);
                if !is_uuid(id) {
                    continue;
                }
                // Correlate with the v7 ID's embedded UTC instant; the
                // filename timestamp is local wall-clock time.
                let Some(ms) = v7_millis(id) else { continue };
                if !within_window_ms(u128::from(ms), spawn_ms) {
                    continue;
                }
                if !line1_admits(&entry.path(), cwd) {
                    continue;
                }
                // A reverted thread keeps its ID and gains a second rollout,
                // so both names carry one conversation and both land in the
                // window the shared ID's v7 instant defines. The same uuid
                // twice is still one candidate.
                let id = id.to_string();
                if !survivors.contains(&id) {
                    survivors.push(id);
                }
            }
        }
        match survivors.as_slice() {
            [only] => Some(only.clone()),
            _ => None,
        }
    }
}

/// Whether Codex notification capture can preserve the configured route.
#[derive(Debug, PartialEq, Eq)]
enum NotifyRoute {
    /// No active route, so the capture notifier can run alone.
    Vacant,
    /// One representable route, executed after the capture write.
    Chain(Vec<String>),
    /// A route that cannot be represented without changing its argv. Capture
    /// injection is disabled so the route remains untouched.
    Opaque,
}

/// Classify the effective `notify` route from `config.toml`. Because this is
/// deliberately line-based rather than TOML-aware, two `notify` lines in one
/// file are ambiguous and produce [`NotifyRoute::Opaque`].
///
/// `config.toml` is the only file read, and the injected `-c` override is why
/// that suffices: it lands in codex's `SessionFlags` layer at precedence 30,
/// outranking every layer a user config can occupy — user config 20,
/// user-with-profile 21, project 25 — and enterprise-managed config too, at
/// 15. Only the two legacy managed layers, at 40 and 50, beat it.
fn config_notify_route(home: Option<&Path>) -> NotifyRoute {
    let Some(root) = Codex.home_root(home) else {
        return NotifyRoute::Vacant;
    };
    let text = fs::read_to_string(root.join("config.toml")).unwrap_or_default();
    let mut values = text.lines().filter_map(notify_value);
    let Some(value) = values.next() else {
        return NotifyRoute::Vacant;
    };
    if values.next().is_some() {
        return NotifyRoute::Opaque;
    }
    route_for(value)
}

/// Classify one notify assignment for the newline-delimited chain transport.
/// Newlines collide with the delimiter, empty elements disappear during shell
/// field splitting, and an empty array names no program. Each case is opaque.
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

/// Value after `=` of an uncommented bare `notify` assignment, or `None`.
fn notify_value(line: &str) -> Option<&str> {
    let rest = line.trim_start().strip_prefix("notify")?;
    rest.trim_start_matches([' ', '\t']).strip_prefix('=')
}

/// Parse a one-line TOML array of basic strings. Literal strings, non-string
/// elements, multiline arrays, and trailing junk return `None`. A trailing
/// comma or `#` comment remains valid.
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

/// Decode a TOML basic string after its opening quote and return the remaining
/// input after the closing quote. The parser accepts TOML's fixed escape set,
/// which is a superset of [`toml_escape`]'s output. Unknown escapes, malformed
/// Unicode escapes, and unterminated strings return `None`.
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

/// Extract the millisecond timestamp from a validated v7 UUID. Other versions
/// return `None`.
fn v7_millis(id: &str) -> Option<u64> {
    if id.as_bytes()[14] != b'7' {
        return None;
    }
    u64::from_str_radix(&format!("{}{}", &id[..8], &id[9..13]), 16).ok()
}

/// Check whether the rollout's first record names `cwd` and belongs to a thread
/// the user started. Subagent and guardian-review threads inherit the parent's
/// cwd and are minted within seconds of it, so cwd and the window alone leave
/// several rollouts standing and correlation collapses to `None`;
/// `thread_source: "subagent"` and a `parent_thread_id` are what separate them.
///
/// The test rejects rather than admits by name: codex's `ThreadSource`
/// deserializer turns any unknown string into `Feature(String)`, so requiring
/// `"user"` would silently drop legitimate future thread kinds. Absence is a
/// pass for the same reason — both fields are omitted when empty and neither
/// existed before 0.147.0, so pre-0.147 sessions stay resumable. Reads stop at
/// 64 KiB because later records do not participate in correlation.
fn line1_admits(path: &Path, cwd: &Path) -> bool {
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
    let payload = &meta["payload"];
    if payload["thread_source"].as_str() == Some("subagent")
        || !payload["parent_thread_id"].is_null()
    {
        return false;
    }
    // codex records the cwd its own process reports, and `getcwd(3)` resolves
    // symlinks: a task spawned in `/tmp/x` on macOS is recorded as
    // `/private/tmp/x` and never matches verbatim. Resolving this side is
    // enough — the recorded path is already physical.
    payload["cwd"].as_str().is_some_and(|c| {
        let recorded = Path::new(c);
        recorded == cwd || cwd.canonicalize().is_ok_and(|p| p == recorded)
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::{
        harness::fixtures::{OTHER, assert_all_opaque, assert_corpus_scrape, paths},
        testutil::{CORPUS_COLS, Scratch, temp, v7_at, write_rollout, write_rollout_named},
    };

    /// Codex's own launch and resume commands carry v7 IDs; the shared v4
    /// fixture stays valid for detection, which is version-agnostic.
    const ID: &str = "019f5453-de22-7240-b2e5-0d32692aa6d9";

    /// Codex-specific opaque shapes: subcommands (including `exec` and
    /// single-letter aliases), flags, `-c` overrides, `--resume` (the wrong
    /// selector), and out-of-position or named resume forms. The syntax
    /// shared by every harness is covered by the table test in
    /// `harness::tests`.
    #[test]
    fn everything_else_is_opaque_and_never_rewritten() {
        let opaque: Vec<String> = [
            "codex resume my-thread",
            "codex resume --last",
            "codex -m gpt-5",
            "codex -m gpt-5 'do x'",
            "codex e 'x'",
            "codex exec 'x'",
            "codex a",
            "codex -p team",
            r#"codex -c 'notify=["/my/hook"]'"#,
            "codex -\u{e9}x",
            "codexx",
        ]
        .iter()
        .map(|s| s.to_string())
        .chain([
            format!("codex resume {ID} -m gpt-5"),
            format!("codex -m gpt-5 resume {ID}"),
            format!("codex --resume {ID}"),
        ])
        .collect();
        assert_all_opaque(&Codex, ID, &opaque);
    }

    /// Scratch home without a `config.toml`.
    fn no_config_home() -> Scratch {
        temp("codex_no_config_home")
    }

    /// Both accepted shapes receive the same notify override because Codex
    /// cannot pin an ID at launch.
    #[test]
    fn instrument_installs_the_notify_override() {
        for cmd in ["codex".to_string(), format!("codex resume {ID}")] {
            let inv = Codex.detect(&cmd).unwrap();
            let plan = Codex.instrument(&inv, &paths(), Some(&no_config_home()));
            assert_eq!(
                plan.args_suffix, r#" -c 'notify=["/tmp/Application Support/notify.sh"]'"#,
                "{cmd}"
            );
            assert_eq!(plan.injected_id, None, "{cmd}");
            assert_eq!(
                plan.env,
                vec![
                    (
                        CAPTURE_ENV.into(),
                        PathBuf::from("/tmp/cap/session.json").into_os_string()
                    ),
                    // The explicit empty value overrides any inherited chain.
                    (NOTIFY_CHAIN_ENV.into(), "".into()),
                ],
                "{cmd}"
            );
        }
    }

    /// A representable `notify` assignment passes through
    /// [`NOTIFY_CHAIN_ENV`]. Comments, longer keys, and missing files do not
    /// define a route, so capture runs alone.
    #[test]
    fn instrument_chains_a_config_toml_notify() {
        let home = temp("codex_cfg_notify");
        let inv = Codex.detect("codex").unwrap();
        let chained = |plan: &SpawnPlan| {
            plan.env
                .iter()
                .find(|(k, _)| k == NOTIFY_CHAIN_ENV)
                .map(|(_, v)| v.clone())
        };

        // Missing config file: plain injection, and the chain is present but
        // empty.
        let plan = Codex.instrument(&inv, &paths(), Some(&home));
        assert!(!plan.args_suffix.is_empty());
        assert_eq!(chained(&plan), Some("".into()));

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
            assert_eq!(chained(&plan), Some("".into()), "{inert:?}");
        }
    }

    /// The newline-joined chain preserves spaces within argv elements.
    #[test]
    fn instrument_chains_the_vendor_desktop_entry() {
        let home = temp("codex_vendor_notify");
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
    }

    /// An unrepresentable route disables capture injection.
    #[test]
    fn instrument_skips_an_unrepresentable_config_notify() {
        let home = temp("codex_opaque_notify");
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

    /// `route_for` rejects parsed arrays that the chain transport would alter.
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

        // A named form without an ID yields nothing.
        assert_eq!(
            Codex.scrape_exit("run codex resume, then select my-thread"),
            None
        );
        assert_eq!(Codex.scrape_exit("codex resume my-thread"), None);

        // The last hint wins.
        let both = format!("run codex resume {OTHER}\n...\nrun codex resume, then select x ({ID})");
        assert_eq!(Codex.scrape_exit(&both).as_deref(), Some(ID));
    }

    /// A fatal exit prints no resume hint and names the ID outright. Nothing
    /// else recovers it: the notify hook never fired and the rollout may be
    /// empty.
    #[test]
    fn scrape_exit_reads_the_fatal_session_id_line() {
        assert_eq!(
            Codex.scrape_exit(&format!("Session ID: {ID}")).as_deref(),
            Some(ID)
        );

        // The label alone, a name, and a token-extending ID yield nothing.
        assert_eq!(Codex.scrape_exit("Session ID:"), None);
        assert_eq!(Codex.scrape_exit("Session ID: my session"), None);
        assert_eq!(Codex.scrape_exit(&format!("Session ID: {ID}ff")), None);

        // Off the start of the row the phrase is quoted text, not codex's own
        // line. A task killed before any exit line would otherwise resume on
        // it; refusing costs only the fall through to `correlate_fs`.
        for quoted in [
            format!("the log said Session ID: {ID}"),
            format!("• Session ID: {ID}"),
            format!("  Session ID: {ID}"),
        ] {
            assert_eq!(Codex.scrape_exit(&quoted), None, "{quoted:?}");
        }

        // Across lines, the last channel to speak wins, either way round.
        let hint_last = format!("Session ID: {OTHER}\nrun codex resume {ID}");
        assert_eq!(Codex.scrape_exit(&hint_last).as_deref(), Some(ID));
        let id_last = format!("run codex resume {OTHER}\nSession ID: {ID}");
        assert_eq!(Codex.scrape_exit(&id_last).as_deref(), Some(ID));
    }

    /// A half-migrated home — a legacy `profile` key beside the file it once
    /// selected — must not chain the profile's notifier: 0.147.0 refuses to
    /// start on that key at all, and layers `<profile>.config.toml` only under
    /// `-p`, a command this harness never instruments. Chaining it would run a
    /// notifier codex itself would not.
    #[test]
    fn config_notify_route_reads_config_toml_alone() {
        let home = temp("codex_profile_notify");
        let cfg = home.join("config.toml");
        fs::write(home.join("team.config.toml"), "notify = [\"/team/hook\"]\n").unwrap();

        fs::write(&cfg, "profile = \"team\"\n").unwrap();
        assert_eq!(config_notify_route(Some(&home)), NotifyRoute::Vacant);

        // The base file's own assignment is the only one that counts.
        fs::write(&cfg, "profile = \"team\"\nnotify = [\"/base/hook\"]\n").unwrap();
        assert_eq!(
            config_notify_route(Some(&home)),
            NotifyRoute::Chain(vec!["/base/hook".to_string()])
        );

        // Two assignment lines remain ambiguous.
        fs::write(&cfg, "notify = [\"/a\"]\nnotify = [\"/b\"]\n").unwrap();
        assert_eq!(config_notify_route(Some(&home)), NotifyRoute::Opaque);
    }

    #[test]
    fn correlate_fs_requires_a_unique_cwd_matched_rollout() {
        let home = temp("codex_correlate");
        let spawn_ms: u64 = 1_785_000_000_000; // 2026-07-25T02:40Z
        let spawned = SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(spawn_ms);

        let id = write_rollout(&home, spawn_ms + 4_000, 1, Path::new("/work/proj"));
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
        write_rollout(&home, spawn_ms + 90_000, 2, Path::new("/late/proj"));
        assert_eq!(
            Codex.correlate_fs(Path::new("/late/proj"), spawned, Some(&home)),
            None
        );

        // Two in-window rollouts from the same directory are ambiguous.
        write_rollout(&home, spawn_ms + 8_000, 3, Path::new("/work/proj"));
        assert_eq!(
            Codex.correlate_fs(Path::new("/work/proj"), spawned, Some(&home)),
            None
        );
    }

    /// Subagent and guardian-review threads mint their own ID, write their own
    /// rollout, and inherit the parent's cwd, so cwd and the window alone leave
    /// several rollouts standing. Line 1's provenance fields are what separate
    /// them, and either one alone disqualifies a file.
    #[test]
    fn correlate_fs_excludes_spawned_threads() {
        let home = temp("codex_subagent");
        let spawn_ms: u64 = 1_785_000_000_000;
        let spawned = SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(spawn_ms);
        let cwd = Path::new("/work/proj");
        let parent = write_rollout(&home, spawn_ms + 1_000, 1, cwd);
        let resolves = |home: &Path| Codex.correlate_fs(cwd, spawned, Some(home));

        // `thread_source` alone, as 0.147.0 writes it for a spawned thread.
        write_rollout_named(
            &home,
            spawn_ms + 3_000,
            2,
            cwd,
            "",
            r#","source":{"subagent":{"other":"guardian"}},"thread_source":"subagent""#,
        );
        assert_eq!(resolves(&home).as_deref(), Some(parent.as_str()));

        // `parent_thread_id` alone: any value at all names a spawning thread.
        write_rollout_named(
            &home,
            spawn_ms + 5_000,
            3,
            cwd,
            "",
            &format!(r#","parent_thread_id":"{parent}""#),
        );
        assert_eq!(resolves(&home).as_deref(), Some(parent.as_str()));

        // A second rollout carrying neither field is a real sibling, and
        // uniqueness fails as it always has.
        write_rollout(&home, spawn_ms + 7_000, 4, cwd);
        assert_eq!(resolves(&home), None);
    }

    /// Disqualify by field rather than requiring `thread_source: "user"`:
    /// codex's deserializer turns any unknown string into `Feature(String)`, so
    /// an allow-list would drop future thread kinds. Absence passes too, which
    /// is what keeps pre-0.147 rollouts resumable — the case above already
    /// leans on it.
    #[test]
    fn correlate_fs_admits_thread_sources_it_does_not_know() {
        let spawn_ms: u64 = 1_785_000_000_000;
        let spawned = SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(spawn_ms);
        let cwd = Path::new("/work/proj");
        for source in ["user", "some_future_kind"] {
            let home = temp("codex_thread_source");
            let id = write_rollout_named(
                &home,
                spawn_ms + 1_000,
                1,
                cwd,
                "",
                &format!(r#","thread_source":"{source}""#),
            );
            assert_eq!(
                Codex.correlate_fs(cwd, spawned, Some(&home)).as_deref(),
                Some(id.as_str()),
                "{source:?}"
            );
        }
    }

    /// A thread created by `thread/revert` carries a second ID in its filename.
    /// The trailing 36 characters are then the rollout ID — a valid UUID naming
    /// a different object — so the thread ID is read from a fixed offset.
    #[test]
    fn correlate_fs_reads_the_thread_id_not_the_rollout_id() {
        let spawn_ms: u64 = 1_785_000_000_000;
        let spawned = SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(spawn_ms);
        let cwd = Path::new("/work/proj");
        let rollout_id = v7_at(spawn_ms + 1_000, 9);
        // One home throughout: revert keeps the thread ID and adds a rollout
        // rather than replacing one, so the two names coexist and the second
        // pass proves the pair still resolves to a single conversation.
        let home = temp("codex_revert_name");
        for suffix in [String::new(), format!("_{rollout_id}")] {
            let thread = write_rollout_named(&home, spawn_ms + 1_000, 1, cwd, &suffix, "");
            assert_ne!(thread, rollout_id);
            assert_eq!(
                Codex.correlate_fs(cwd, spawned, Some(&home)).as_deref(),
                Some(thread.as_str()),
                "{suffix:?}"
            );
        }
    }

    /// codex records the cwd `getcwd(3)` reports, which has resolved every
    /// symlink; fleetcom holds the path the task was spawned with. On macOS a
    /// task under `/tmp` is recorded as `/private/tmp` and a verbatim compare
    /// never matches.
    #[test]
    fn correlate_fs_matches_a_symlinked_spawn_path() {
        let home = temp("codex_symlink_cwd");
        let spawn_ms: u64 = 1_785_000_000_000;
        let spawned = SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(spawn_ms);

        let real = home.join("real");
        fs::create_dir_all(&real).unwrap();
        let link = home.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        // The rollout names the resolved path; the task carries the link.
        let id = write_rollout(&home, spawn_ms + 1_000, 1, &real.canonicalize().unwrap());
        assert_eq!(
            Codex.correlate_fs(&link, spawned, Some(&home)).as_deref(),
            Some(id.as_str())
        );
        // An unrelated directory still fails, resolved or not.
        assert_eq!(Codex.correlate_fs(&home, spawned, Some(&home)), None);
    }

    /// The ±2-day probe includes a rollout in the adjacent day directory.
    #[test]
    fn correlate_fs_spans_adjacent_day_directories() {
        let home = temp("codex_dayspan");
        let spawn_ms: u64 = 1_785_000_000_000;
        let spawned = SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(spawn_ms);

        let id = v7_at(spawn_ms + 2_000, 7);
        let (y, m, d) = crate::format::civil_from_days((spawn_ms / 86_400_000) as i64 - 1);
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
    }

    /// The anchor has to survive emulation, not just `str::lines`.
    /// `text_with_history` joins soft-wrapped rows into one logical line, so a
    /// preceding row that exactly fills the width is the case that could push
    /// the fatal line off offset 0.
    #[test]
    fn fatal_session_id_holds_offset_zero_after_a_full_width_row() {
        let bytes = format!("{}\r\nSession ID: {ID}\r\n", "x".repeat(CORPUS_COLS));
        assert_corpus_scrape(&Codex, bytes.as_bytes(), ID);
    }

    /// The scraper recovers an SGR-split exit hint from the corpus bytes after
    /// terminal emulation removes the styling.
    #[test]
    fn corpus_scrape_recovers_the_exit_hint_id() {
        assert_corpus_scrape(
            &Codex,
            include_bytes!("../../tests/corpus/codex_resume.bin"),
            "019f5453-de22-7240-b2e5-0d32692aa6d9",
        );
    }
}
