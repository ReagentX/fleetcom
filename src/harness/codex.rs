//! A Codex ID cannot be selected at launch. Explicitly enable embedded mode
//! with one override on every launch, and when the configured notifier can
//! be chained and this binary is usable, inject a `notify` override as well.
//!
//! Notifications are emitted for every thread in one Codex process: the
//! conversation on screen, each spawned sub-agent, and the hidden thread used
//! to title a new session. Only the first is the task's conversation. Resuming
//! a sub-agent with an unloaded parent or the unsaved title thread exits 1.
//! The injected script hands each notification to `fleetcom --codex-notify-v1`
//! ([`record_arrival`]), which resolves the notified thread to its session
//! tree's root through the rollout header and replaces the capture file with
//! the bare root UUID. A thread that cannot be classified writes nothing, so
//! the title thread, whose notification lands before or after the root's,
//! never erases an accepted root. `parse_capture` then accepts the slot only
//! as that one UUID.

use std::{
    fmt::Write as _,
    fs, io,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
};

use super::{
    BINARY_ENV, CAPTURE_ENV, CapturePaths, Harness, NOTIFY_CHAIN_ENV, SpawnPlan, capture_id,
    home_root, is_uuid, resolve_home,
};

/// Config override for explicitly launching in embedded mode.
/// Since codex 0.157.0, running plain `codex` attaches to a shared background
/// server. With a `-c` override outside a short allowlist, embedded mode is
/// used instead. `notify` is not on that list, so with only the notify override,
/// "Running without the shared background server" is displayed at startup.
/// The warning is displayed only with this feature enabled; its key is on
/// the allowlist. Use embedded mode to keep the conversation in the task's
/// process, under `fleetcom` supervision.
///
/// With `--no-daemon`, the same mode is selected, but versions predating the
/// flag fail to start. With an unknown `features.*` key, only an
/// unrecognized-setting warning is displayed.
const EMBEDDED_OVERRIDE: &str = "features.daemon_auto_start=false";

/// Upper bound on a rollout's first line, terminator included. The session's
/// base instructions are included in this line: about 22 KB in rollouts from
/// codex 0.135.0 through 0.160.0. Use 1 MiB to allow over 45 times that size
/// while bounding the read even when no newline is present.
const HEADER_MAX: u64 = 1024 * 1024;

pub struct Codex;

impl Harness for Codex {
    fn resolve_home(&self, env: &dyn Fn(&str) -> Option<PathBuf>) -> Option<PathBuf> {
        resolve_home(env, "CODEX_HOME", ".codex")
    }

    fn shape(&self) -> (&'static str, &'static str) {
        ("codex", "resume")
    }

    /// Every launch carries the embedded override: without it, `codex`
    /// attaches to its shared background server and killing the task kills
    /// only the TUI client. The notify override and its environment ride
    /// along only when the configured route can be chained and this binary
    /// is usable; otherwise the plan explains why capture is off. No session
    /// flag: codex cannot pin an ID at launch.
    fn overlay(&self, capture: &CapturePaths, home: Option<&Path>) -> SpawnPlan {
        let mut plan = SpawnPlan {
            args: vec!["-c".into(), EMBEDDED_OVERRIDE.into()],
            ..SpawnPlan::default()
        };
        let chain = match config_notify_route(home) {
            // Set an explicit empty value to exclude an inherited chain from the
            // injected script.
            NotifyRoute::Vacant => String::new(),
            // Chain the configured notifier after the validation step: exec this
            // argv with the payload appended from the injected script.
            NotifyRoute::Chain(argv) => argv.join("\n"),
            // Skip injection when the configured route cannot be encoded.
            NotifyRoute::Opaque => {
                plan.notice = Some(format!("{CAPTURE_OFF}: `notify` config can't be chained"));
                return plan;
            }
        };
        // The script runs this binary to validate each notification. Without
        // a usable path there is nothing to run, so inject nothing: an
        // unvalidated slot would be worse than none.
        let Some(binary) = &capture.fleetcom_binary else {
            plan.notice = Some(format!(
                "{CAPTURE_OFF}: fleetcom binary replaced; restart the daemon"
            ));
            return plan;
        };
        let toml = format!(
            "notify=[\"{}\"]",
            toml_escape(&capture.codex_notify.to_string_lossy())
        );
        plan.args = vec![
            "-c".into(),
            toml.into(),
            "-c".into(),
            EMBEDDED_OVERRIDE.into(),
        ];
        plan.env = vec![
            (
                CAPTURE_ENV.into(),
                capture.capture_file.clone().into_os_string(),
            ),
            (NOTIFY_CHAIN_ENV.into(), chain.into()),
            (BINARY_ENV.into(), binary.clone().into_os_string()),
        ];
        plan
    }

    /// Accept the slot only as exactly one bare root UUID: the v1 format
    /// written by [`record_arrival`]. No trimming: the writer emits no
    /// newline, so trailing whitespace marks a different writer. Validation
    /// happened at arrival, when the rollout header was on disk; the payload
    /// is not reparsed and no rollout is read here, so a root stays accepted
    /// after codex compresses its rollout. An old-format JSON slot fails
    /// [`is_uuid`] and is refused.
    fn parse_capture(
        &self,
        payload: &str,
        // Notifications for every thread originate in the task's own process.
        _pid: Option<u32>,
        // The home was consumed at arrival.
        _home: Option<&Path>,
    ) -> Option<String> {
        is_uuid(payload).then(|| payload.to_string())
    }
}

/// Status-line prefix for a launch that carries the embedded override alone.
const CAPTURE_OFF: &str = "codex capture unavailable";

/// The `--codex-notify-v1` mode: validate one notification at arrival and
/// replace the capture file with its root UUID. `env` is the process's own
/// environment, inherited from the `codex` launch: [`CAPTURE_ENV`] names the
/// slot, and the home resolves as in [`Codex::resolve_home`].
///
/// Return the root written, or `None` when nothing was written: no capture
/// path, a payload that is not an `agent-turn-complete` for a strict thread
/// ID, a thread [`root_thread`] cannot classify, or a failed write. The slot
/// only ever moves from one accepted root to another.
pub fn record_arrival(payload: &str, env: &dyn Fn(&str) -> Option<PathBuf>) -> Option<String> {
    let capture = env(CAPTURE_ENV).filter(|p| !p.as_os_str().is_empty())?;
    let root = arrival_root(payload, Codex.resolve_home(env).as_deref())?;
    replace_slot(&capture, &root).ok()?;
    Some(root)
}

/// Resolve an `agent-turn-complete` notification to the root thread of the
/// notified thread's session tree: the conversation the task's TUI is on.
/// For a sub-agent, that is its root. Return `None` for any other payload
/// type, a non-strict thread ID, or a thread [`root_thread`] cannot classify.
///
/// Look up only the thread ID from the task's own notification and classify
/// it from its rollout header. Do not infer conversation ownership from
/// other sessions in the store.
fn arrival_root(payload: &str, home: Option<&Path>) -> Option<String> {
    let v = jzon::parse(payload).ok()?;
    if v["type"].as_str() != Some("agent-turn-complete") {
        return None;
    }
    let thread = capture_id(&v, "thread-id")?;
    root_thread(&home_root(home, ".codex")?, &thread)
}

/// Write `root` to `<capture>.<pid>.tmp` beside the capture file and rename
/// it over the destination, so a reader never sees a partial slot. Remove the
/// temporary file when either step fails.
fn replace_slot(capture: &Path, root: &str) -> io::Result<()> {
    let mut tmp = capture.as_os_str().to_owned();
    tmp.push(format!(".{}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let result = fs::write(&tmp, root).and_then(|()| fs::rename(&tmp, capture));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Resolve `thread` to the root thread of its session tree from the header of
/// its rollout under the Codex `home`. `thread` must already satisfy
/// [`is_uuid`](super::is_uuid): it is matched against file names.
///
/// Each thread is saved as
/// `sessions/<YYYY>/<MM>/<DD>/rollout-<local time>-<thread>.jsonl`. The first
/// line is `{"type":"session_meta","payload":{…}}`: `payload.id` is the thread
/// and `payload.session_id` is the root thread of its tree. For a root thread,
/// both IDs are its own, with no `subagent` member in `source`. For a
/// sub-agent, `session_id` is the root's ID, with a `subagent` member in the
/// `source` object.
///
/// Return `None` unless exactly one rollout is named for `thread` and its
/// header is in one of those two formats. No rollout is saved for the title
/// thread. Examine only directory listings and the first line.
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
    // Require exactly one condition. In codex 0.140.0 and 0.141.0, a
    // sub-agent's own ID was stored as `session_id`, so its root is unknown.
    ((root == thread) != meta["source"].has_key("subagent")).then_some(root)
}

/// Push every entry with a name ending in `suffix`, exactly `depth` directory
/// levels below `dir`. Skip non-directory entries above that level. Return
/// `None` when a directory cannot be listed: a second match may be inside.
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
/// no newline is present in the first [`HEADER_MAX`] bytes: the file is empty,
/// the write is incomplete, or the line is too long. Also return `None`
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
    /// One representable route, executed after the validation step.
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
    use std::{ffi::OsString, path::PathBuf};

    use super::*;
    use crate::{
        harness::{
            Intent,
            fixtures::{OTHER, argv, assert_all_opaque, paths},
            plan, shell_words,
        },
        testutil::{Scratch, codex_session_meta, install_codex_rollout, install_codex_root, temp},
    };

    /// Codex's own launch and resume commands carry v7 IDs; the shared v4
    /// fixture stays valid for detection, which is version-agnostic.
    const ID: &str = "019f5453-de22-7240-b2e5-0d32692aa6d9";
    /// A sub-agent thread and a second-level sub-agent thread in the session
    /// rooted at [`ID`].
    const CHILD: &str = "019f5454-0c11-7b33-9a4e-5f0e6d7c8b9a";
    const GRANDCHILD: &str = "019f5454-3d70-7e02-b1c8-2a4b6c8d0e1f";
    /// Hidden title thread ID, reported through notify but never saved in a rollout.
    const TITLE: &str = "019f5453-de9f-7e61-8c0d-1a2b3c4d5e6f";

    /// Suffix of every fully instrumented launch that uses [`paths`].
    const SUFFIX: &str = concat!(
        r#" -c 'notify=["/tmp/Application Support/notify.sh"]'"#,
        " -c 'features.daemon_auto_start=false'",
    );
    /// Suffix of a launch whose capture is off: the embedded override alone.
    const EMBEDDED_ONLY: &str = " -c 'features.daemon_auto_start=false'";

    /// Environment of a fully instrumented launch that uses [`paths`] with
    /// `chain` as the encoded notifier argv.
    fn full_env(chain: &str) -> Vec<(OsString, OsString)> {
        vec![
            (
                CAPTURE_ENV.into(),
                PathBuf::from("/tmp/cap/session.json").into_os_string(),
            ),
            (NOTIFY_CHAIN_ENV.into(), chain.into()),
            (
                BINARY_ENV.into(),
                PathBuf::from("/tmp/Application Support/fleetcom").into_os_string(),
            ),
        ]
    }

    /// The notify override that [`paths`] produces.
    const NOTIFY: &str = r#"notify=["/tmp/Application Support/notify.sh"]"#;

    /// The overlay of a launch whose capture is off, explained by `why`.
    fn embedded_only(why: &str) -> SpawnPlan {
        SpawnPlan {
            args: argv(&["-c", EMBEDDED_OVERRIDE]),
            env: Vec::new(),
            resume_id: None,
            notice: Some(format!("codex capture unavailable: {why}")),
        }
    }

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

    /// Managed argv: a fresh launch carries the two overrides alone (no ID
    /// can be pinned, so the minted one is ignored); a resume leads with the
    /// `resume` subcommand and the ID, then the same overrides. The
    /// environment names the capture file, an explicitly empty chain so no
    /// inherited value reaches the script, and the binary the script runs.
    #[test]
    fn managed_argv_installs_the_notify_and_embedded_overrides() {
        let home = no_config_home();
        let fresh = plan(&Codex, &Intent::Fresh, Some(ID), &paths(), Some(&home));
        assert_eq!(fresh.args, argv(&["-c", NOTIFY, "-c", EMBEDDED_OVERRIDE]));
        assert_eq!(fresh.resume_id, None, "the minted id is ignored");
        assert_eq!(fresh.notice, None);
        assert_eq!(fresh.env, full_env(""));

        let resume = plan(
            &Codex,
            &Intent::Resume(ID.into()),
            None,
            &paths(),
            Some(&home),
        );
        assert_eq!(
            resume.args,
            argv(&["resume", ID, "-c", NOTIFY, "-c", EMBEDDED_OVERRIDE])
        );
        assert_eq!(resume.resume_id.as_deref(), Some(ID));
        assert_eq!(resume.env, full_env(""));
    }

    /// Both literal shapes get the same suffix: the notifier, then explicit
    /// embedded mode.
    #[test]
    fn literal_suffix_installs_the_notify_and_embedded_overrides() {
        let overlay = Codex.overlay(&paths(), Some(&no_config_home()));
        assert_eq!(shell_words(&overlay.args), SUFFIX);
        assert_eq!(overlay.resume_id, None);
        // The bare word's fresh intent part is empty, so its suffix is the
        // overlay's.
        let fresh = plan(
            &Codex,
            &Intent::Fresh,
            Some(ID),
            &paths(),
            Some(&no_config_home()),
        );
        assert_eq!(shell_words(&fresh.args), SUFFIX);
    }

    /// Without a usable binary, the script would have nothing to run, so the
    /// launch carries the embedded override alone and says why. The route
    /// is still read first: an opaque route reports its own reason. A
    /// managed resume keeps its intent part ahead of the reduced overlay.
    #[test]
    fn overlay_without_a_usable_binary_keeps_only_the_embedded_override() {
        let paths = CapturePaths {
            fleetcom_binary: None,
            ..paths()
        };
        let why = "fleetcom binary replaced; restart the daemon";
        assert_eq!(
            Codex.overlay(&paths, Some(&no_config_home())),
            embedded_only(why)
        );
        assert_eq!(
            shell_words(&Codex.overlay(&paths, Some(&no_config_home())).args),
            EMBEDDED_ONLY
        );
        let resume = plan(
            &Codex,
            &Intent::Resume(ID.into()),
            None,
            &paths,
            Some(&no_config_home()),
        );
        assert_eq!(resume.args, argv(&["resume", ID, "-c", EMBEDDED_OVERRIDE]));
        assert_eq!(
            resume.notice.as_deref(),
            Some(&*format!("{CAPTURE_OFF}: {why}"))
        );

        let home = temp("codex_no_binary_opaque");
        fs::write(home.join("config.toml"), "notify = [1]\n").unwrap();
        assert_eq!(
            Codex.overlay(&paths, Some(&home)),
            embedded_only("`notify` config can't be chained")
        );
    }

    /// Pass a representable `notify` assignment through [`NOTIFY_CHAIN_ENV`]. Run
    /// capture alone for comments, longer keys, and missing files: no route is
    /// configured.
    #[test]
    fn overlay_chains_a_config_toml_notify() {
        let home = temp("codex_cfg_notify");
        let chained = |plan: &SpawnPlan| {
            plan.env
                .iter()
                .find(|(k, _)| k == NOTIFY_CHAIN_ENV)
                .map(|(_, v)| v.clone())
        };

        // Missing config file: plain injection, and the chain is present but
        // empty.
        let plan = Codex.overlay(&paths(), Some(&home));
        assert_eq!(shell_words(&plan.args), SUFFIX);
        assert_eq!(chained(&plan), Some("".into()));

        let cfg = home.join("config.toml");
        for active in [
            "notify = [\"/my/thing\"]\n",
            "notify=[\"/my/thing\"]\n",
            "\tnotify\t= [\"/my/thing\"] # mine\n",
            "model = \"gpt-5\"\nnotify = [\"/my/thing\"]\n",
        ] {
            fs::write(&cfg, active).unwrap();
            let plan = Codex.overlay(&paths(), Some(&home));
            assert_eq!(shell_words(&plan.args), SUFFIX, "{active:?}");
            assert_eq!(plan.env, full_env("/my/thing"), "{active:?}");
            assert_eq!(plan.notice, None, "{active:?}");
        }
        for inert in [
            "# notify = [\"/my/thing\"]\n",
            "  # notify = [\"/my/thing\"]\n",
            "notify_extra = 1\n",
            "notify = []\n",
        ] {
            fs::write(&cfg, inert).unwrap();
            let plan = Codex.overlay(&paths(), Some(&home));
            assert_eq!(shell_words(&plan.args), SUFFIX, "{inert:?}");
            assert_eq!(chained(&plan), Some("".into()), "{inert:?}");
        }
    }

    /// Preserve spaces within argv elements in the newline-joined chain.
    #[test]
    fn overlay_chains_the_vendor_desktop_entry() {
        let home = temp("codex_vendor_notify");
        fs::write(
            home.join("config.toml"),
            "notify = [\"/Applications/Codex Computer Use.app/Contents/MacOS/SkyComputerUseClient\", \"turn-ended\"]\n",
        )
        .unwrap();
        let plan = Codex.overlay(&paths(), Some(&home));
        assert_eq!(shell_words(&plan.args), SUFFIX);
        assert!(plan.env.contains(&(
            NOTIFY_CHAIN_ENV.into(),
            "/Applications/Codex Computer Use.app/Contents/MacOS/SkyComputerUseClient\nturn-ended"
                .into()
        )));
    }

    /// For an unrepresentable route, inject the embedded override alone, no
    /// environment, and a notice: the task stays in embedded mode either way.
    #[test]
    fn overlay_skips_an_unrepresentable_config_notify() {
        let home = temp("codex_opaque_notify");
        let cfg = home.join("config.toml");
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
                Codex.overlay(&paths(), Some(&home)),
                embedded_only("`notify` config can't be chained"),
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
        let plan = Codex.overlay(&paths, Some(&no_config_home()));
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("printf '%s\\n'{}", shell_words(&plan.args)))
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
        arrival_root(&turn_complete(thread), Some(home))
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

    /// `source` of a thread started through the model's `spawn_agent` tool.
    fn thread_spawn(parent: &str, depth: u8) -> String {
        format!(
            r#"{{"subagent":{{"thread_spawn":{{"parent_thread_id":"{parent}","depth":{depth},"agent_path":"/root/pong","agent_nickname":"Pong"}}}}}}"#
        )
    }

    /// Validate the payload before looking up its rollout. Even with a root
    /// rollout present, accept only a turn-complete payload with a strict ID.
    #[test]
    fn arrival_root_accepts_only_turn_complete_payloads() {
        let home = temp("codex_capture_payload");
        install_codex_root(&home, ID);
        let parse = |payload: &str| arrival_root(payload, Some(&home));
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

    /// The slot is exactly one bare UUID. Refuse the pre-v1 JSON payload, a
    /// trailing newline (a different writer), and malformed or empty input,
    /// whatever the PID and home: nothing is reparsed or looked up at read.
    #[test]
    fn parse_capture_accepts_only_a_bare_uuid_slot() {
        let home = temp("codex_capture_slot");
        install_codex_root(&home, ID);
        for (pid, home) in [(None, None), (Some(4242), Some(&*home))] {
            let parse = |slot: &str| Codex.parse_capture(slot, pid, home);
            assert_eq!(parse(ID).as_deref(), Some(ID));
            assert_eq!(parse(&turn_complete(ID)), None, "old JSON format");
            assert_eq!(parse(&format!("{ID}\n")), None, "trailing newline");
            assert_eq!(parse(&format!(" {ID}")), None, "leading space");
            assert_eq!(parse(&ID.to_uppercase()), None, "uppercase");
            assert_eq!(parse(&ID[..35]), None, "truncated");
            assert_eq!(parse("not a uuid"), None);
            assert_eq!(parse(""), None);
        }
    }

    /// Environment of a `--codex-notify-v1` run: the capture slot, the
    /// scratch `CODEX_HOME`, and `HOME` for the fallback case.
    fn arrival_env<'a>(
        capture: Option<&'a Path>,
        codex_home: Option<&'a Path>,
        home: Option<&'a Path>,
    ) -> impl Fn(&str) -> Option<PathBuf> + 'a {
        move |key| {
            match key {
                "FLEETCOM_CAPTURE_FILE" => capture,
                "CODEX_HOME" => codex_home,
                "HOME" => home,
                _ => None,
            }
            .map(Path::to_path_buf)
        }
    }

    /// Names in the directory holding `cap`, sorted.
    fn siblings(cap: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(cap.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// The headline regression: the title thread's notification, which has
    /// no rollout, lands before the root's in some sessions and after it in
    /// others. Either way the slot ends up holding the root, because a
    /// refusal writes nothing. No temporary file is left behind.
    #[test]
    fn record_arrival_keeps_the_root_across_a_title_thread_notification() {
        let home = temp("codex_arrival_title");
        install_codex_root(&home, ID);
        let slot = temp("codex_arrival_title_slot");
        let cap = slot.join("task-1-0.json");
        let env = arrival_env(Some(&cap), Some(&home), None);

        // Title first: nothing is written at all.
        assert_eq!(record_arrival(&turn_complete(TITLE), &env), None);
        assert!(!cap.exists(), "a refused thread must not create the slot");
        assert_eq!(siblings(&cap), Vec::<String>::new());

        assert_eq!(
            record_arrival(&turn_complete(ID), &env).as_deref(),
            Some(ID)
        );
        assert_eq!(fs::read_to_string(&cap).unwrap(), ID);

        // Title after the root: the slot keeps the root.
        assert_eq!(record_arrival(&turn_complete(TITLE), &env), None);
        assert_eq!(fs::read_to_string(&cap).unwrap(), ID);
        assert_eq!(siblings(&cap), ["task-1-0.json"]);
        assert_eq!(
            Codex.parse_capture(&fs::read_to_string(&cap).unwrap(), None, Some(&home)),
            Some(ID.to_string())
        );
    }

    /// A sub-agent's notification writes its root, not its own ID, replacing
    /// an earlier, longer slot in full.
    #[test]
    fn record_arrival_writes_a_sub_agents_root() {
        let home = temp("codex_arrival_child");
        install_codex_rollout(
            &home,
            CHILD,
            codex_session_meta(&child_meta(CHILD, ID, ID, &thread_spawn(ID, 1))),
        );
        let slot = temp("codex_arrival_child_slot");
        let cap = slot.join("task-2-0.json");
        fs::write(&cap, "9".repeat(4096)).unwrap();
        let env = arrival_env(Some(&cap), Some(&home), None);
        assert_eq!(
            record_arrival(&turn_complete(CHILD), &env).as_deref(),
            Some(ID)
        );
        assert_eq!(fs::read_to_string(&cap).unwrap(), ID);
        assert_eq!(siblings(&cap), ["task-2-0.json"]);
    }

    /// Refuse before touching the slot: a non-turn-complete type, a non-UUID
    /// thread, malformed JSON, and a thread without a rollout all leave an
    /// existing slot as it was.
    #[test]
    fn record_arrival_refuses_without_writing() {
        let home = temp("codex_arrival_refuse");
        install_codex_root(&home, ID);
        let slot = temp("codex_arrival_refuse_slot");
        let cap = slot.join("task-3-0.json");
        let env = arrival_env(Some(&cap), Some(&home), None);
        for payload in [
            format!(r#"{{"type":"agent-turn-start","thread-id":"{ID}"}}"#),
            format!(
                r#"{{"type":"agent-turn-complete","thread-id":"{}"}}"#,
                ID.to_uppercase()
            ),
            r#"{"type":"agent-turn-complete","thread-id":"../../x"}"#.to_string(),
            r#"{"type":"agent-turn-complete"}"#.to_string(),
            "not json".to_string(),
            String::new(),
            turn_complete(TITLE),
        ] {
            assert_eq!(record_arrival(&payload, &env), None, "{payload:?}");
            assert!(!cap.exists(), "{payload:?} must write nothing");
        }
        fs::write(&cap, OTHER).unwrap();
        for payload in [turn_complete(TITLE), "not json".to_string()] {
            assert_eq!(record_arrival(&payload, &env), None, "{payload:?}");
            assert_eq!(fs::read_to_string(&cap).unwrap(), OTHER, "{payload:?}");
        }
        assert_eq!(siblings(&cap), ["task-3-0.json"]);
    }

    /// Without a capture path, the mode is a no-op even for an accepted root.
    /// An empty value reads as unset, as in the script.
    #[test]
    fn record_arrival_is_a_no_op_without_a_capture_path() {
        let home = temp("codex_arrival_nocap");
        install_codex_root(&home, ID);
        for capture in [None, Some(Path::new(""))] {
            let env = arrival_env(capture, Some(&home), None);
            assert_eq!(
                record_arrival(&turn_complete(ID), &env),
                None,
                "{capture:?}"
            );
        }
        // Nothing lands beside the store, where an empty path could resolve.
        assert_eq!(siblings(&home.join("sessions")), ["sessions"]);
    }

    /// Resolve the home as `Codex::resolve_home` does: `CODEX_HOME` first,
    /// then `$HOME/.codex`. A rollout under the wrong home is not found.
    #[test]
    fn record_arrival_resolves_the_home_from_its_environment() {
        let user_home = temp("codex_arrival_home");
        install_codex_root(&user_home.join(".codex"), ID);
        let other = temp("codex_arrival_other_home");
        let slot = temp("codex_arrival_home_slot");
        let cap = slot.join("task-4-0.json");

        let env = arrival_env(Some(&cap), Some(&other), Some(&user_home));
        assert_eq!(
            record_arrival(&turn_complete(ID), &env),
            None,
            "CODEX_HOME wins"
        );
        assert!(!cap.exists());

        let env = arrival_env(Some(&cap), None, Some(&user_home));
        assert_eq!(
            record_arrival(&turn_complete(ID), &env).as_deref(),
            Some(ID)
        );
        assert_eq!(fs::read_to_string(&cap).unwrap(), ID);
    }

    /// A slot whose directory is gone, as after the daemon's namespace is
    /// dropped, is a refusal with no temporary file left anywhere.
    #[test]
    fn record_arrival_reports_a_failed_write() {
        let home = temp("codex_arrival_badslot");
        install_codex_root(&home, ID);
        let cap = home.join("missing").join("task-5-0.json");
        let env = arrival_env(Some(&cap), Some(&home), None);
        assert_eq!(record_arrival(&turn_complete(ID), &env), None);
        assert!(!cap.parent().unwrap().exists());
    }

    /// For a root thread, `session_id` is its own ID under every observed
    /// string `source`.
    #[test]
    fn arrival_root_accepts_a_root_thread() {
        for source in ["cli", "vscode", "exec"] {
            let home = temp("codex_capture_root");
            install_codex_rollout(
                &home,
                ID,
                codex_session_meta(&meta(ID, ID, &format!("\"{source}\""))),
            );
            assert_eq!(resolve(&home, ID).as_deref(), Some(ID), "{source}");
        }
    }

    /// Resolve a spawned sub-agent to `session_id`, not its parent: for the
    /// second-level thread, the parent is itself a sub-agent. Omit the root's
    /// rollout to verify that no lookup is needed for it.
    #[test]
    fn arrival_root_maps_a_spawned_sub_agent_to_its_root() {
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

    /// No parent is specified in a guardian sub-agent's `source`; classify
    /// it by the `subagent` member alone.
    #[test]
    fn arrival_root_maps_a_guardian_sub_agent_to_its_root() {
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

    /// In codex 0.140.0 and 0.141.0, a sub-agent's own ID was stored as
    /// `session_id`. Reject `id == session_id` with a `subagent` source:
    /// the root is unknown, and the sub-agent's ID is not resumable.
    #[test]
    fn arrival_root_refuses_a_sub_agent_that_names_itself_as_root() {
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

    /// No rollout is saved for the title thread. Reject its notification
    /// with an absent store, an empty store, or rollouts for other threads.
    #[test]
    fn arrival_root_refuses_a_thread_without_a_rollout() {
        let home = temp("codex_capture_title");
        assert_eq!(resolve(&home, TITLE), None, "no sessions directory");
        fs::create_dir_all(home.join("sessions/2026/10/04")).unwrap();
        assert_eq!(resolve(&home, TITLE), None, "empty day directory");
        install_codex_root(&home, ID);
        assert_eq!(resolve(&home, TITLE), None, "another thread's rollout");
        assert_eq!(resolve(&home, ID).as_deref(), Some(ID));
    }

    /// Look for rollouts exactly three directories below `sessions`.
    /// Ignore matching names at other depths and files among the directories.
    #[test]
    fn arrival_root_finds_rollouts_only_in_day_directories() {
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

    /// Reject duplicate rollouts for one thread, within or across day
    /// directories. Accept the capture after removing the extra rollout.
    #[test]
    fn arrival_root_refuses_two_rollouts_for_one_thread() {
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

    /// Reject the capture if a day directory cannot be listed: a second
    /// rollout may be present even when only one is visible.
    #[test]
    fn arrival_root_refuses_a_store_it_cannot_list() {
        use std::os::unix::fs::PermissionsExt;
        let home = temp("codex_capture_unlistable");
        install_codex_root(&home, ID);
        let locked = home.join("sessions/2026/10/05");
        fs::create_dir_all(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let refused = resolve(&home, ID);
        // Restore access before asserting so the scratch tree can be removed.
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(refused, None);
        assert_eq!(resolve(&home, ID).as_deref(), Some(ID));
    }

    /// Require the notified thread's ID and its session tree in the header.
    /// Reject every other format as unclassified.
    #[test]
    fn arrival_root_refuses_an_unclassified_header() {
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

    /// A read during the header write can return any prefix of it. Accept
    /// only a terminated line; reject an empty file or a complete object
    /// without its newline.
    #[test]
    fn arrival_root_refuses_empty_and_torn_headers() {
        let home = temp("codex_capture_torn");
        let header = codex_session_meta(&meta(ID, ID, r#""cli""#));
        for cut in 0..header.len() {
            install_codex_rollout(&home, ID, &header[..cut]);
            assert_eq!(resolve(&home, ID), None, "{:?}", &header[..cut]);
        }
        install_codex_rollout(&home, ID, &header);
        assert_eq!(resolve(&home, ID).as_deref(), Some(ID));
    }

    /// Require a newline within the first [`HEADER_MAX`] bytes. Bound the
    /// line length, not the file size.
    #[test]
    fn arrival_root_bounds_the_header_line() {
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

    /// Accept a header alone or followed by bytes that are neither JSON nor
    /// UTF-8: nothing past the first line is examined.
    #[test]
    fn arrival_root_reads_nothing_after_the_header() {
        let home = temp("codex_capture_body");
        let header = codex_session_meta(&meta(ID, ID, r#""cli""#));
        install_codex_rollout(&home, ID, &header);
        assert_eq!(resolve(&home, ID).as_deref(), Some(ID));

        let mut with_body = header.into_bytes();
        with_body.extend_from_slice(b"\xff\xfe not json\n{\"type\":\"session_meta\"");
        install_codex_rollout(&home, ID, with_body);
        assert_eq!(resolve(&home, ID).as_deref(), Some(ID));
    }

    /// Reject unreadable rollouts and non-UTF-8 first lines. Place the invalid
    /// byte in a field unused for classification to test strict decoding.
    #[test]
    fn arrival_root_refuses_an_unreadable_header() {
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
