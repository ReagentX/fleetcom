//! Codex does not let the caller select an ID at launch. This harness instead
//! injects a `notify` override, chains compatible configured notifiers, and
//! scans supported exit lines for an ID.

use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
};

use super::{
    CAPTURE_ENV, CapturePaths, Harness, Invocation, NOTIFY_CHAIN_ENV, SpawnPlan, capture_id,
    home_root, last_hint, leading_uuid, resolve_home, shell_quote,
};

pub struct Codex;

impl Harness for Codex {
    fn resolve_home(&self, env: &dyn Fn(&str) -> Option<PathBuf>) -> Option<PathBuf> {
        resolve_home(env, "CODEX_HOME", ".codex")
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
        capture_id(&v, "thread-id")
    }

    fn scrape_exit(&self, text: &str) -> Option<String> {
        let mut last = None;
        for line in text.lines() {
            // `Session ID:` has no program marker and can appear in captured
            // conversation text. Accept it only at the start of a row.
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

/// Classify the `notify` route declared in `config.toml`. The parser is
/// deliberately line-based: duplicate assignments are ambiguous and produce
/// [`NotifyRoute::Opaque`].
fn config_notify_route(home: Option<&Path>) -> NotifyRoute {
    let Some(root) = home_root(home, ".codex") else {
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::{
        harness::fixtures::{OTHER, assert_all_opaque, assert_corpus_scrape, paths},
        testutil::{CORPUS_COLS, Scratch, temp},
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
            codex_notify: PathBuf::from(r#"/Odd Path/it's "here"\now"#),
            ..paths()
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

    /// A fatal exit can name the session without printing a resume hint.
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

        // An indented or embedded label can be conversation text.
        for quoted in [
            format!("the log said Session ID: {ID}"),
            format!("• Session ID: {ID}"),
            format!("  Session ID: {ID}"),
        ] {
            assert_eq!(Codex.scrape_exit(&quoted), None, "{quoted:?}");
        }

        // Across lines, the last valid ID wins.
        let hint_last = format!("Session ID: {OTHER}\nrun codex resume {ID}");
        assert_eq!(Codex.scrape_exit(&hint_last).as_deref(), Some(ID));
        let id_last = format!("run codex resume {OTHER}\nSession ID: {ID}");
        assert_eq!(Codex.scrape_exit(&id_last).as_deref(), Some(ID));
    }

    /// Notification chaining reads `config.toml` and ignores sibling files.
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

    /// A preceding full-width row does not merge with the session-ID row after
    /// terminal emulation.
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
