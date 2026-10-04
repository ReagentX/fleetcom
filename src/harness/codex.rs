//! Codex does not let the caller select an ID at launch. This harness instead
//! injects a `notify` override and chains compatible configured notifiers.
//!
//! Every thread a Codex process runs reports through that notifier: the
//! conversation on screen, each sub-agent it spawns, and the hidden thread the
//! TUI starts to title a new session. Only the first is the task's
//! conversation. `codex resume` exits 1 for a sub-agent whose parent is not
//! loaded, and for the title thread, which is never saved. `parse_capture`
//! therefore resolves the notified thread to the root thread of its session
//! tree through that thread's rollout header, and refuses a thread it cannot
//! classify.

use std::{
    fmt::Write as _,
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
};

use super::{
    CAPTURE_ENV, CapturePaths, Harness, Invocation, NOTIFY_CHAIN_ENV, SpawnPlan, capture_id,
    home_root, resolve_home, shell_quote,
};

/// Config override that makes an instrumented launch's embedded mode explicit.
/// Since codex 0.157.0 a plain `codex` attaches to a shared background server,
/// and a `-c` override outside a short allowlist forces embedded mode. `notify`
/// is not on that list, so the notify override alone raises a "Running without
/// the shared background server" warning at startup. Codex emits the warning
/// only while this feature is enabled, and the key is on the allowlist.
/// Embedded mode is deliberate: `fleetcom` owns the lifecycle of the task's
/// process, so that process must hold the conversation.
///
/// `--no-daemon` selects the same mode, but a codex that predates the flag
/// refuses to start on it. An unknown `features.*` key only adds an
/// unrecognized-setting warning.
const EMBEDDED_OVERRIDE: &str = "features.daemon_auto_start=false";

/// Upper bound on a rollout's first line, terminator included. The line embeds
/// the session's base instructions: about 22 KB in rollouts written by codex
/// 0.135.0 through 0.160.0. 1 MiB leaves that text room to grow over 45-fold
/// and bounds the read from a file whose first line never ends.
const HEADER_MAX: u64 = 1024 * 1024;

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
            // Set an explicit empty value to exclude an inherited chain from the
            // injected script.
            NotifyRoute::Vacant => String::new(),
            // Chain the configured notifier after the capture write: exec this argv
            // with the payload appended from the injected script.
            NotifyRoute::Chain(argv) => argv.join("\n"),
            // Skip injection when the configured route cannot be encoded.
            NotifyRoute::Opaque => return SpawnPlan::default(),
        };
        let toml = format!(
            "notify=[\"{}\"]",
            toml_escape(&capture.codex_notify.to_string_lossy())
        );
        SpawnPlan {
            args_suffix: format!(
                " -c {} -c {}",
                shell_quote(&toml),
                shell_quote(EMBEDDED_OVERRIDE)
            ),
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

    /// Accept an `agent-turn-complete` notification and return the root
    /// thread of the notified thread's session tree: the conversation the
    /// task's TUI is on. A sub-agent's notification maps to its root; a thread
    /// [`root_thread`] cannot classify is refused, and the next ID source
    /// decides.
    ///
    /// The session store is not scanned to infer which conversation the task
    /// owns: nothing is discovered there. The task's own notifier names one
    /// thread, and that thread's rollout header classifies it.
    fn parse_capture(
        &self,
        payload: &str,
        // Every thread of the task notifies from the task's own process.
        _pid: Option<u32>,
        home: Option<&Path>,
    ) -> Option<String> {
        let v = jzon::parse(payload).ok()?;
        if v["type"].as_str() != Some("agent-turn-complete") {
            return None;
        }
        let thread = capture_id(&v, "thread-id")?;
        root_thread(&home_root(home, ".codex")?, &thread)
    }
}

/// Resolve `thread` to the root thread of its session tree from the header of
/// its rollout under the Codex `home`. `thread` must already satisfy
/// [`is_uuid`](super::is_uuid): it is matched against file names.
///
/// Codex saves each thread as
/// `sessions/<YYYY>/<MM>/<DD>/rollout-<local time>-<thread>.jsonl`. The first
/// line is `{"type":"session_meta","payload":{…}}`: `payload.id` is the thread
/// and `payload.session_id` is the root thread of its tree. A root thread
/// carries its own ID in both and no `subagent` member in `source`. A
/// sub-agent carries the root's ID in `session_id` and a `source` object with
/// a `subagent` member.
///
/// Return `None` unless exactly one rollout is named for `thread` and its
/// header has one of those two shapes. The title thread has no rollout. Only
/// directory listings and that first line are examined.
fn root_thread(home: &Path, thread: &str) -> Option<String> {
    let mut rollouts = Vec::new();
    collect_rollouts(
        &home.join("sessions"),
        // Year, month, and day directories.
        3,
        &format!("-{thread}.jsonl"),
        &mut rollouts,
    )?;
    let [rollout] = rollouts.as_slice() else {
        return None;
    };
    let v = jzon::parse(&read_header(rollout)?).ok()?;
    let meta = &v["payload"];
    if v["type"].as_str() != Some("session_meta") || meta["id"].as_str() != Some(thread) {
        return None;
    }
    let root = capture_id(meta, "session_id")?;
    // Exactly one may hold. codex 0.140.0 and 0.141.0 wrote a sub-agent's own
    // ID as its `session_id`: that header names no root.
    ((root == thread) != meta["source"].has_key("subagent")).then_some(root)
}

/// Push every entry whose name ends with `suffix` and that sits exactly
/// `depth` directory levels below `dir`. Entries above that level that are not
/// directories are skipped. Return `None` when a directory cannot be listed:
/// it may hold a second match.
fn collect_rollouts(dir: &Path, depth: u8, suffix: &str, found: &mut Vec<PathBuf>) -> Option<()> {
    for entry in fs::read_dir(dir).ok()? {
        let entry = entry.ok()?;
        if depth == 0 {
            if entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.ends_with(suffix))
            {
                found.push(entry.path());
            }
        } else if entry.path().is_dir() {
            collect_rollouts(&entry.path(), depth - 1, suffix, found)?;
        }
    }
    Some(())
}

/// Read the first line of `path` without its terminator. Return `None` when
/// the first [`HEADER_MAX`] bytes hold no newline: the file is empty, Codex is
/// still writing the line, or the line exceeds the bound. Also return `None`
/// for an unreadable file and for a line that is not UTF-8.
fn read_header(path: &Path) -> Option<String> {
    let mut line = Vec::new();
    BufReader::new(fs::File::open(path).ok()?)
        .take(HEADER_MAX)
        .read_until(b'\n', &mut line)
        .ok()?;
    if line.pop() != Some(b'\n') {
        return None;
    }
    String::from_utf8(line).ok()
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

/// Read bare top-level keys until the first table. Unsupported syntax disables
/// injection: it may contain a notifier that this reader cannot preserve.
fn config_notify_route(home: Option<&Path>) -> NotifyRoute {
    let Some(root) = home_root(home, ".codex") else {
        return NotifyRoute::Vacant;
    };
    let text = match fs::read_to_string(root.join("config.toml")) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return NotifyRoute::Vacant,
        Err(_) => return NotifyRoute::Opaque,
    };
    let mut route = None;
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // TOML cannot return to the root table after a table header. Values
        // above it must be complete so a header inside a string cannot stop us.
        if line.starts_with('[') {
            break;
        }
        let Some((key, value)) = line.split_once('=') else {
            return NotifyRoute::Opaque;
        };
        let key = key.trim();
        if key.is_empty()
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        {
            return NotifyRoute::Opaque;
        }
        if key == "notify" {
            if route.is_some() {
                return NotifyRoute::Opaque;
            }
            let Some(argv) = parse_notify_array(value) else {
                return NotifyRoute::Opaque;
            };
            if argv
                .iter()
                .any(|a| a.is_empty() || a.contains(['\n', '\0']))
            {
                return NotifyRoute::Opaque;
            }
            route = Some(if argv.is_empty() {
                NotifyRoute::Vacant
            } else {
                NotifyRoute::Chain(argv)
            });
        } else if !complete_value(value.trim()) {
            return NotifyRoute::Opaque;
        }
    }
    route.unwrap_or(NotifyRoute::Vacant)
}

/// Recognize complete single-line values without interpreting unrelated settings.
/// Accept only basic strings in arrays; exclude multiline values and inline tables.
fn complete_value(value: &str) -> bool {
    if value.starts_with("\"\"\"") || value.starts_with("'''") {
        return false;
    }
    let tail = if let Some(rest) = value.strip_prefix('"') {
        parse_basic_string(rest).map(|(_, tail)| tail)
    } else if let Some(rest) = value.strip_prefix('\'') {
        rest.find('\'').map(|i| &rest[i + 1..])
    } else if value.starts_with('[') {
        return parse_notify_array(value).is_some();
    } else {
        let scalar = value.split('#').next().unwrap_or_default().trim();
        return matches!(scalar, "true" | "false") || scalar.parse::<f64>().is_ok();
    };
    tail.is_some_and(|tail| {
        let tail = tail.trim();
        tail.is_empty() || tail.starts_with('#')
    })
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
        harness::fixtures::{assert_all_opaque, paths},
        testutil::{Scratch, codex_session_meta, install_codex_rollout, install_codex_root, temp},
    };

    /// Codex's own launch and resume commands carry v7 IDs; the shared v4
    /// fixture stays valid for detection, which is version-agnostic.
    const ID: &str = "019f5453-de22-7240-b2e5-0d32692aa6d9";
    /// A sub-agent thread and a second-level sub-agent thread in the session
    /// rooted at [`ID`].
    const CHILD: &str = "019f5454-0c11-7b33-9a4e-5f0e6d7c8b9a";
    const GRANDCHILD: &str = "019f5454-3d70-7e02-b1c8-2a4b6c8d0e1f";
    /// The hidden title thread: it notifies but has no rollout.
    const TITLE: &str = "019f5453-de9f-7e61-8c0d-1a2b3c4d5e6f";

    /// Suffix of every instrumented launch that uses [`paths`].
    const SUFFIX: &str = concat!(
        r#" -c 'notify=["/tmp/Application Support/notify.sh"]'"#,
        " -c 'features.daemon_auto_start=false'",
    );

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

    /// Both accepted shapes receive the same two overrides because Codex
    /// cannot pin an ID at launch: the notifier, then explicit embedded mode.
    #[test]
    fn instrument_installs_the_notify_and_embedded_overrides() {
        for cmd in ["codex".to_string(), format!("codex resume {ID}")] {
            let inv = Codex.detect(&cmd).unwrap();
            let plan = Codex.instrument(&inv, &paths(), Some(&no_config_home()));
            assert_eq!(plan.args_suffix, SUFFIX, "{cmd}");
            assert_eq!(plan.injected_id, None, "{cmd}");
            assert_eq!(
                plan.env,
                vec![
                    (
                        CAPTURE_ENV.into(),
                        PathBuf::from("/tmp/cap/session.json").into_os_string()
                    ),
                    // Override any inherited chain with an explicit empty value.
                    (NOTIFY_CHAIN_ENV.into(), "".into()),
                ],
                "{cmd}"
            );
        }
    }

    /// Pass a representable `notify` assignment through [`NOTIFY_CHAIN_ENV`]. Run
    /// capture alone for comments, longer keys, and missing files: no route is
    /// configured.
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
            // Include the capture environment when chaining a notifier.
            assert!(plan.env.iter().any(|(k, _)| k == CAPTURE_ENV), "{active:?}");
        }
        for inert in [
            "# notify = [\"/my/thing\"]\n",
            "  # notify = [\"/my/thing\"]\n",
            "notify_extra = 1\n",
            "notify = []\n",
        ] {
            fs::write(&cfg, inert).unwrap();
            let plan = Codex.instrument(&inv, &paths(), Some(&home));
            assert!(!plan.args_suffix.is_empty(), "{inert:?}");
            assert_eq!(chained(&plan), Some("".into()), "{inert:?}");
        }
    }

    /// Preserve spaces within argv elements in the newline-joined chain.
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
        assert_eq!(plan.args_suffix, SUFFIX);
        assert!(plan.env.contains(&(
            NOTIFY_CHAIN_ENV.into(),
            "/Applications/Codex Computer Use.app/Contents/MacOS/SkyComputerUseClient\nturn-ended"
                .into()
        )));
    }

    /// Disable capture injection for an unrepresentable route: the launch
    /// receives neither override and no environment.
    #[test]
    fn instrument_skips_an_unrepresentable_config_notify() {
        let home = temp("codex_opaque_notify");
        let cfg = home.join("config.toml");
        let inv = Codex.detect("codex").unwrap();
        for opaque in [
            // Malformed TOML cannot identify an active route.
            "notify = [\n",
            "notify\n",
            "notify = [1]\n",
            "notify = [\"a\\u0000b\"]\n",
            // Empty element: the script's field split would drop it.
            "notify = [\"\"]\n",
            // Embedded newline: the chain encoding's delimiter.
            "notify = [\"a\\nb\"]\n",
            // Not an array.
            "notify = \"/my/thing\"\n",
            // Duplicate top-level assignments are invalid TOML.
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
            format!(
                "-c\n{}\n-c\nfeatures.daemon_auto_start=false\n",
                r#"notify=["/Odd Path/it's \"here\"\\now"]"#
            )
        );
    }

    /// Unknown syntax must not be mistaken for an absent notifier.
    #[test]
    fn config_notify_route_declines_unsupported_root_syntax() {
        let home = temp("codex_unknown_notify");
        for text in [
            "\"notify\" = [\"/hook\"]",
            "'notify' = ['/hook']",
            "notify = ['/hook']",
            "notify = [\n  \"/hook\",\n]",
            "description = '''\n[other]\n'''\nnotify = [\"/hook\"]",
            "description = \"\"\"\nnotify = [\"/quoted\"]\n\"\"\"",
            "other = [\n  \"value\",\n]\nnotify = [\"/hook\"]",
            "other = { value = 1 }\nnotify = [\"/hook\"]",
            "other.key = true\nnotify = [\"/hook\"]",
        ] {
            fs::write(home.join("config.toml"), text).unwrap();
            assert_eq!(
                config_notify_route(Some(&home)),
                NotifyRoute::Opaque,
                "{text:?}"
            );
        }
    }

    #[test]
    fn config_notify_route_stops_at_tables_after_complete_values() {
        let home = temp("codex_root_notify");
        let preamble = "model = \"example\" # comment\nname = 'literal'\nenabled = true\nlimit = 42\nother = [\"a\", \"b\"]\n";
        for (root, expected) in [
            ("", NotifyRoute::Vacant),
            ("notify = []\n", NotifyRoute::Vacant),
            (
                "notify = [\"/hook\"]\n",
                NotifyRoute::Chain(vec!["/hook".into()]),
            ),
        ] {
            fs::write(
                home.join("config.toml"),
                format!("{preamble}{root}[other]\nnotify = [\"/ignored\"]\n"),
            )
            .unwrap();
            assert_eq!(config_notify_route(Some(&home)), expected);
        }
    }

    #[test]
    fn config_notify_route_skips_unreadable_config() {
        let home = temp("codex_unreadable_notify");
        fs::create_dir(home.join("config.toml")).unwrap();
        assert_eq!(config_notify_route(Some(&home)), NotifyRoute::Opaque);
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

    /// Notification JSON for a completed turn of `thread`.
    fn turn_complete(thread: &str) -> String {
        format!(
            r#"{{"type":"agent-turn-complete","thread-id":"{thread}","turn-id":"t","cwd":"/w"}}"#
        )
    }

    /// Resolve a completed-turn notification for `thread` against `home`.
    fn resolve(home: &Path, thread: &str) -> Option<String> {
        Codex.parse_capture(&turn_complete(thread), None, Some(home))
    }

    /// `session_meta` payload with raw JSON for `source`.
    fn meta(id: &str, session: &str, source: &str) -> String {
        format!(r#"{{"id":"{id}","session_id":"{session}","source":{source}}}"#)
    }

    /// `session_meta` payload of a sub-agent of `parent` in the session
    /// rooted at `root`, with raw JSON for `source`.
    fn child_meta(id: &str, parent: &str, root: &str, source: &str) -> String {
        format!(
            r#"{{"id":"{id}","session_id":"{root}","parent_thread_id":"{parent}","source":{source}}}"#
        )
    }

    /// `source` of a thread the model's `spawn_agent` tool started.
    fn thread_spawn(parent: &str, depth: u8) -> String {
        format!(
            r#"{{"subagent":{{"thread_spawn":{{"parent_thread_id":"{parent}","depth":{depth},"agent_path":"/root/pong","agent_nickname":"Pong"}}}}}}"#
        )
    }

    /// The payload checks precede the rollout lookup: with the root's rollout
    /// in place, only a turn-complete payload naming a strict ID resolves.
    #[test]
    fn parse_capture_accepts_only_turn_complete_payloads() {
        let home = temp("codex_capture_payload");
        install_codex_root(&home, ID);
        let parse = |payload: &str| Codex.parse_capture(payload, None, Some(&home));
        assert_eq!(parse(&turn_complete(ID)).as_deref(), Some(ID));

        let wrong_type = format!(r#"{{"type":"other","thread-id":"{ID}"}}"#);
        assert_eq!(parse(&wrong_type), None);
        for thread in ["my session", "../../../config", "*", ""] {
            let payload = format!(r#"{{"type":"agent-turn-complete","thread-id":"{thread}"}}"#);
            assert_eq!(parse(&payload), None, "{thread:?}");
        }
        assert_eq!(parse(r#"{"type":"agent-turn-complete"}"#), None);
        assert_eq!(parse("not json"), None);
        assert_eq!(parse(""), None);
    }

    /// A root thread carries its own ID as `session_id` under every observed
    /// string `source`. The task's PID plays no part.
    #[test]
    fn parse_capture_accepts_a_root_thread() {
        for source in ["cli", "vscode", "exec"] {
            let home = temp("codex_capture_root");
            install_codex_rollout(
                &home,
                ID,
                codex_session_meta(&meta(ID, ID, &format!("\"{source}\""))),
            );
            assert_eq!(resolve(&home, ID).as_deref(), Some(ID), "{source}");
            assert_eq!(
                Codex
                    .parse_capture(&turn_complete(ID), Some(4242), Some(&home))
                    .as_deref(),
                Some(ID),
                "{source}"
            );
        }
    }

    /// A spawned sub-agent resolves to `session_id`, not to its parent: the
    /// second-level thread's parent is itself a sub-agent. The root's own
    /// rollout is not consulted, so none is installed.
    #[test]
    fn parse_capture_maps_a_spawned_sub_agent_to_its_root() {
        let home = temp("codex_capture_spawned");
        install_codex_rollout(
            &home,
            CHILD,
            codex_session_meta(&child_meta(CHILD, ID, ID, &thread_spawn(ID, 1))),
        );
        install_codex_rollout(
            &home,
            GRANDCHILD,
            codex_session_meta(&child_meta(GRANDCHILD, CHILD, ID, &thread_spawn(CHILD, 2))),
        );
        assert_eq!(resolve(&home, CHILD).as_deref(), Some(ID));
        assert_eq!(resolve(&home, GRANDCHILD).as_deref(), Some(ID));
        assert_eq!(resolve(&home, ID), None, "the root has no rollout here");
    }

    /// The guardian sub-agent's `source` names no parent; the `subagent`
    /// member alone classifies it.
    #[test]
    fn parse_capture_maps_a_guardian_sub_agent_to_its_root() {
        let home = temp("codex_capture_guardian");
        install_codex_rollout(
            &home,
            CHILD,
            codex_session_meta(&child_meta(
                CHILD,
                ID,
                ID,
                r#"{"subagent":{"other":"guardian"}}"#,
            )),
        );
        assert_eq!(resolve(&home, CHILD).as_deref(), Some(ID));
    }

    /// codex 0.140.0 and 0.141.0 wrote a sub-agent's own ID as `session_id`.
    /// Such a header has the root's `id == session_id` shape and a `subagent`
    /// source: it names no root, and the sub-agent's ID is not resumable.
    #[test]
    fn parse_capture_refuses_a_sub_agent_that_names_itself_as_root() {
        let home = temp("codex_capture_self_rooted");
        for source in [
            thread_spawn(ID, 1),
            r#"{"subagent":{"other":"guardian"}}"#.to_string(),
            r#"{"subagent":"review"}"#.to_string(),
        ] {
            install_codex_rollout(
                &home,
                CHILD,
                codex_session_meta(&child_meta(CHILD, ID, CHILD, &source)),
            );
            assert_eq!(resolve(&home, CHILD), None, "{source}");
        }
    }

    /// The title thread notifies but is never saved: no rollout names it,
    /// whether the store is absent, empty, or holds other threads.
    #[test]
    fn parse_capture_refuses_a_thread_without_a_rollout() {
        let home = temp("codex_capture_title");
        assert_eq!(resolve(&home, TITLE), None, "no sessions directory");
        fs::create_dir_all(home.join("sessions/2026/10/04")).unwrap();
        assert_eq!(resolve(&home, TITLE), None, "empty day directory");
        install_codex_root(&home, ID);
        assert_eq!(resolve(&home, TITLE), None, "another thread's rollout");
        assert_eq!(resolve(&home, ID).as_deref(), Some(ID));
    }

    /// Rollouts sit exactly three directories below `sessions`. A matching
    /// name at any other depth is not a rollout, and a file among the
    /// directories does not stop the search.
    #[test]
    fn parse_capture_finds_rollouts_only_in_day_directories() {
        let home = temp("codex_capture_depth");
        let name = format!("rollout-2026-10-04T13-49-56-{ID}.jsonl");
        let header = codex_session_meta(&meta(ID, ID, r#""cli""#));
        for dir in [
            "sessions",
            "sessions/2026",
            "sessions/2026/10",
            "sessions/2026/10/04/extra",
        ] {
            let dir = home.join(dir);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(&name), &header).unwrap();
        }
        assert_eq!(resolve(&home, ID), None);

        fs::write(home.join("sessions/2026/10/04").join(&name), &header).unwrap();
        fs::write(home.join("sessions/.DS_Store"), "").unwrap();
        assert_eq!(resolve(&home, ID).as_deref(), Some(ID));
    }

    /// Two rollouts for one thread are ambiguous, in one day directory or
    /// across two. Removing the extra restores the capture.
    #[test]
    fn parse_capture_refuses_two_rollouts_for_one_thread() {
        let header = codex_session_meta(&meta(ID, ID, r#""cli""#));
        for day in ["2026/10/04", "2026/10/05", "2027/01/01"] {
            let home = temp("codex_capture_twins");
            install_codex_root(&home, ID);
            let dir = home.join("sessions").join(day);
            fs::create_dir_all(&dir).unwrap();
            let twin = dir.join(format!("rollout-2026-10-05T09-00-00-{ID}.jsonl"));
            fs::write(&twin, &header).unwrap();
            assert_eq!(resolve(&home, ID), None, "{day}");
            fs::remove_file(&twin).unwrap();
            assert_eq!(resolve(&home, ID).as_deref(), Some(ID), "{day}");
        }
    }

    /// A day directory that cannot be listed may hold a second rollout, so
    /// the thread's one visible rollout is not enough.
    #[test]
    fn parse_capture_refuses_a_store_it_cannot_list() {
        use std::os::unix::fs::PermissionsExt;
        let home = temp("codex_capture_unlistable");
        install_codex_root(&home, ID);
        let locked = home.join("sessions/2026/10/05");
        fs::create_dir_all(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let refused = resolve(&home, ID);
        // Restore access before asserting so the scratch tree stays removable.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(refused, None);
        assert_eq!(resolve(&home, ID).as_deref(), Some(ID));
    }

    /// The header must describe the notified thread and place it in a
    /// session tree: every other shape is unclassified.
    #[test]
    fn parse_capture_refuses_an_unclassified_header() {
        let spawned = thread_spawn(ID, 1);
        for (what, header) in [
            (
                "the header's id is another thread",
                codex_session_meta(&meta(CHILD, CHILD, r#""cli""#)),
            ),
            (
                "a sub-agent's header that names the notified thread as its root",
                codex_session_meta(&child_meta(CHILD, ID, ID, &spawned)),
            ),
            (
                "no session_id",
                codex_session_meta(&format!(r#"{{"id":"{ID}","source":"cli"}}"#)),
            ),
            (
                "session_id is not a string",
                codex_session_meta(&format!(r#"{{"id":"{ID}","session_id":7,"source":"cli"}}"#)),
            ),
            (
                "session_id is not a strict ID",
                codex_session_meta(&meta(ID, "x'; rm -rf ~'", &spawned)),
            ),
            (
                "session_id is uppercase",
                codex_session_meta(&meta(ID, &CHILD.to_uppercase(), &spawned)),
            ),
            (
                "a foreign session_id under a string source",
                codex_session_meta(&meta(ID, CHILD, r#""cli""#)),
            ),
            (
                "a foreign session_id without a source",
                codex_session_meta(&format!(r#"{{"id":"{ID}","session_id":"{CHILD}"}}"#)),
            ),
            (
                "a foreign session_id under an object source that names no sub-agent",
                codex_session_meta(&meta(ID, CHILD, r#"{"custom":"x"}"#)),
            ),
            (
                "a foreign session_id under a string source spelled like the member",
                codex_session_meta(&meta(ID, CHILD, r#""subagent""#)),
            ),
            (
                "the wrong record type",
                format!(
                    r#"{{"timestamp":"2026-10-04T17:49:56.012Z","ordinal":0,"type":"turn_context","payload":{}}}"#,
                    meta(ID, ID, r#""cli""#)
                ) + "\n",
            ),
            (
                "no record type",
                format!(r#"{{"payload":{}}}"#, meta(ID, ID, r#""cli""#)) + "\n",
            ),
            ("no payload", "{\"type\":\"session_meta\"}\n".to_string()),
            ("a JSON array", "[]\n".to_string()),
            ("not JSON", "not json\n".to_string()),
            (
                "a blank first line",
                format!("\n{}", codex_session_meta(&meta(ID, ID, r#""cli""#))),
            ),
        ] {
            let home = temp("codex_capture_unclassified");
            install_codex_rollout(&home, ID, header);
            assert_eq!(resolve(&home, ID), None, "{what}");
        }
    }

    /// A reader that races the header's write can observe any prefix of it.
    /// Only the terminated line is a header: the empty file and the complete
    /// object without its newline are both refused.
    #[test]
    fn parse_capture_refuses_empty_and_torn_headers() {
        let home = temp("codex_capture_torn");
        let header = codex_session_meta(&meta(ID, ID, r#""cli""#));
        for cut in 0..header.len() {
            install_codex_rollout(&home, ID, &header[..cut]);
            assert_eq!(resolve(&home, ID), None, "{:?}", &header[..cut]);
        }
        install_codex_rollout(&home, ID, &header);
        assert_eq!(resolve(&home, ID).as_deref(), Some(ID));
    }

    /// The first line must end within [`HEADER_MAX`] bytes, terminator
    /// included. The bound applies to the line, not the file.
    #[test]
    fn parse_capture_bounds_the_header_line() {
        let home = temp("codex_capture_bound");
        let header = |pad: usize| {
            codex_session_meta(&format!(
                r#"{{"id":"{ID}","session_id":"{ID}","source":"cli","base_instructions":{{"text":"{}"}}}}"#,
                "x".repeat(pad)
            ))
        };
        let max = usize::try_from(HEADER_MAX).unwrap();
        let fill = max - header(0).len();

        let at_bound = header(fill);
        assert_eq!(at_bound.len(), max);
        install_codex_rollout(&home, ID, at_bound.clone() + &at_bound);
        assert_eq!(resolve(&home, ID).as_deref(), Some(ID));

        let over = header(fill + 1);
        assert_eq!(over.len(), max + 1);
        install_codex_rollout(&home, ID, over);
        assert_eq!(resolve(&home, ID), None);
    }

    /// Nothing past the first line is examined: a header alone resolves, and
    /// a body that is neither JSON nor UTF-8 changes nothing.
    #[test]
    fn parse_capture_reads_nothing_after_the_header() {
        let home = temp("codex_capture_body");
        let header = codex_session_meta(&meta(ID, ID, r#""cli""#));
        install_codex_rollout(&home, ID, &header);
        assert_eq!(resolve(&home, ID).as_deref(), Some(ID));

        let mut with_body = header.into_bytes();
        with_body.extend_from_slice(b"\xff\xfe not json\n{\"type\":\"session_meta\"");
        install_codex_rollout(&home, ID, with_body);
        assert_eq!(resolve(&home, ID).as_deref(), Some(ID));
    }

    /// A rollout that cannot be read as a file, and a first line that is not
    /// UTF-8, are refused. The invalid byte sits inside a string the gate
    /// does not read, so only strict decoding refuses it.
    #[test]
    fn parse_capture_refuses_an_unreadable_header() {
        let home = temp("codex_capture_unreadable");
        let path = install_codex_root(&home, ID);
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert_eq!(resolve(&home, ID), None, "a directory");

        fs::remove_dir(&path).unwrap();
        let header = codex_session_meta(&format!(
            r#"{{"id":"{ID}","session_id":"{ID}","source":"cli","cwd":"/w"}}"#
        ));
        install_codex_rollout(&home, ID, &header);
        assert_eq!(resolve(&home, ID).as_deref(), Some(ID));
        let mut bytes = header.into_bytes();
        let cwd = bytes.windows(2).position(|w| w == b"/w").unwrap();
        bytes.insert(cwd, 0xff);
        install_codex_rollout(&home, ID, bytes);
        assert_eq!(resolve(&home, ID), None, "invalid UTF-8");
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
    }
}
