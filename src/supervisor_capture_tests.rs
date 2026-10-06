use super::*;
use crate::{
    harness::fixtures::{ID as CAP_ID, OTHER as CAP_OTHER},
    protocol::{Lifecycle, Preview, PreviewSource},
    testutil::{
        codex_session_meta, dead_pid, install_codex_rollout, install_codex_root,
        install_resident_shell,
    },
};

// --- session-capture wiring -------------------------------------------

/// Install an executable stub to record `FLEETCOM_CAPTURE_FILE`, `FLEETCOM_BINARY`, and
/// argv, one token per line, then exit.
fn install_stub(bin: &Path, name: &str, out: &Path) {
    install_script(
        bin,
        name,
        // Write each record to a per-process file beside its destination and rename it into
        // place. Redirection truncates before writing, and `printf` may write arguments
        // separately; polling for a non-empty file could otherwise accept incomplete argv.
        &format!(
            "printf '%s' \"$FLEETCOM_CAPTURE_FILE\" > '{out}/capenv.'$$'.tmp' && mv '{out}/capenv.'$$'.tmp' '{out}/capenv'\n\
             printf '%s' \"$FLEETCOM_BINARY\" > '{out}/binenv.'$$'.tmp' && mv '{out}/binenv.'$$'.tmp' '{out}/binenv'\n\
             printf '%s\\n' \"$@\" > '{out}/argv.'$$'.tmp' && mv '{out}/argv.'$$'.tmp' '{out}/argv'",
            out = out.display()
        ),
    );
}

/// Launch context containing only the stub path, shell, and capture root.
fn agent_ctx(bin: &Path, runtime: &Path, cwd: PathBuf) -> LaunchContext {
    LaunchContext {
        env: vec![
            (
                "PATH".into(),
                format!("{}:/usr/bin:/bin", bin.display()).into(),
            ),
            ("SHELL".into(), "/bin/sh".into()),
            (
                "FLEETCOM_RUNTIME_DIR".into(),
                runtime.as_os_str().to_os_string(),
            ),
        ],
        cwd,
    }
}

/// Poll until the stub's argv record contains data, then return its lines.
fn wait_argv(s: &mut Supervisor, path: &Path) -> Vec<String> {
    assert!(
        reap_until(s, Duration::from_secs(5), |_| std::fs::read_to_string(path)
            .is_ok_and(|c| !c.is_empty())),
        "the stub never recorded its argv"
    );
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

/// `agent_ctx` plus explicit environment pairs for recipe and harness
/// storage tests.
fn agent_ctx_plus(
    bin: &Path,
    runtime: &Path,
    cwd: PathBuf,
    extra: &[(&str, &Path)],
) -> LaunchContext {
    let mut ctx = agent_ctx(bin, runtime, cwd);
    for (k, v) in extra {
        ctx.env.push(((*k).into(), v.as_os_str().to_os_string()));
    }
    ctx
}

/// Install an executable stub with caller-supplied shell behavior.
fn install_script(bin: &Path, name: &str, body: &str) {
    std::fs::create_dir_all(bin).unwrap();
    write_executable(&bin.join(name), body);
}

/// Write a matching interactive registry record with raw status fields.
fn install_status_record(home: &Path, pid: u32, cwd: &Path, status: &str) {
    let sessions = home.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(
        sessions.join(format!("{pid}.json")),
        format!(
            r#"{{"pid":{pid},"sessionId":"{CAP_ID}","cwd":"{cwd}","startedAt":{started},"kind":"interactive",{status}}}"#,
            cwd = cwd.display(),
            started = now_ms()
        ),
    )
    .unwrap();
}

/// Capture-file contents from the task's own `SessionStart` hook:
/// the leader PID on the first line, then the hook's JSON and its newline.
fn stamped(task: &Task, json: &str) -> String {
    format!(
        "{}\n{json}\n",
        task.pid().expect("a spawned task has a pid")
    )
}

/// The JSON Claude's `SessionStart` hook writes for session `id` started from `source`.
fn hook_json(id: &str, source: &str) -> String {
    format!(r#"{{"session_id":"{id}","hook_event_name":"SessionStart","source":"{source}"}}"#)
}

/// Save a recipe and return its persisted JSON.
fn save_and_read(s: &mut Supervisor, config: &Path, name: &str) -> String {
    s.apply(Command::SaveSession { name: name.into() });
    let _ = s.drain();
    std::fs::read_to_string(config.join("sessions").join(format!("{name}.json"))).unwrap()
}

/// The status lines among `events`.
fn notices(events: Vec<Event>) -> Vec<String> {
    events
        .into_iter()
        .filter_map(|e| match e {
            Event::Status(m) => Some(m),
            _ => None,
        })
        .collect()
}

/// Require exactly one status among `events`, containing `needle`. Match the distinctive
/// token so message wording can change without affecting the behavior check.
fn assert_sole_notice(events: Vec<Event>, needle: &str) {
    let got = notices(events);
    assert!(
        got.len() == 1 && got[0].contains(needle),
        "expected one notice naming {needle:?}; got {got:?}"
    );
}

/// Check for a spawn acknowledgement in drained events.
fn acknowledged(events: &[Event]) -> bool {
    events.iter().any(|e| matches!(e, Event::Spawned { .. }))
}

/// Save `entries` for `dir` through the real serializer, then load the named recipe. Use
/// this path to start a managed task with a specified resume target.
fn load_recipe(
    s: &mut Supervisor,
    config: &Path,
    name: &str,
    dir: &Path,
    entries: Vec<SessionEntry>,
) {
    let cfg = SessionConfig::from([(dir.to_string_lossy().into_owned(), entries)]);
    session::save_in(&config.join("sessions"), name, &cfg).unwrap();
    s.apply(Command::LoadSession { name: name.into() });
}

/// The entries of the sole directory in the saved recipe `name`.
fn saved_entries(s: &mut Supervisor, config: &Path, name: &str) -> Vec<SessionEntry> {
    save_and_read(s, config, name);
    session::load_in(&config.join("sessions"), name)
        .unwrap()
        .into_values()
        .flatten()
        .collect()
}

/// Resume target saved under `name` for the sole managed task.
fn saved_resume(s: &mut Supervisor, config: &Path, name: &str) -> Option<String> {
    match saved_entries(s, config, name).as_slice() {
        [
            SessionEntry {
                kind: EntryKind::Managed { resume, .. },
                ..
            },
        ] => resume.clone(),
        other => panic!("expected one managed entry, got {other:?}"),
    }
}

/// Tick until the recovery snapshot lands under `config`'s session root, then load it. One
/// incarnation owns one snapshot file, so the sole file is the one to read.
fn recovered(s: &mut Supervisor, config: &Path) -> SessionConfig {
    assert!(
        wait_until(Duration::from_secs(5), || {
            s.tick();
            !recovery_files(config).is_empty()
        }),
        "the recovery snapshot never landed"
    );
    let files = recovery_files(config);
    assert_eq!(files.len(), 1);
    let stem = files[0].strip_suffix(".json").unwrap();
    session::load_recovery_in(&config.join("sessions/recovery"), stem).unwrap()
}

/// The FNV-1a discriminator is stable and separates distinct config roots.
#[test]
fn fnv_discriminator_is_stable_and_distinguishes_roots() {
    // FNV-1a 64-bit vectors for the empty input and "a".
    assert_eq!(fnv1a_hex(b""), "cbf29ce484222325");
    assert_eq!(fnv1a_hex(b"a"), "af63dc4c8601ec8c");
    assert_eq!(fnv1a_hex(b"/cfg/one"), fnv1a_hex(b"/cfg/one"));
    assert_ne!(fnv1a_hex(b"/cfg/one"), fnv1a_hex(b"/cfg/two"));
}

/// Launch managed `claude` with a pinned ID, settings overlay, and capture environment.
/// Preserve the program word for display.
#[test]
fn spawn_claude_pins_an_id_and_layers_settings() {
    let dir = scratch("cap_claude");
    let (bin, runtime) = (dir.join("bin"), dir.join("run"));
    install_stub(&bin, "claude", &dir);
    let mut s = sup_ctx(agent_ctx(&bin, &runtime, dir.to_path_buf()));
    s.spawn_agent("claude", dir.to_path_buf(), None);

    let argv = wait_argv(&mut s, &dir.join("argv"));
    let si = argv
        .iter()
        .position(|a| a == "--session-id")
        .expect("the stub must receive --session-id");
    let id = argv[si + 1].clone();
    assert!(
        crate::harness::is_uuid(&id),
        "the pinned id must be a strict uuid: {id:?}"
    );
    let fi = argv
        .iter()
        .position(|a| a == "--settings")
        .expect("the stub must receive --settings");
    let settings = PathBuf::from(&argv[fi + 1]);
    assert!(settings.is_file(), "the settings overlay must exist");
    let parsed = jzon::parse(&std::fs::read_to_string(&settings).unwrap())
        .expect("the settings overlay must be valid JSON");
    let hook = parsed["hooks"]["SessionStart"][0]["hooks"][0]["command"]
        .as_str()
        .expect("the overlay must carry the hook command");
    assert!(
        hook.contains("FLEETCOM_CAPTURE_FILE"),
        "the hook must write to the capture env: {hook:?}"
    );

    let t = &s.tasks[0];
    let cap = t.capture_file.clone().expect("capture file set");
    // An explicit runtime directory is used without a discriminator;
    // capture files sit in this incarnation's `<pid>-<nonce>` namespace
    // directly under it.
    let ns = cap.parent().expect("capture file must sit in a namespace");
    assert_eq!(ns.parent(), Some(runtime.as_path()));
    assert!(
        ns.file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with(&format!("{}-", std::process::id())),
        "the namespace must carry this process's pid prefix: {ns:?}"
    );
    assert_eq!(
        cap.file_name().unwrap().to_str().unwrap(),
        format!("task-{}-0.json", t.id),
        "the capture file must be keyed by task and run"
    );
    assert_eq!(
        settings.parent(),
        Some(ns),
        "assets and captures must share the namespace"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("capenv")).unwrap(),
        cap.display().to_string(),
        "the capture env must name task-<id>-<run>.json under the incarnation namespace"
    );
    assert_eq!(
        t.command, "claude",
        "instrumentation must never leak into the stored command"
    );
    assert_eq!(t.resume_id.as_deref(), Some(id.as_str()));
    assert!(t.harness.is_some());
}

/// An unrecognized command spawns without capture state or assets.
#[test]
fn spawn_non_agent_command_is_not_instrumented() {
    let dir = scratch("cap_plain");
    let runtime = dir.join("run");
    let mut s = sup_ctx(agent_ctx(&dir.join("bin"), &runtime, dir.to_path_buf()));
    spawn(&mut s, "printf ok", dir.to_path_buf());
    let t = &s.tasks[0];
    assert!(t.harness.is_none());
    assert!(t.capture_file.is_none());
    assert!(t.resume_id.is_none());
    assert!(
        s.capture.is_empty(),
        "a non-agent spawn must not install capture assets"
    );
    assert!(!runtime.exists());
}

/// Run a typed `claude` literally, with no harness channel, and save the exact text. Ignore
/// both a capture file at the path for a managed task with the same ID and a matching
/// registry record.
#[test]
fn typed_agent_word_is_literal_runs_verbatim_and_saves_as_text() {
    let dir = scratch("literal_claude");
    let (bin, runtime, config) = (dir.join("bin"), dir.join("run"), dir.join("config"));
    let claude_home = dir.join("claude_home");
    // Publish the argument count and capture environment atomically from the stub.
    install_script(
        &bin,
        "claude",
        &format!(
            "printf '%s\\n' \"$#\" \"$FLEETCOM_CAPTURE_FILE\" > '{out}/argv.'$$'.tmp' \
             && mv '{out}/argv.'$$'.tmp' '{out}/argv'",
            out = dir.display()
        ),
    );
    let mut s = sup_ctx(agent_ctx_plus(
        &bin,
        &runtime,
        dir.to_path_buf(),
        &[
            ("FLEETCOM_CONFIG_DIR", &config),
            ("CLAUDE_CONFIG_DIR", &claude_home),
        ],
    ));
    spawn(&mut s, "claude", dir.to_path_buf());
    assert_eq!(
        wait_argv(&mut s, &dir.join("argv")),
        ["0", ""],
        "a literal launch adds no argument and names no capture file"
    );
    let t = &s.tasks[0];
    assert!(t.harness.is_none(), "a literal task has no harness channel");
    assert!(t.capture_file.is_none());
    assert!(t.resume_id.is_none());
    assert_eq!(t.command, "claude");
    assert!(
        t.summary_adapter.is_some(),
        "the screen-only summary adapter still applies"
    );
    assert!(
        s.capture.is_empty(),
        "no assets are installed for a literal"
    );
    assert!(!runtime.exists());
    let pid = t.pid().expect("a spawned task has a pid");

    // Install a namespace by launching a managed sibling. Write a capture for task 1 there,
    // stamped with task 1's leader PID, and create a registry record keyed to that PID.
    s.spawn_agent("claude", dir.to_path_buf(), None);
    let ns = s.tasks[1]
        .capture_file
        .as_deref()
        .and_then(Path::parent)
        .expect("a managed task has a namespaced capture file")
        .to_path_buf();
    std::fs::write(
        ns.join("task-1-0.json"),
        stamped(&s.tasks[0], &format!(r#"{{"session_id":"{CAP_ID}"}}"#)),
    )
    .unwrap();
    install_status_record(
        &claude_home,
        pid,
        &dir,
        r#""status":"waiting","waitingFor":"permission prompt""#,
    );
    assert_eq!(current_resume_id(&s.tasks[0]), None);
    let entries = saved_entries(&mut s, &config, "typed");
    assert_eq!(entries[0], SessionEntry::literal("claude"));
    assert!(matches!(
        &entries[1].kind,
        EntryKind::Managed { agent, resume: Some(_) } if agent == "claude"
    ));
    let text = std::fs::read_to_string(config.join("sessions/typed.json")).unwrap();
    assert!(
        !text.contains(CAP_ID),
        "the capture-shaped file must be unreachable: {text}"
    );
}

/// A rerun uses a new capture path, so the displaced run's payload and
/// later writes cannot affect the replacement.
#[test]
fn rerun_cannot_read_the_old_runs_stale_capture() {
    let dir = scratch("cap_stale_run");
    let (bin, runtime, config) = (dir.join("bin"), dir.join("run"), dir.join("config"));
    install_stub(&bin, "claude", &dir);
    let mut s = sup_ctx(agent_ctx_plus(
        &bin,
        &runtime,
        dir.to_path_buf(),
        &[("FLEETCOM_CONFIG_DIR", &config)],
    ));
    s.spawn_agent("claude", dir.to_path_buf(), None);
    let id = s.tasks[0].id;
    // The old run's final capture becomes the new run's launch ID.
    let old_cap = s.tasks[0].capture_file.clone().expect("capture file set");
    std::fs::write(
        &old_cap,
        stamped(&s.tasks[0], &format!(r#"{{"session_id":"{CAP_ID}"}}"#)),
    )
    .unwrap();
    assert!(reap_until(&mut s, Duration::from_secs(5), |s| s.tasks[0]
        .finished
        .is_some()));

    s.apply(Command::Restart { id });
    let new_cap = s.tasks[0].capture_file.clone().expect("capture file set");
    assert_ne!(new_cap, old_cap, "the fresh run needs its own capture file");
    assert_eq!(s.tasks[0].resume_id.as_deref(), Some(CAP_ID));
    assert!(
        !old_cap.exists(),
        "rerun must delete the displaced run's capture file"
    );

    // A late hook write can recreate the old path, but the new run cannot read it.
    // Stamp with the new leader's PID to test isolation by path alone.
    let stale = stamped(&s.tasks[0], &hook_json(CAP_OTHER, "startup"));
    std::fs::write(&old_cap, &stale).unwrap();
    assert_eq!(
        saved_resume(&mut s, &config, "stalecap").as_deref(),
        Some(CAP_ID),
        "the fresh run must save its launch target, never the old run's stale capture"
    );
}

/// Removing a task also removes its capture file.
#[test]
fn remove_deletes_the_capture_file() {
    use crate::protocol::Lifecycle;
    let dir = scratch("cap_remove");
    let (bin, runtime) = (dir.join("bin"), dir.join("run"));
    install_stub(&bin, "claude", &dir);
    let mut s = sup_ctx(agent_ctx(&bin, &runtime, dir.to_path_buf()));
    s.spawn_agent("claude", dir.to_path_buf(), None);
    let _ = wait_argv(&mut s, &dir.join("argv"));
    let id = s.tasks[0].id;
    wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);
    let cap = s.tasks[0].capture_file.clone().unwrap();
    std::fs::write(&cap, "{}").unwrap();

    s.apply(Command::Remove { id });
    assert!(!cap.exists(), "Remove must delete the task's capture file");
}

/// Reconnecting with the active root reuses installed assets and preserves
/// live capture files.
#[test]
fn reconnect_with_unchanged_root_preserves_capture_files() {
    let dir = scratch("cap_reconnect");
    let (bin, runtime) = (dir.join("bin"), dir.join("run"));
    install_stub(&bin, "claude", &dir);
    let mut s = sup_ctx(agent_ctx(&bin, &runtime, dir.to_path_buf()));
    s.spawn_agent("claude", dir.to_path_buf(), None);
    let cap = s.tasks[0].capture_file.clone().expect("capture file set");
    std::fs::write(&cap, "{}").unwrap();

    // The client reconnects with an identical env and spawns again.
    s.set_launch_context(agent_ctx(&bin, &runtime, dir.to_path_buf()));
    s.spawn_agent("claude", dir.to_path_buf(), None);
    assert_eq!(s.tasks.len(), 2);
    assert!(
        cap.exists(),
        "an unchanged root must not disturb live capture files"
    );
}

/// Returning to an installed root preserves its live capture files.
#[test]
fn returning_to_a_prior_root_preserves_its_live_captures() {
    let dir = scratch("cap_aba");
    let (bin, root_a, root_b) = (dir.join("bin"), dir.join("run-a"), dir.join("run-b"));
    install_stub(&bin, "claude", &dir);
    let mut s = sup_ctx(agent_ctx(&bin, &root_a, dir.to_path_buf()));
    s.spawn_agent("claude", dir.to_path_buf(), None);
    let cap_a = s.tasks[0].capture_file.clone().expect("capture file set");
    std::fs::write(&cap_a, "{}").unwrap();

    // The client reconnects under root B, spawns, then returns to A and
    // spawns again.
    s.set_launch_context(agent_ctx(&bin, &root_b, dir.to_path_buf()));
    s.spawn_agent("claude", dir.to_path_buf(), None);
    s.set_launch_context(agent_ctx(&bin, &root_a, dir.to_path_buf()));
    s.spawn_agent("claude", dir.to_path_buf(), None);

    assert_eq!(s.tasks.len(), 3);
    assert!(
        cap_a.exists(),
        "returning to a known root must not disturb its live captures"
    );
    let ns_b = s.tasks[1]
        .capture_file
        .as_deref()
        .and_then(|c| c.parent())
        .expect("the root-B spawn must have a namespaced capture file");
    assert!(ns_b.starts_with(&root_b), "root B owns its namespace");
    assert!(
        ns_b.join("claude-settings.json").is_file(),
        "the interleaved root must keep its own namespaced assets"
    );
}

/// Remove deletes the capture file under the root the task spawned in,
/// not under whichever root the current client presents.
#[test]
fn remove_deletes_the_capture_file_under_the_spawn_root() {
    use crate::protocol::Lifecycle;
    let dir = scratch("cap_remove_cross");
    let (bin, root_a, root_b) = (dir.join("bin"), dir.join("run-a"), dir.join("run-b"));
    install_stub(&bin, "claude", &dir);
    let mut s = sup_ctx(agent_ctx(&bin, &root_a, dir.to_path_buf()));
    s.spawn_agent("claude", dir.to_path_buf(), None);
    let _ = wait_argv(&mut s, &dir.join("argv"));
    let id = s.tasks[0].id;
    wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);
    let cap = s.tasks[0].capture_file.clone().unwrap();
    std::fs::write(&cap, "{}").unwrap();

    // Root B is installed by a newer spawn; a same-id file under it must
    // survive the A task's removal.
    s.set_launch_context(agent_ctx(&bin, &root_b, dir.to_path_buf()));
    s.spawn_agent("claude", dir.to_path_buf(), None);
    let decoy = s.tasks[1]
        .capture_file
        .as_deref()
        .and_then(|c| c.parent())
        .expect("the root-B spawn must have a namespaced capture file")
        .join(format!("task-{id}-0.json"));
    std::fs::write(&decoy, "{}").unwrap();

    s.apply(Command::Remove { id });
    assert!(
        !cap.exists(),
        "Remove must delete the task's own capture file"
    );
    assert!(
        decoy.exists(),
        "Remove must not touch the same id under another root"
    );
}

/// For a `codex` spawn, add `notify=[...]` with the executable capture script, then the
/// embedded-mode override. Set `FLEETCOM_BINARY` to this process's executable for
/// validation from the script.
#[test]
fn spawn_codex_installs_the_notify_and_embedded_overrides() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("cap_codex");
    let (bin, runtime) = (dir.join("bin"), dir.join("run"));
    install_stub(&bin, "codex", &dir);
    // Keep config lookup within this test's scratch directory.
    let mut s = sup_ctx(agent_ctx_plus(
        &bin,
        &runtime,
        dir.to_path_buf(),
        &[("CODEX_HOME", &dir.join("codex_home"))],
    ));
    s.spawn_agent("codex", dir.to_path_buf(), None);

    let argv = wait_argv(&mut s, &dir.join("argv"));
    let ci = argv
        .iter()
        .position(|a| a == "-c")
        .expect("the stub must receive -c");
    let script = argv[ci + 1]
        .strip_prefix("notify=[\"")
        .and_then(|t| t.strip_suffix("\"]"))
        .unwrap_or_else(|| panic!("malformed notify override: {:?}", argv[ci + 1]));
    assert_eq!(
        argv[ci + 2..],
        ["-c", "features.daemon_auto_start=false"],
        "the embedded-mode override must follow the notify override"
    );
    let meta = std::fs::metadata(script).expect("the notify program must exist");
    assert!(
        meta.permissions().mode() & 0o111 != 0,
        "codex execs the notify program directly; it must be executable"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("binenv")).unwrap(),
        std::env::current_exe().unwrap().to_string_lossy(),
        "the child env must name the supervisor's own executable"
    );
    let t = &s.tasks[0];
    assert_eq!(t.command, "codex");
    assert!(t.resume_id.is_none(), "codex cannot pin an id at launch");
    assert!(t.capture_file.is_some());
}

/// A `grok` spawn receives exactly the pinned ID: no settings overlay,
/// no config override, and no capture environment (grok has no
/// injectable live channel). The saved recipe resumes the pinned ID.
#[test]
fn spawn_grok_pins_an_id_and_injects_nothing_else() {
    let dir = scratch("cap_grok");
    let (bin, runtime, config) = (dir.join("bin"), dir.join("run"), dir.join("config"));
    install_stub(&bin, "grok", &dir);
    let mut s = sup_ctx(agent_ctx_plus(
        &bin,
        &runtime,
        dir.to_path_buf(),
        &[("FLEETCOM_CONFIG_DIR", &config)],
    ));
    s.spawn_agent("grok", dir.to_path_buf(), None);

    let argv = wait_argv(&mut s, &dir.join("argv"));
    assert_eq!(
        argv.len(),
        2,
        "only the pinned id may be injected: {argv:?}"
    );
    assert_eq!(argv[0], "--session-id");
    let id = argv[1].clone();
    assert!(
        crate::harness::is_uuid(&id),
        "the pinned id must be a strict uuid: {id:?}"
    );
    // The stub recorded an empty FLEETCOM_CAPTURE_FILE: no capture env.
    assert_eq!(
        std::fs::read_to_string(dir.join("capenv")).unwrap(),
        "",
        "grok has no capture channel, so the env must not name one"
    );
    let t = &s.tasks[0];
    assert_eq!(
        t.command, "grok",
        "instrumentation must never leak into the stored command"
    );
    assert_eq!(t.resume_id.as_deref(), Some(id.as_str()));

    assert_eq!(
        saved_entries(&mut s, &config, "grokpin"),
        [SessionEntry::managed("grok", Some(&id))],
        "the recipe must resume the pinned session"
    );
}

/// An `omp` spawn receives `-e <module>` and the capture environment. omp
/// cannot pin an ID at launch, so the task carries no session ID.
#[test]
fn spawn_omp_loads_the_capture_extension() {
    let dir = scratch("cap_omp");
    let (bin, runtime) = (dir.join("bin"), dir.join("run"));
    install_stub(&bin, "omp", &dir);
    let mut s = sup_ctx(agent_ctx(&bin, &runtime, dir.to_path_buf()));
    s.spawn_agent("omp", dir.to_path_buf(), None);

    let argv = wait_argv(&mut s, &dir.join("argv"));
    let ei = argv
        .iter()
        .position(|a| a == "-e")
        .expect("the stub must receive -e");
    let module = PathBuf::from(&argv[ei + 1]);
    assert!(module.is_file(), "the extension module must exist");
    let text = std::fs::read_to_string(&module).unwrap();
    assert!(
        text.contains("FLEETCOM_CAPTURE_FILE"),
        "the module must write to the capture env: {text:?}"
    );
    let t = &s.tasks[0];
    let cap = t.capture_file.clone().expect("capture file set");
    assert_eq!(
        module.parent(),
        cap.parent(),
        "assets and captures must share the namespace"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("capenv")).unwrap(),
        cap.display().to_string(),
        "the child env must name this run's capture file"
    );
    assert_eq!(
        t.command, "omp",
        "instrumentation must never leak into the stored command"
    );
    assert!(t.harness.is_some());
    assert!(t.resume_id.is_none(), "omp cannot pin an id at launch");
}

/// Treat printed hints as display content, even after exit and reader EOF. Use capture or
/// launch IDs for named saves, recovery snapshots, and reruns; without either ID, save and
/// rerun fresh.
#[test]
fn printed_resume_hints_do_not_change_saved_recovery_or_rerun_targets() {
    for (tool, hints) in [
        (
            "claude",
            format!("Resume this session with:\nclaude --resume {CAP_ID}\n"),
        ),
        (
            "codex",
            format!(
                "To continue this session, run codex resume {CAP_ID}\n\
                 To continue this session, run codex resume, then select docs ({CAP_ID})\n\
                 Session ID: {CAP_ID}\n"
            ),
        ),
        (
            "grok",
            format!("grok -r {CAP_ID}\ngrok --resume {CAP_ID}\n"),
        ),
        (
            "omp",
            format!(
                "Resume this session with omp --resume {CAP_ID}\n\
                 [Recovery]\n  Main: omp --resume {CAP_ID}\n"
            ),
        ),
    ] {
        let dir = scratch(tool);
        let (bin, runtime, config) = (dir.join("bin"), dir.join("run"), dir.join("config"));
        install_script(&bin, tool, &format!("printf '%s' '{hints}'"));
        let mut s = sup_ctx(agent_ctx_plus(
            &bin,
            &runtime,
            dir.to_path_buf(),
            &[
                ("FLEETCOM_CONFIG_DIR", &config),
                ("CLAUDE_CONFIG_DIR", &dir.join("claude-home")),
                ("CODEX_HOME", &dir.join("codex-home")),
            ],
        ));
        s.set_recovery_timing(Duration::from_millis(20), Duration::from_millis(100));
        s.spawn_agent(tool, dir.to_path_buf(), None);
        s.tasks[0].group = Some("agents".into());
        s.tasks[0].name = Some(tool.into());
        let expected_id = match tool {
            "claude" => {
                std::fs::write(
                    s.tasks[0].capture_file.as_ref().unwrap(),
                    stamped(&s.tasks[0], &format!(r#"{{"session_id":"{CAP_OTHER}"}}"#)),
                )
                .unwrap();
                Some(CAP_OTHER.to_string())
            }
            "grok" => s.tasks[0].resume_id.clone(),
            _ => None,
        };
        assert_ne!(expected_id.as_deref(), Some(CAP_ID));
        assert!(
            reap_until(&mut s, Duration::from_secs(5), |s| {
                s.tasks[0].finished.is_some() && s.tasks[0].reader_done()
            }),
            "{tool}: output never completed"
        );
        // EOF can become visible after this pass's per-task work.
        s.reap();
        assert!(
            s.tasks[0]
                .screen_lines()
                .iter()
                .any(|line| line.contains(CAP_ID)),
            "{tool}: the printed UUID must reach the terminal"
        );
        let expected = SessionConfig::from([(
            path::abbreviate(&dir),
            vec![SessionEntry {
                group: Some("agents".into()),
                name: Some(tool.into()),
                ..SessionEntry::managed(tool, expected_id.as_deref())
            }],
        )]);
        save_and_read(&mut s, &config, "hints");
        assert_eq!(
            session::load_in(&config.join("sessions"), "hints").unwrap(),
            expected,
            "{tool}: named save"
        );
        assert_eq!(recovered(&mut s, &config), expected, "{tool}: recovery");
        let id = s.tasks[0].id;
        s.apply(Command::Restart { id });
        assert_eq!(s.tasks[0].run, 1, "{tool}: rerun must replace the task");
        assert_eq!(s.tasks[0].command, tool, "{tool}: rerun keeps the word");
        assert_eq!(s.tasks[0].resume_id, expected_id, "{tool}: rerun ID");
    }
}

/// Latch a recent exit before checking rerun eligibility. Retain the explicit launch ID
/// even when another conversation is named in terminal output. Load a recipe to start the
/// managed task on a specified conversation.
#[test]
fn rerun_latches_exit_without_reap_and_preserves_the_launch_id() {
    use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};
    let dir = scratch("rerun_without_reap");
    let (bin, runtime, config) = (dir.join("bin"), dir.join("run"), dir.join("config"));
    install_script(
        &bin,
        "grok",
        &format!(
            "printf '%s\\n' \"$@\" > '{}/argv'\n\
             printf 'grok --resume {CAP_ID}\\n'",
            dir.display()
        ),
    );
    let mut s = sup_ctx(agent_ctx_plus(
        &bin,
        &runtime,
        dir.to_path_buf(),
        &[("FLEETCOM_CONFIG_DIR", &config)],
    ));
    load_recipe(
        &mut s,
        &config,
        "target",
        &dir,
        vec![SessionEntry::managed("grok", Some(CAP_OTHER))],
    );
    assert_eq!(s.tasks.len(), 1, "{:?}", notices(s.drain()));
    assert!(s.tasks[0].harness.is_some());
    assert_eq!(s.tasks[0].resume_id.as_deref(), Some(CAP_OTHER));
    let id = s.tasks[0].id;
    let pid = Pid::from_raw(s.tasks[0].pid().unwrap() as i32).unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || {
            let exited = waitid(
                WaitId::Pid(pid),
                WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
            )
            .unwrap()
            .is_some();
            exited && s.tasks[0].reader_done()
        }),
        "the stub never exited and drained its output"
    );
    assert!(
        s.tasks[0].finished.is_none(),
        "no exit latch may have run yet"
    );
    std::fs::remove_file(dir.join("argv")).unwrap();
    s.apply(Command::Restart { id });
    assert_eq!(s.tasks[0].run, 1);
    assert_eq!(s.tasks[0].resume_id.as_deref(), Some(CAP_OTHER));
    assert_eq!(
        wait_argv(&mut s, &dir.join("argv")),
        ["--resume", CAP_OTHER]
    );
}

/// Session ID precedence is capture file, live registry, then spawn-time pin.
#[test]
fn resume_id_precedence_registry_over_spawn_under_capture() {
    let dir = scratch("registry_precedence");
    let (bin, runtime, config) = (dir.join("bin"), dir.join("run"), dir.join("config"));
    let (claude_home, done) = (dir.join("claude_home"), dir.join("done"));
    install_script(
        &bin,
        "claude",
        &format!(
            "until [ -e '{d}' ]; do sleep 0.05; done",
            d = done.display()
        ),
    );
    let mut s = sup_ctx(agent_ctx_plus(
        &bin,
        &runtime,
        dir.to_path_buf(),
        &[
            ("FLEETCOM_CONFIG_DIR", &config),
            ("CLAUDE_CONFIG_DIR", &claude_home),
        ],
    ));
    s.spawn_agent("claude", dir.to_path_buf(), None);
    let injected = s.tasks[0]
        .resume_id
        .clone()
        .expect("a fresh claude launch pins an id");
    assert_ne!(injected.as_str(), CAP_ID);
    // Key the registry fixture to the spawned task.
    let pid = s.tasks[0].pid().expect("a live task has a pid");

    install_status_record(&claude_home, pid, &dir, r#""status":"idle""#);
    assert_eq!(
        saved_resume(&mut s, &config, "registry").as_deref(),
        Some(CAP_ID),
        "the registry must beat the injected id {injected}"
    );

    // A capture-file ID outranks the registry ID.
    let cap = s.tasks[0].capture_file.clone().expect("capture file set");
    std::fs::write(&cap, stamped(&s.tasks[0], &hook_json(CAP_OTHER, "clear"))).unwrap();
    assert_eq!(
        saved_resume(&mut s, &config, "capture").as_deref(),
        Some(CAP_OTHER),
        "the capture file must beat the registry"
    );
    std::fs::write(&done, b"").unwrap();
}

/// Session ID reported by a process that is not the task.
const FOREIGN_ID: &str = "22222222-3333-4444-8555-666666666666";

/// Run the installed hook from a process other than the task leader to
/// replace the capture with a foreign session. Reject the foreign parent's
/// PID stamp and use the spawn-time ID, then the registry ID once available.
#[test]
fn capture_stamped_by_a_foreign_process_falls_back_to_the_next_source() {
    let dir = scratch("cap_foreign");
    let (bin, runtime, config) = (dir.join("bin"), dir.join("run"), dir.join("config"));
    let claude_home = dir.join("claude_home");
    // Write a capture stamped with the task leader's PID. Then run the installed hook from
    // the stub through an intermediate shell, with the same inherited environment as in
    // Claude's daemon. Append `:` to prevent exec of the pipeline's last command: the
    // hook's `$PPID` must be the intermediate shell, never the leader.
    install_script(
        &bin,
        "claude",
        &format!(
            r#"until [ -e '{d}/foreign' ]; do sleep 0.05; done
sh -c 'printf "%s\n" "$1" | sh "$0"; :' '{d}/hook' '{foreign}'
: > '{d}/foreign-done'
until [ -e '{d}/done' ]; do sleep 0.05; done"#,
            d = dir.display(),
            foreign = hook_json(FOREIGN_ID, "startup"),
        ),
    );
    let mut s = sup_ctx(agent_ctx_plus(
        &bin,
        &runtime,
        dir.to_path_buf(),
        &[
            ("FLEETCOM_CONFIG_DIR", &config),
            ("CLAUDE_CONFIG_DIR", &claude_home),
        ],
    ));
    s.spawn_agent("claude", dir.to_path_buf(), None);
    let pinned = s.tasks[0]
        .resume_id
        .clone()
        .expect("a fresh claude launch pins an id");
    let pid = s.tasks[0].pid().expect("a live task has a pid");
    let cap = s.tasks[0].capture_file.clone().expect("capture file set");

    // The foreign stage runs the hook command from the installed overlay.
    let overlay = std::fs::read_to_string(cap.with_file_name("claude-settings.json")).unwrap();
    let hook = jzon::parse(&overlay).unwrap()["hooks"]["SessionStart"][0]["hooks"][0]["command"]
        .as_str()
        .expect("the overlay must carry the hook command")
        .to_string();
    std::fs::write(dir.join("hook"), hook).unwrap();

    std::fs::write(&cap, stamped(&s.tasks[0], &hook_json(CAP_OTHER, "clear"))).unwrap();
    assert_eq!(
        current_resume_id(&s.tasks[0]).as_deref(),
        Some(CAP_OTHER),
        "the task's own capture must beat the pin"
    );

    std::fs::write(dir.join("foreign"), b"").unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || dir.join("foreign-done").exists()),
        "the stub never ran the hook under the intermediate shell"
    );
    let text = std::fs::read_to_string(&cap).unwrap();
    let (stamp, rest) = text
        .split_once('\n')
        .expect("the hook must write a stamp line");
    assert_eq!(
        rest,
        format!("{}\n", hook_json(FOREIGN_ID, "startup")),
        "the foreign session must have replaced the task's capture"
    );
    assert!(
        stamp.parse::<u32>().is_ok_and(|p| p != pid),
        "the stamp must name the hook's own parent, not the leader: {stamp:?}"
    );

    // With no registry record yet, use the pinned ID after rejecting the capture.
    assert_eq!(
        current_resume_id(&s.tasks[0]).as_deref(),
        Some(pinned.as_str()),
        "a foreign capture must fall through to the spawn-time id"
    );
    install_status_record(&claude_home, pid, &dir, r#""status":"idle""#);
    assert_eq!(
        current_resume_id(&s.tasks[0]).as_deref(),
        Some(CAP_ID),
        "a foreign capture must fall through to the registry"
    );
    assert_eq!(
        saved_resume(&mut s, &config, "foreign").as_deref(),
        Some(CAP_ID),
        "the recipe must never resume the foreign session {FOREIGN_ID}"
    );
    std::fs::write(dir.join("done"), b"").unwrap();
}

/// Do not infer ownership of a nearby rollout from a shared directory. With no capture,
/// save both managed tasks without resume IDs in named sessions and recovery snapshots.
#[test]
fn silent_codex_tasks_save_and_recover_without_a_resume_id() {
    let dir = scratch("uncaptured_codex");
    let (bin, runtime, config) = (dir.join("bin"), dir.join("run"), dir.join("config"));
    let codex_home = dir.join("codex_home");
    install_script(&bin, "codex", "exit 0");

    // Use the same directory and the former 30-second window for both tasks,
    // but write the rollout before either launch. Include a complete root
    // header; it must be read only after a notification for this thread.
    let ms = now_ms();
    let id = format!(
        "{:08x}-{:04x}-7000-8000-000000000001",
        ms >> 16,
        ms & 0xffff
    );
    let (y, m, d) = crate::format::civil_from_days((ms / 86_400_000) as i64);
    let rollouts = codex_home.join(format!("sessions/{y:04}/{m:02}/{d:02}"));
    std::fs::create_dir_all(&rollouts).unwrap();
    std::fs::write(
        rollouts.join(format!("rollout-2026-07-13T09-00-00-{id}.jsonl")),
        codex_session_meta(&format!(
            r#"{{"id":"{id}","session_id":"{id}","source":"cli","cwd":"{}"}}"#,
            dir.display()
        )),
    )
    .unwrap();

    let mut s = sup_ctx(agent_ctx_plus(
        &bin,
        &runtime,
        dir.to_path_buf(),
        &[
            ("FLEETCOM_CONFIG_DIR", &config),
            ("CODEX_HOME", &codex_home),
        ],
    ));
    s.set_recovery_timing(Duration::from_millis(20), Duration::from_millis(100));
    s.spawn_agent("codex", dir.to_path_buf(), None);
    s.spawn_agent("codex", dir.to_path_buf(), None);
    assert_eq!(s.tasks.len(), 2);
    assert!(reap_until(&mut s, Duration::from_secs(5), |s| s
        .tasks
        .iter()
        .all(|t| t.finished.is_some())));
    for task in &s.tasks {
        assert!(
            current_resume_id(task).is_none(),
            "no capture channel fired"
        );
        let spawn_ms = task
            .spawned_at
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        assert!(
            spawn_ms.abs_diff(u128::from(ms)) <= 30_000,
            "the decoy must remain plausible"
        );
    }
    let expected = SessionConfig::from([(
        path::abbreviate(&dir),
        vec![
            SessionEntry::managed("codex", None),
            SessionEntry::managed("codex", None),
        ],
    )]);
    save_and_read(&mut s, &config, "silent");
    assert_eq!(
        session::load_in(&config.join("sessions"), "silent").unwrap(),
        expected
    );

    assert_eq!(recovered(&mut s, &config), expected);
}

/// Thread IDs reported through one codex process's notifier: the
/// conversation, a spawned sub-agent, and the hidden title thread.
const CODEX_ROOT: &str = "019f5453-de22-7240-b2e5-0d32692aa6d9";
const CODEX_CHILD: &str = "019f5454-0c11-7b33-9a4e-5f0e6d7c8b9a";
const CODEX_TITLE: &str = "019f5453-de9f-7e61-8c0d-1a2b3c4d5e6f";

/// Notification JSON passed to the injected notifier after a completed turn of `thread`.
/// `last` is the final assistant message, already escaped for a JSON string.
fn turn_complete(thread: &str, last: &str) -> String {
    format!(
        r#"{{"type":"agent-turn-complete","thread-id":"{thread}","turn-id":"t","cwd":"/w","input-messages":["ping"],"last-assistant-message":"{last}"}}"#
    )
}

/// The title thread's notification: its final message is the generated title.
fn title_turn() -> String {
    turn_complete(CODEX_TITLE, r#"{\"title\":\"Ping the sub-agent\"}"#)
}

/// Run `--codex-notify-v1` in-process for the sole task, using its capture file and
/// `codex_home` as the environment. Return the written root, as for a binary invocation
/// from the injected script.
fn arrive(s: &Supervisor, codex_home: &Path, payload: &str) -> Option<String> {
    let cap = s.tasks[0].capture_file.clone().expect("capture file set");
    crate::harness::record_arrival(payload, &|key| match key {
        crate::harness::CAPTURE_ENV => Some(cap.clone()),
        "CODEX_HOME" => Some(codex_home.to_path_buf()),
        _ => None,
    })
}

/// Install a stand-in at `<bin>/fleetcom` to write `CAP_ID` over the capture file. The real
/// notify mode cannot run from the unit-test executable because its `main` is the test
/// harness. Set `FLEETCOM_BINARY` to this stand-in when invoking the injected script from a
/// stub.
fn install_fake_fleetcom(bin: &Path) -> PathBuf {
    install_script(
        bin,
        "fleetcom",
        &format!("printf '%s' '{CAP_ID}' > \"$FLEETCOM_CAPTURE_FILE\""),
    );
    bin.join("fleetcom")
}

/// Report notifications from one codex process in the observed order: title thread,
/// sub-agent, root conversation, then title again to cover its later arrival in most
/// sessions. Without a title rollout, write nothing and preserve the authored command.
/// Resolve the sub-agent to its root and the conversation to itself; preserve that root
/// after the later title notification.
#[test]
fn codex_capture_resolves_each_notifying_thread_to_the_root() {
    let dir = scratch("codex_threads");
    let (bin, runtime, config) = (dir.join("bin"), dir.join("run"), dir.join("config"));
    let codex_home = dir.join("codex_home");
    install_script(&bin, "codex", "exit 0");
    install_codex_root(&codex_home, CODEX_ROOT);
    install_codex_rollout(
        &codex_home,
        CODEX_CHILD,
        codex_session_meta(&format!(
            r#"{{"id":"{CODEX_CHILD}","session_id":"{CODEX_ROOT}","parent_thread_id":"{CODEX_ROOT}","source":{{"subagent":{{"thread_spawn":{{"parent_thread_id":"{CODEX_ROOT}","depth":1,"agent_path":"/root/pong","agent_nickname":"Pong"}}}}}}}}"#
        )),
    );
    let mut s = sup_ctx(agent_ctx_plus(
        &bin,
        &runtime,
        dir.to_path_buf(),
        &[
            ("FLEETCOM_CONFIG_DIR", &config),
            ("CODEX_HOME", &codex_home),
        ],
    ));
    s.spawn_agent("codex", dir.to_path_buf(), None);
    assert!(s.tasks[0].resume_id.is_none(), "codex pins no id at launch");

    assert_eq!(arrive(&s, &codex_home, &title_turn()), None);
    assert_eq!(
        saved_resume(&mut s, &config, "title"),
        None,
        "the title thread must not become the resume target"
    );
    assert_eq!(
        arrive(&s, &codex_home, &turn_complete(CODEX_CHILD, "pong")).as_deref(),
        Some(CODEX_ROOT)
    );
    assert_eq!(
        saved_resume(&mut s, &config, "child").as_deref(),
        Some(CODEX_ROOT),
        "a sub-agent's turn must resume the conversation that spawned it"
    );
    assert_eq!(
        arrive(&s, &codex_home, &turn_complete(CODEX_ROOT, "done")).as_deref(),
        Some(CODEX_ROOT)
    );
    assert_eq!(
        saved_resume(&mut s, &config, "root").as_deref(),
        Some(CODEX_ROOT),
        "the conversation's own turn must resume it"
    );
    assert_eq!(arrive(&s, &codex_home, &title_turn()), None);
    assert_eq!(
        saved_resume(&mut s, &config, "after_title").as_deref(),
        Some(CODEX_ROOT),
        "a title notification after the root's must leave the root in place"
    );
}

/// For a codex task loaded on a chosen conversation, preserve the launch ID
/// after rejecting notifications for the title thread or a rollout outside
/// the launch-time Codex home. Prefer a captured root thread in the task's
/// own home over the launch ID.
#[test]
fn refused_codex_capture_keeps_the_launch_target() {
    /// A root thread saved under another Codex home.
    const FOREIGN: &str = "019f5460-1a2b-7c3d-8e4f-5a6b7c8d9e0f";
    let dir = scratch("codex_refused");
    let (bin, runtime, config) = (dir.join("bin"), dir.join("run"), dir.join("config"));
    let codex_home = dir.join("codex_home");
    install_script(&bin, "codex", "exit 0");
    install_codex_root(&dir.join("other_home"), FOREIGN);
    install_codex_root(&codex_home, CODEX_ROOT);
    let mut s = sup_ctx(agent_ctx_plus(
        &bin,
        &runtime,
        dir.to_path_buf(),
        &[
            ("FLEETCOM_CONFIG_DIR", &config),
            ("CODEX_HOME", &codex_home),
        ],
    ));
    load_recipe(
        &mut s,
        &config,
        "target",
        &dir,
        vec![SessionEntry::managed("codex", Some(CAP_ID))],
    );
    assert_eq!(s.tasks.len(), 1, "{:?}", notices(s.drain()));
    assert_eq!(s.tasks[0].resume_id.as_deref(), Some(CAP_ID));

    assert_eq!(arrive(&s, &codex_home, &title_turn()), None);
    assert_eq!(
        saved_resume(&mut s, &config, "title").as_deref(),
        Some(CAP_ID),
        "the title thread must not displace the launch target"
    );
    assert_eq!(arrive(&s, &codex_home, &turn_complete(FOREIGN, "hi")), None);
    assert_eq!(
        saved_resume(&mut s, &config, "foreign").as_deref(),
        Some(CAP_ID),
        "a thread outside the task's Codex home must not displace the launch target"
    );
    assert_eq!(
        arrive(&s, &codex_home, &turn_complete(CODEX_ROOT, "done")).as_deref(),
        Some(CODEX_ROOT)
    );
    assert_eq!(
        saved_resume(&mut s, &config, "root").as_deref(),
        Some(CODEX_ROOT),
        "the task's own root thread must still outrank the launch target"
    );
}

/// Home resolution order: the tool's own var, then the launch env's HOME
/// joined with the tool's dot directory, then nothing.
#[test]
fn harness_home_prefers_the_tool_var_then_home() {
    use crate::harness::{Claude, Codex, Grok, Omp};
    let env: Vec<(OsString, OsString)> = vec![
        ("HOME".into(), "/h".into()),
        ("CODEX_HOME".into(), "/x".into()),
    ];
    assert_eq!(harness_home(&env, &Codex).as_deref(), Some(Path::new("/x")));
    let env: Vec<(OsString, OsString)> = vec![("HOME".into(), "/h".into())];
    assert_eq!(
        harness_home(&env, &Codex).as_deref(),
        Some(Path::new("/h/.codex"))
    );
    assert_eq!(
        harness_home(&env, &Claude).as_deref(),
        Some(Path::new("/h/.claude"))
    );
    assert_eq!(harness_home(&env, &Grok), None);
    assert_eq!(harness_home(&env, &Omp), None);
    assert_eq!(harness_home(&[], &Codex), None);
}

/// With only `HOME` in the launch environment, read notify routing from
/// `<home>/.codex/config.toml`. For an unchainable route, include only the embedded
/// override.
#[test]
fn home_only_launch_env_targets_the_clients_dot_codex() {
    let dir = scratch("home_resolve");
    let (bin, runtime, config, home) = (
        dir.join("bin"),
        dir.join("run"),
        dir.join("config"),
        dir.join("home"),
    );
    let codex_home = home.join(".codex");
    std::fs::create_dir_all(&codex_home).unwrap();
    // An empty argument cannot survive the chain transport: injection is
    // suppressed only when the guard reads the client's config through HOME.
    std::fs::write(
        codex_home.join("config.toml"),
        "notify = [\"/my/thing\", \"\"]\n",
    )
    .unwrap();
    install_stub(&bin, "codex", &dir);
    let mut s = sup_ctx(agent_ctx_plus(
        &bin,
        &runtime,
        dir.to_path_buf(),
        &[("FLEETCOM_CONFIG_DIR", &config), ("HOME", &home)],
    ));
    s.spawn_agent("codex", dir.to_path_buf(), None);
    assert_eq!(
        s.tasks[0].harness_home.as_deref(),
        Some(codex_home.as_path()),
        "HOME alone must resolve the harness home"
    );
    let argv = wait_argv(&mut s, &dir.join("argv"));
    assert_eq!(
        argv,
        ["-c", "features.daemon_auto_start=false"],
        "the guard must read <home>/.codex/config.toml and inject only embedded mode"
    );
    assert!(reap_until(&mut s, Duration::from_secs(5), |s| s.tasks[0]
        .finished
        .is_some()));

    assert_eq!(
        saved_entries(&mut s, &config, "homeonly"),
        [SessionEntry::managed("codex", None)]
    );
}

/// With no configured notifier, instrumentation clears an inherited
/// `FLEETCOM_NOTIFY_CHAIN` so the capture script cannot execute it.
#[test]
fn stale_inherited_notify_chain_is_never_executed() {
    let dir = scratch("stale_chain");
    let (bin, runtime) = (dir.join("bin"), dir.join("run"));
    // No config.toml exists: nothing routed, so nothing may be chained.
    let codex_home = dir.join("codex_home");
    install_codex_root(&codex_home, CAP_ID);
    let stale = dir.join("stale");
    let record = dir.join("stale-record");
    write_executable(&stale, &format!("touch '{}'", record.display()));

    // Invoke the injected notifier from the stub as from codex, with the fake binary set
    // for validation.
    let payload = format!(r#"{{"type":"agent-turn-complete","thread-id":"{CAP_ID}"}}"#);
    let fake = install_fake_fleetcom(&bin);
    // The notify script sits beside the capture file, in a namespace
    // whose nonce is unknowable before spawn: derive it from the env.
    install_script(
        &bin,
        "codex",
        &format!(
            "FLEETCOM_BINARY='{}' \"${{FLEETCOM_CAPTURE_FILE%/*}}/codex-notify.sh\" '{payload}'",
            fake.display()
        ),
    );
    let mut ctx = agent_ctx_plus(
        &bin,
        &runtime,
        dir.to_path_buf(),
        &[("CODEX_HOME", &codex_home)],
    );
    ctx.env.push((
        crate::harness::NOTIFY_CHAIN_ENV.into(),
        stale.as_os_str().to_os_string(),
    ));
    let mut s = sup_ctx(ctx);
    s.spawn_agent("codex", dir.to_path_buf(), None);
    assert!(reap_until(&mut s, Duration::from_secs(5), |s| s.tasks[0]
        .finished
        .is_some()));
    let cap = s.tasks[0].capture_file.clone().expect("capture file set");
    assert_eq!(
        std::fs::read_to_string(&cap).unwrap(),
        CAP_ID,
        "the validation step must land before the script exits"
    );
    assert!(
        !record.exists(),
        "the stale inherited chain must not execute"
    );
    assert_eq!(
        current_resume_id(&s.tasks[0]).as_deref(),
        Some(CAP_ID),
        "the notifier's payload for a root thread must pass the capture gate"
    );
}

/// Without a known ID, save only the managed agent word; omit `resume`.
#[test]
fn managed_save_without_any_id_omits_resume() {
    let dir = scratch("no_id");
    let (bin, runtime, config) = (dir.join("bin"), dir.join("run"), dir.join("config"));
    // No config.toml exists, so notifier routing has nothing to read.
    let codex_home = dir.join("codex_home");
    install_stub(&bin, "codex", &dir);
    let mut s = sup_ctx(agent_ctx_plus(
        &bin,
        &runtime,
        dir.to_path_buf(),
        &[
            ("FLEETCOM_CONFIG_DIR", &config),
            ("CODEX_HOME", &codex_home),
        ],
    ));
    s.spawn_agent("codex", dir.to_path_buf(), None);
    assert!(reap_until(&mut s, Duration::from_secs(5), |s| s.tasks[0]
        .finished
        .is_some()));

    let text = save_and_read(&mut s, &config, "plainagent");
    assert!(
        text.contains("\"agent\": \"codex\""),
        "the agent word must be written; got {text}"
    );
    assert!(
        !text.contains("resume"),
        "no id exists, so no resume field may be written; got {text}"
    );
}

/// For a representable `notify` assignment, run the configured notifier after validation
/// through the injected script.
#[test]
fn config_toml_notify_chains_through_the_injected_script() {
    use crate::harness::NOTIFY_CHAIN_ENV;
    let dir = scratch("cfg_chain");
    let (bin, runtime) = (dir.join("bin"), dir.join("run"));
    let codex_home = dir.join("codex_home");
    install_codex_root(&codex_home, CAP_ID);
    // The notifier path contains spaces and carries a fixed argument.
    let notifier = dir.join("Fake App.app").join("Sky Client");
    let record = dir.join("notifier-record");
    install_fake_notifier(&notifier, &record);
    std::fs::write(
        codex_home.join("config.toml"),
        format!("notify = [\"{}\", \"turn-ended\"]\n", notifier.display()),
    )
    .unwrap();

    // Record argv and the chain environment in the stub, then invoke the notify script with
    // notification JSON as the final argument. Use the fake binary for validation.
    let payload = format!(r#"{{"type":"agent-turn-complete","thread-id":"{CAP_ID}"}}"#);
    let fake = install_fake_fleetcom(&bin);
    install_script(
        &bin,
        "codex",
        &format!(
            "printf '%s\\n' \"$@\" > '{out}/argv'\n\
                 printf '%s' \"${chain}\" > '{out}/chainenv'\n\
                 FLEETCOM_BINARY='{fake}' \"${{FLEETCOM_CAPTURE_FILE%/*}}/codex-notify.sh\" '{payload}'",
            out = dir.display(),
            chain = NOTIFY_CHAIN_ENV,
            fake = fake.display(),
        ),
    );
    let mut s = sup_ctx(agent_ctx_plus(
        &bin,
        &runtime,
        dir.to_path_buf(),
        &[("CODEX_HOME", &codex_home)],
    ));
    s.spawn_agent("codex", dir.to_path_buf(), None);
    let argv = wait_argv(&mut s, &dir.join("argv"));
    assert!(
        argv.iter().any(|a| a.starts_with("notify=[")),
        "a chained spawn must still inject the override; argv: {argv:?}"
    );
    // Redirection creates the record before `printf` writes, so wait for the
    // complete expected contents.
    let expected = format!("turn-ended\n{payload}\n");
    assert!(
        reap_until(&mut s, Duration::from_secs(5), |_| {
            std::fs::read_to_string(&record).is_ok_and(|r| r == expected)
        }),
        "the chained notifier must receive its original args plus the payload, \
         and never wrote that complete record"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("chainenv")).unwrap(),
        format!("{}\nturn-ended", notifier.display()),
        "the child env must carry the displaced argv, newline-joined"
    );
    let cap = s.tasks[0].capture_file.clone().unwrap();
    assert_eq!(
        std::fs::read_to_string(&cap).unwrap(),
        CAP_ID,
        "the validation step must precede the chain handoff"
    );
    assert_eq!(
        current_resume_id(&s.tasks[0]).as_deref(),
        Some(CAP_ID),
        "the notifier's payload for a root thread must pass the capture gate"
    );
}

/// For an unrepresentable `notify` value, disable capture injection, include only the
/// embedded override, and report the reason. With the assignment commented out, inject
/// capture again without a notice.
#[test]
fn unrepresentable_config_notify_suppresses_injection() {
    let dir = scratch("cfg_guard");
    let (bin, runtime) = (dir.join("bin"), dir.join("run"));
    let codex_home = dir.join("codex_home");
    std::fs::create_dir_all(&codex_home).unwrap();
    // An empty argument cannot survive shell field splitting.
    std::fs::write(
        codex_home.join("config.toml"),
        "notify = [\"/my/thing\", \"\"]\n",
    )
    .unwrap();
    install_stub(&bin, "codex", &dir);
    let mut s = sup_ctx(agent_ctx_plus(
        &bin,
        &runtime,
        dir.to_path_buf(),
        &[("CODEX_HOME", &codex_home)],
    ));
    s.spawn_agent("codex", dir.to_path_buf(), None);
    assert_sole_notice(s.drain(), "capture unavailable");
    let argv = wait_argv(&mut s, &dir.join("argv"));
    assert_eq!(
        argv,
        ["-c", "features.daemon_auto_start=false"],
        "fleetcom must preserve an unrepresentable notify and inject only embedded mode"
    );

    // The same route commented out is inert: the injection returns.
    std::fs::write(
        codex_home.join("config.toml"),
        "# notify = [\"/my/thing\"]\n",
    )
    .unwrap();
    std::fs::remove_file(dir.join("argv")).unwrap();
    s.spawn_agent("codex", dir.to_path_buf(), None);
    assert_eq!(notices(s.drain()), Vec::<String>::new());
    let argv = wait_argv(&mut s, &dir.join("argv"));
    assert!(
        argv.iter().any(|a| a.starts_with("notify=[")),
        "a commented notify must not suppress the injection; argv: {argv:?}"
    );
}

/// Non-agent commands remain plain string entries in persisted JSON.
#[test]
fn non_agent_entries_survive_save_as_plain_strings() {
    let dir = scratch("plain_save");
    let config = dir.join("config");
    let mut s = sup_ctx(config_ctx(
        &config,
        dir.to_path_buf(),
        &[("SHELL", "/bin/sh")],
    ));
    spawn(&mut s, "sleep 30", dir.to_path_buf());
    let text = save_and_read(&mut s, &config, "plain");
    assert!(
        text.contains("\"sleep 30\""),
        "string-form member expected; got {text}"
    );
    assert!(
        !text.contains("\"cmd\""),
        "no object form for an unadorned entry; got {text}"
    );
    let cfg = session::load_in(&config.join("sessions"), "plain").unwrap();
    assert_eq!(
        cfg[&path::abbreviate(&dir)],
        vec![SessionEntry::literal("sleep 30")]
    );
}

/// Cadence passes persist changed capture IDs without rewriting stable recipes.
#[test]
fn recovery_cadence_rewrites_on_capture_drift_and_skips_when_static() {
    let dir = scratch("cap_recovery_cadence");
    let (bin, runtime, config) = (dir.join("bin"), dir.join("run"), dir.join("config"));
    install_stub(&bin, "claude", &dir);
    let mut s = sup_ctx(agent_ctx_plus(
        &bin,
        &runtime,
        dir.to_path_buf(),
        &[("FLEETCOM_CONFIG_DIR", &config)],
    ));
    s.set_recovery_timing(Duration::from_millis(20), Duration::from_millis(100));
    s.spawn_agent("claude", dir.to_path_buf(), None);
    let _ = wait_argv(&mut s, &dir.join("argv"));

    let rec = config.join("sessions").join("recovery");
    let snapshot = |rec: &Path| -> Option<(PathBuf, String)> {
        let p = std::fs::read_dir(rec).ok()?.flatten().next()?.path();
        let text = std::fs::read_to_string(&p).ok()?;
        Some((p, text))
    };
    // The initial debounced snapshot contains only the spawn-time ID.
    assert!(
        wait_until(Duration::from_secs(5), || {
            s.tick();
            snapshot(&rec).is_some()
        }),
        "the spawn snapshot never landed"
    );
    assert!(!snapshot(&rec).unwrap().1.contains(CAP_OTHER));

    // Change the capture ID without a recipe mutation.
    let cap = s.tasks[0].capture_file.clone().expect("capture file set");
    std::fs::write(&cap, stamped(&s.tasks[0], &hook_json(CAP_OTHER, "clear"))).unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || {
            s.tick();
            snapshot(&rec).is_some_and(|(_, t)| t.contains(CAP_OTHER))
        }),
        "the cadence pass never picked up the drifted ID"
    );

    // Stable recipe content leaves the snapshot unchanged.
    let (path, _) = snapshot(&rec).unwrap();
    let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
    let rewritten = wait_until(Duration::from_millis(600), || {
        s.tick();
        std::fs::metadata(&path).unwrap().modified().unwrap() != mtime
    });
    assert!(
        !rewritten,
        "an unchanged recipe must not rewrite the snapshot"
    );
    let names: Vec<_> = std::fs::read_dir(&rec).unwrap().flatten().collect();
    assert_eq!(names.len(), 1, "one incarnation owns one snapshot file");
}

// --- managed launches --------------------------------------------------

/// The settings overlay beside the sole task's capture file.
fn settings_beside(s: &Supervisor) -> String {
    s.tasks[0]
        .capture_file
        .as_deref()
        .and_then(Path::parent)
        .expect("a managed task has a namespaced capture file")
        .join("claude-settings.json")
        .display()
        .to_string()
}

/// Run managed claude as the task leader regardless of `SHELL`. Accept its capture stamped
/// with `$$`; reject the same payload stamped with another PID and fall back to the pinned
/// ID.
#[test]
fn managed_claude_accepts_its_own_capture_and_refuses_a_foreign_stamp() {
    let dir = scratch("managed_capture");
    let (bin, runtime) = (dir.join("bin"), dir.join("run"));
    // Simulate Claude's SessionStart hook after `/clear`: write the changed ID and stamp
    // the capture with the stub's PID.
    install_script(
        &bin,
        "claude",
        &format!(
            "printf '%s\\n' \"$@\" > '{out}/argv'\n\
             printf '%s\\n{{\"session_id\":\"{CAP_OTHER}\",\"hook_event_name\":\"SessionStart\",\"source\":\"clear\"}}\\n' \"$$\" > \"$FLEETCOM_CAPTURE_FILE\"",
            out = dir.display()
        ),
    );
    let mut ctx = agent_ctx(&bin, &runtime, dir.to_path_buf());
    ctx.env.retain(|(k, _)| k != "SHELL");
    ctx.env.push((
        "SHELL".into(),
        install_resident_shell(&dir).into_os_string(),
    ));
    let mut s = sup_ctx(ctx);
    s.spawn_agent("claude", dir.to_path_buf(), None);
    assert!(acknowledged(&s.drain()), "a managed spawn is acknowledged");
    let argv = wait_argv(&mut s, &dir.join("argv"));
    let id = s.tasks[0].id;
    wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);

    let t = &s.tasks[0];
    assert_eq!(t.command, "claude", "the program word is the display text");
    assert!(t.harness.is_some());
    let pinned = t
        .resume_id
        .clone()
        .expect("a fresh claude launch pins an id");
    assert_eq!(
        argv,
        ["--session-id", &pinned, "--settings", &settings_beside(&s)]
    );
    assert_eq!(
        current_resume_id(&s.tasks[0]).as_deref(),
        Some(CAP_OTHER),
        "the stub's `$$` is the leader pid: no shell sat between"
    );

    // Reject the same payload when stamped with another process's PID.
    let cap = s.tasks[0].capture_file.clone().unwrap();
    let text = std::fs::read_to_string(&cap).unwrap();
    let (_, json) = text.split_once('\n').unwrap();
    std::fs::write(&cap, format!("{}\n{json}", dead_pid())).unwrap();
    assert_eq!(
        current_resume_id(&s.tasks[0]).as_deref(),
        Some(pinned.as_str()),
        "a foreign stamp must fall through to the pinned id"
    );
}

/// Refuse unknown words, missing binaries, and launches beyond the task ceiling. Report one
/// status and create no task in each case. Apply the same ceiling as for `Spawn`.
#[test]
fn managed_spawn_refuses_an_unknown_word_a_missing_binary_and_a_full_fleet() {
    let dir = scratch("managed_refusals");
    let (bin, runtime) = (dir.join("bin"), dir.join("run"));
    let mut s = sup_ctx(agent_ctx(&bin, &runtime, dir.to_path_buf()));

    s.spawn_agent("vim", dir.to_path_buf(), None);
    let events = s.drain();
    assert!(!acknowledged(&events));
    assert_sole_notice(events, "no agent named");
    s.spawn_agent("claude", dir.to_path_buf(), None);
    let events = s.drain();
    assert!(!acknowledged(&events));
    assert_sole_notice(events, "not found on PATH");
    assert!(s.tasks.is_empty(), "a refused launch creates nothing");
    assert!(!runtime.exists(), "a refused launch installs nothing");

    install_stub(&bin, "claude", &dir);
    s.set_max_tasks(1);
    s.spawn_agent("claude", dir.to_path_buf(), None);
    assert!(acknowledged(&s.drain()));
    assert_eq!(s.tasks.len(), 1);
    s.spawn_agent("claude", dir.to_path_buf(), None);
    assert_sole_notice(s.drain(), "task limit");
    spawn(&mut s, "sleep 1", dir.to_path_buf());
    assert_sole_notice(s.drain(), "task limit");
    assert_eq!(s.tasks.len(), 1, "the ceiling holds for both launch kinds");
}

/// Rerun a managed task through its harness: place the captured ID before the overlay;
/// preserve id, tag, group, name, and program word; increment the run number. Resolve the
/// binary on `PATH` again. If removed since launch, refuse the rerun and preserve the
/// finished task.
#[test]
fn managed_rerun_resumes_the_captured_id_from_the_binary_path_finds_now() {
    let dir = scratch("managed_rerun");
    let (bin, runtime) = (dir.join("bin"), dir.join("run"));
    install_stub(&bin, "claude", &dir);
    let mut s = sup_ctx(agent_ctx(&bin, &runtime, dir.to_path_buf()));
    s.spawn_agent("claude", dir.to_path_buf(), Some("agents".into()));
    let _ = wait_argv(&mut s, &dir.join("argv"));
    let id = s.tasks[0].id;
    s.tasks[0].tagged = true;
    s.tasks[0].name = Some("pilot".into());
    wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);
    std::fs::write(
        s.tasks[0].capture_file.as_ref().unwrap(),
        stamped(&s.tasks[0], &hook_json(CAP_OTHER, "clear")),
    )
    .unwrap();
    std::fs::remove_file(dir.join("argv")).unwrap();

    let old_cap = s.tasks[0].capture_file.clone().unwrap();
    s.apply(Command::Restart { id });
    let argv = wait_argv(&mut s, &dir.join("argv"));
    assert_eq!(
        argv,
        ["--resume", CAP_OTHER, "--settings", &settings_beside(&s)],
        "the intent part leads, the overlay follows, no second pin"
    );
    let t = &s.tasks[0];
    assert!(t.harness.is_some(), "a rerun keeps the task managed");
    assert_eq!((t.id, t.run, t.command.as_str()), (id, 1, "claude"));
    assert_eq!(t.resume_id.as_deref(), Some(CAP_OTHER));
    assert!(t.tagged);
    assert_eq!(t.group.as_deref(), Some("agents"));
    assert_eq!(t.name.as_deref(), Some("pilot"));
    assert!(
        reap_until(&mut s, Duration::from_secs(5), |s| s.graveyard.is_empty()),
        "the displaced run was never collected"
    );
    assert!(
        !old_cap.exists(),
        "rerun must delete the displaced run's capture file; the ID was read first"
    );

    // Resolve the binary again on rerun; refuse if it has been removed.
    wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);
    std::fs::remove_file(bin.join("claude")).unwrap();
    s.apply(Command::Restart { id });
    assert_sole_notice(s.drain(), "not found on PATH");
    assert_eq!(s.tasks[0].run, 1, "the finished task is preserved");
}

/// Without a known ID, start a fresh conversation on rerun. With no pin support in omp, use
/// only the overlay on both runs.
#[test]
fn managed_rerun_without_an_id_starts_fresh() {
    let dir = scratch("managed_rerun_fresh");
    let (bin, runtime) = (dir.join("bin"), dir.join("run"));
    install_stub(&bin, "omp", &dir);
    let mut s = sup_ctx(agent_ctx(&bin, &runtime, dir.to_path_buf()));
    s.spawn_agent("omp", dir.to_path_buf(), None);
    let first = wait_argv(&mut s, &dir.join("argv"));
    let id = s.tasks[0].id;
    assert_eq!(first[0], "-e");
    assert_eq!(first.len(), 2);
    assert!(s.tasks[0].resume_id.is_none(), "omp cannot pin an id");
    wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);
    std::fs::remove_file(dir.join("argv")).unwrap();

    s.apply(Command::Restart { id });
    let again = wait_argv(&mut s, &dir.join("argv"));
    assert_eq!(again, first, "no id known: the rerun is a fresh launch");
    assert!(s.tasks[0].harness.is_some());
    assert_eq!(s.tasks[0].run, 1);
}

/// Save a managed entry with its current session ID, then reload it as managed: the
/// selector and ID lead argv, the reloaded run's own capture is accepted, and the group
/// survives.
#[test]
fn managed_task_saves_as_a_managed_entry_and_reloads_managed() {
    let dir = scratch("managed_save");
    let (bin, runtime, config) = (dir.join("bin"), dir.join("run"), dir.join("config"));
    // Use one record directory per stub. Reload tasks together without a shared argv file
    // that could be truncated during a read.
    let (claude_out, omp_out) = (dir.join("claude-out"), dir.join("omp-out"));
    for out in [&claude_out, &omp_out] {
        std::fs::create_dir_all(out).unwrap();
    }
    // Simulate Claude's SessionStart hook in the stub. Stamp the capture with `$$`, which
    // is accepted only when the stub is the task leader.
    install_script(
        &bin,
        "claude",
        &format!(
            "printf '%s\\n{{\"session_id\":\"{CAP_OTHER}\"}}\\n' \"$$\" > \"$FLEETCOM_CAPTURE_FILE\"\n\
             printf '%s\\n' \"$@\" > '{out}/argv.'$$'.tmp' && mv '{out}/argv.'$$'.tmp' '{out}/argv'",
            out = claude_out.display()
        ),
    );
    install_stub(&bin, "omp", &omp_out);
    let mut s = sup_ctx(agent_ctx_plus(
        &bin,
        &runtime,
        dir.to_path_buf(),
        &[("FLEETCOM_CONFIG_DIR", &config)],
    ));
    s.spawn_agent("claude", dir.to_path_buf(), Some("agents".into()));
    s.spawn_agent("omp", dir.to_path_buf(), None);
    assert!(s.tasks.iter().all(|t| t.harness.is_some()));
    // Wait for both first-run records before touching captures or clearing
    // them, so a late write from the original launch cannot pass for the
    // reloaded one.
    wait_argv(&mut s, &claude_out.join("argv"));
    wait_argv(&mut s, &omp_out.join("argv"));
    let claude = s.tasks[0].id;
    wait_for_lifecycle(&mut s, claude, |l| l == Lifecycle::Ok);
    std::fs::write(
        s.tasks[0].capture_file.as_ref().unwrap(),
        stamped(&s.tasks[0], &format!(r#"{{"session_id":"{CAP_ID}"}}"#)),
    )
    .unwrap();

    save_and_read(&mut s, &config, "managed");
    assert_eq!(
        session::load_in(&config.join("sessions"), "managed").unwrap(),
        SessionConfig::from([(
            path::abbreviate(&dir),
            vec![
                SessionEntry {
                    group: Some("agents".into()),
                    ..SessionEntry::managed("claude", Some(CAP_ID))
                },
                SessionEntry::managed("omp", None),
            ]
        )])
    );

    std::fs::remove_file(claude_out.join("argv")).unwrap();
    std::fs::remove_file(omp_out.join("argv")).unwrap();
    s.apply(Command::LoadSession {
        name: "managed".into(),
    });
    assert_eq!(s.tasks.len(), 4, "{:?}", notices(s.drain()));
    let reloaded = &s.tasks[2];
    assert!(
        reloaded.harness.is_some(),
        "a managed entry reloads managed"
    );
    assert_eq!(reloaded.command, "claude");
    assert_eq!(reloaded.resume_id.as_deref(), Some(CAP_ID));
    assert_eq!(reloaded.group.as_deref(), Some("agents"));
    assert_eq!(s.tasks[3].command, "omp");
    assert!(s.tasks[3].harness.is_some());
    assert!(s.tasks[3].resume_id.is_none());
    let claude_argv = wait_argv(&mut s, &claude_out.join("argv"));
    assert!(
        claude_argv.starts_with(&["--resume".into(), CAP_ID.into()]),
        "the reloaded claude entry must resume its captured ID: {claude_argv:?}"
    );
    assert_eq!(
        current_resume_id(&s.tasks[2]).as_deref(),
        Some(CAP_OTHER),
        "the reloaded run's own capture outranks the recipe's resume ID"
    );
    let omp_argv = wait_argv(&mut s, &omp_out.join("argv"));
    assert_eq!(
        omp_argv.first().map(String::as_str),
        Some("-e"),
        "the reloaded omp entry must load the capture extension: {omp_argv:?}"
    );
}

/// Continue loading a recipe after a missing agent, including the failure reason in the
/// final summary. Only the last status in a client poll is displayed, so a separate earlier
/// diagnostic would be lost. Report recovery loads the same way.
#[test]
fn load_reports_a_missing_agent_and_loads_the_rest() {
    let dir = scratch("load_missing_agent");
    let (bin, runtime, config) = (dir.join("bin"), dir.join("run"), dir.join("config"));
    install_stub(&bin, "claude", &dir);
    let mut s = sup_ctx(agent_ctx_plus(
        &bin,
        &runtime,
        dir.to_path_buf(),
        &[("FLEETCOM_CONFIG_DIR", &config)],
    ));
    load_recipe(
        &mut s,
        &config,
        "fleet",
        &dir,
        vec![
            SessionEntry::managed("grok", None),
            SessionEntry::literal("true"),
            SessionEntry {
                name: Some("pilot".into()),
                ..SessionEntry::managed("claude", Some(CAP_ID))
            },
        ],
    );
    let got = notices(s.drain());
    assert!(
        got.len() == 1
            && got[0].contains("1 failed to spawn")
            && got[0].contains("grok not found on PATH"),
        "one summary naming the missing agent; got {got:?}"
    );
    assert_eq!(s.tasks.len(), 2);
    assert!(s.tasks[0].harness.is_none());
    assert_eq!(s.tasks[0].command, "true");
    assert!(s.tasks[1].harness.is_some());
    assert_eq!(s.tasks[1].name.as_deref(), Some("pilot"));
    assert_eq!(
        wait_argv(&mut s, &dir.join("argv"))[..2],
        ["--resume", CAP_ID]
    );

    let stem = "20990101-000000-1";
    let cfg = SessionConfig::from([(
        dir.to_string_lossy().into_owned(),
        vec![SessionEntry::managed("grok", None)],
    )]);
    session::save_recovery_in(
        &session::recovery_dir(&config.join("sessions")),
        stem,
        "snapshot",
        &cfg,
    )
    .unwrap();
    s.apply(Command::LoadRecovery { stem: stem.into() });
    assert_sole_notice(s.drain(), "grok not found on PATH");
}

/// With capture disabled, a rerun places `resume <id>` before the embedded override and
/// repeats the capture notice. The launch's own notice and embedded-only argv are pinned by
/// `unrepresentable_config_notify_suppresses_injection`.
#[test]
fn managed_codex_reports_the_capture_notice_and_leads_a_rerun_with_resume() {
    let dir = scratch("managed_codex");
    let (bin, runtime, codex_home) = (dir.join("bin"), dir.join("run"), dir.join("codex_home"));
    std::fs::create_dir_all(&codex_home).unwrap();
    std::fs::write(codex_home.join("config.toml"), "notify = [1]\n").unwrap();
    install_stub(&bin, "codex", &dir);
    let mut s = sup_ctx(agent_ctx_plus(
        &bin,
        &runtime,
        dir.to_path_buf(),
        &[("CODEX_HOME", &codex_home)],
    ));
    s.spawn_agent("codex", dir.to_path_buf(), None);
    // Drain the launch notice so the rerun's is the sole one.
    let _ = s.drain();
    let _ = wait_argv(&mut s, &dir.join("argv"));
    let id = s.tasks[0].id;
    assert!(s.tasks[0].resume_id.is_none(), "codex cannot pin an id");
    wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);

    // The v1 slot holds one bare root UUID.
    std::fs::write(s.tasks[0].capture_file.as_ref().unwrap(), CAP_ID).unwrap();
    std::fs::remove_file(dir.join("argv")).unwrap();
    s.apply(Command::Restart { id });
    assert_sole_notice(s.drain(), "capture unavailable");
    assert_eq!(
        wait_argv(&mut s, &dir.join("argv")),
        ["resume", CAP_ID, "-c", "features.daemon_auto_start=false"]
    );
    assert_eq!(s.tasks[0].resume_id.as_deref(), Some(CAP_ID));
}

// --- live registry blocked status --------------------------------------

/// Tick until the sole task's preview satisfies `pred` or the budget expires,
/// then return the last preview.
fn tick_until_preview(
    s: &mut Supervisor,
    budget: Duration,
    mut pred: impl FnMut(&Preview) -> bool,
) -> Preview {
    let mut last = None;
    wait_until(budget, || {
        s.tick();
        for e in s.drain() {
            if let Event::Tasks(v) = e
                && let Some(t) = v.into_iter().next()
            {
                last = Some(t.preview);
            }
        }
        last.as_ref().is_some_and(&mut pred)
    });
    last.expect("a Tasks snapshot must carry the task's preview")
}

/// A matching `waiting` record reaches the dashboard; non-waiting and post-exit
/// records do not.
#[test]
fn registry_waiting_status_reaches_the_dashboard_preview() {
    let dir = scratch("registry_blocked");
    let (bin, runtime) = (dir.join("bin"), dir.join("run"));
    let (claude_home, done) = (dir.join("claude_home"), dir.join("done"));
    install_script(
        &bin,
        "claude",
        &format!(
            "until [ -e '{d}' ]; do sleep 0.05; done",
            d = done.display()
        ),
    );
    let mut s = sup_ctx(agent_ctx_plus(
        &bin,
        &runtime,
        dir.to_path_buf(),
        &[("CLAUDE_CONFIG_DIR", &claude_home)],
    ));
    s.spawn_agent("claude", dir.to_path_buf(), None);
    // Key the record to the task's leader PID.
    let pid = s.tasks[0].pid().expect("a live task has a pid");

    // A non-waiting status must not anchor the preview.
    install_status_record(&claude_home, pid, &dir, r#""status":"idle""#);
    let p = tick_until_preview(&mut s, Duration::from_millis(750), |p| {
        p.source == PreviewSource::Anchor
    });
    assert_ne!(
        p.source,
        PreviewSource::Anchor,
        "a non-waiting record must not anchor the preview: {p:?}"
    );

    // A waiting status becomes an Anchor preview.
    install_status_record(
        &claude_home,
        pid,
        &dir,
        r#""status":"waiting","waitingFor":"permission prompt""#,
    );
    let p = tick_until_preview(&mut s, Duration::from_secs(5), |p| {
        p.source == PreviewSource::Anchor
    });
    assert_eq!(
        (p.text.as_str(), p.source, p.rule),
        (
            "awaiting approval",
            PreviewSource::Anchor,
            Some("claude:registry-approval")
        ),
        "a waiting record must reach the dashboard as the anchor tier"
    );

    // After exit, a changed record must neither retain nor replace the cached
    // blocked preview.
    std::fs::write(&done, b"").unwrap();
    assert!(
        reap_until(&mut s, Duration::from_secs(5), |s| s.tasks[0]
            .finished
            .is_some()),
        "the leader never exited"
    );
    install_status_record(
        &claude_home,
        pid,
        &dir,
        r#""status":"waiting","waitingFor":"dialog open""#,
    );
    let p = tick_until_preview(&mut s, Duration::from_secs(5), |p| {
        p.text != "awaiting approval"
    });
    assert!(
        p.text != "awaiting approval" && p.text != "dialog open",
        "an exited leader must not render as blocked: {p:?}"
    );
    assert!(
        claude_home
            .join("sessions")
            .join(format!("{pid}.json"))
            .is_file(),
        "the surviving record is the whole point of the case"
    );
}
