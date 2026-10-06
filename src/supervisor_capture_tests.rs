use super::*;
use crate::{
    harness::fixtures::{ID as CAP_ID, OTHER as CAP_OTHER},
    protocol::{Lifecycle, Preview, PreviewSource},
    testutil::{codex_session_meta, dead_pid, install_codex_rollout, install_codex_root},
};

// --- session-capture wiring -------------------------------------------

/// Install an executable stub that records `FLEETCOM_CAPTURE_FILE`,
/// `FLEETCOM_BINARY`, and its argv, one token per line, then exits.
fn install_stub(bin: &Path, name: &str, out: &Path) {
    install_script(
        bin,
        name,
        // Write each record to a per-process file beside its destination and
        // rename it into place: `>` truncates first and `printf` may write
        // one argument at a time, so a reader polling for a non-empty file
        // could otherwise see a prefix of the argv.
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

/// Save a recipe and return its persisted JSON.
fn save_and_read(s: &mut Supervisor, config: &Path, name: &str) -> String {
    s.apply(Command::SaveSession { name: name.into() });
    let _ = s.drain();
    std::fs::read_to_string(config.join("sessions").join(format!("{name}.json"))).unwrap()
}

/// Drain the queued events and return the status lines among them.
fn notices(s: &mut Supervisor) -> Vec<String> {
    s.drain()
        .into_iter()
        .filter_map(|e| match e {
            Event::Status(m) => Some(m),
            _ => None,
        })
        .collect()
}

/// Whether a drained event batch acknowledges a spawn.
fn acknowledged(events: &[Event]) -> bool {
    events.iter().any(|e| matches!(e, Event::Spawned { .. }))
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

/// A `claude` spawn receives a pinned ID, settings overlay, and capture
/// environment without changing the stored command.
#[test]
fn spawn_claude_pins_an_id_and_layers_settings() {
    let dir = scratch("cap_claude");
    let (bin, runtime) = (dir.join("bin"), dir.join("run"));
    install_stub(&bin, "claude", &dir);
    let mut s = sup_ctx(agent_ctx(&bin, &runtime, dir.to_path_buf()));
    spawn(&mut s, "claude", dir.to_path_buf());

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

/// A resuming `claude` launch retains its target ID and adds only the
/// capture overlay.
#[test]
fn spawn_resuming_claude_injects_only_the_capture_channel() {
    let dir = scratch("cap_resume");
    let (bin, runtime) = (dir.join("bin"), dir.join("run"));
    install_stub(&bin, "claude", &dir);
    let mut s = sup_ctx(agent_ctx(&bin, &runtime, dir.to_path_buf()));
    spawn(
        &mut s,
        format!("claude --resume {CAP_ID}"),
        dir.to_path_buf(),
    );

    let argv = wait_argv(&mut s, &dir.join("argv"));
    assert!(
        !argv.iter().any(|a| a == "--session-id"),
        "a resuming launch must never pin a second id; argv: {argv:?}"
    );
    assert!(
        argv.iter().any(|a| a == "--settings"),
        "the settings overlay must still ride along; argv: {argv:?}"
    );
    let t = &s.tasks[0];
    assert_eq!(t.command, format!("claude --resume {CAP_ID}"));
    assert_eq!(t.resume_id.as_deref(), Some(CAP_ID));
}

/// Rerun prefers the capture-file ID, stores the resulting resume command,
/// and deletes the displaced run's capture after deriving the resume command.
#[test]
fn rerun_resumes_the_captured_conversation() {
    use crate::protocol::Lifecycle;
    let dir = scratch("cap_rerun");
    let (bin, runtime) = (dir.join("bin"), dir.join("run"));
    install_stub(&bin, "claude", &dir);
    let mut s = sup_ctx(agent_ctx(&bin, &runtime, dir.to_path_buf()));
    spawn(&mut s, "claude", dir.to_path_buf());
    let _ = wait_argv(&mut s, &dir.join("argv"));
    let id = s.tasks[0].id;
    wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);

    // The capture payload reports a different ID from the pinned one.
    let cap = s.tasks[0].capture_file.clone().expect("capture file set");
    std::fs::write(
        &cap,
        stamped(
            &s.tasks[0],
            &format!(
                r#"{{"session_id":"{CAP_OTHER}","hook_event_name":"SessionStart","source":"clear"}}"#
            ),
        ),
    )
    .unwrap();
    std::fs::remove_file(dir.join("argv")).unwrap();

    s.apply(Command::Restart { id });
    let argv = wait_argv(&mut s, &dir.join("argv"));
    assert_eq!(
        s.tasks[0].command,
        format!("claude --resume '{CAP_OTHER}'"),
        "the stored command must become the resuming one"
    );
    let ri = argv
        .iter()
        .position(|a| a == "--resume")
        .expect("the respawn must resume");
    assert_eq!(argv[ri + 1], CAP_OTHER);
    assert!(
        !argv.iter().any(|a| a == "--session-id"),
        "re-detection classifies the respawn as resuming: no second id"
    );
    assert!(
        reap_until(&mut s, Duration::from_secs(5), |s| s.graveyard.is_empty()),
        "the displaced run was never collected"
    );
    // The resume command retains the ID after the source capture is deleted.
    assert!(
        !cap.exists(),
        "rerun must delete the displaced run's capture file"
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
    spawn(&mut s, "claude", dir.to_path_buf());
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
    assert_eq!(s.tasks[0].command, format!("claude --resume '{CAP_ID}'"));
    assert!(
        !old_cap.exists(),
        "rerun must delete the displaced run's capture file"
    );

    // A late hook write can recreate the old path, but the new run cannot read it.
    // Stamp with the new leader's PID to test isolation by path alone.
    let stale = stamped(
        &s.tasks[0],
        &format!(
            r#"{{"session_id":"{CAP_OTHER}","hook_event_name":"SessionStart","source":"startup"}}"#
        ),
    );
    std::fs::write(&old_cap, &stale).unwrap();
    let text = save_and_read(&mut s, &config, "stalecap");
    assert!(
        text.contains(&format!("claude --resume '{CAP_ID}'")),
        "the fresh run must save the drifted session; got {text}"
    );
    assert!(
        !text.contains(CAP_OTHER),
        "the old run's stale capture must be unreachable; got {text}"
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
    spawn(&mut s, "claude", dir.to_path_buf());
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
    spawn(&mut s, "claude", dir.to_path_buf());
    let cap = s.tasks[0].capture_file.clone().expect("capture file set");
    std::fs::write(&cap, "{}").unwrap();

    // The client reconnects with an identical env and spawns again.
    s.set_launch_context(agent_ctx(&bin, &runtime, dir.to_path_buf()));
    spawn(&mut s, "claude", dir.to_path_buf());
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
    spawn(&mut s, "claude", dir.to_path_buf());
    let cap_a = s.tasks[0].capture_file.clone().expect("capture file set");
    std::fs::write(&cap_a, "{}").unwrap();

    // The client reconnects under root B, spawns, then returns to A and
    // spawns again.
    s.set_launch_context(agent_ctx(&bin, &root_b, dir.to_path_buf()));
    spawn(&mut s, "claude", dir.to_path_buf());
    s.set_launch_context(agent_ctx(&bin, &root_a, dir.to_path_buf()));
    spawn(&mut s, "claude", dir.to_path_buf());

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
    spawn(&mut s, "claude", dir.to_path_buf());
    let _ = wait_argv(&mut s, &dir.join("argv"));
    let id = s.tasks[0].id;
    wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);
    let cap = s.tasks[0].capture_file.clone().unwrap();
    std::fs::write(&cap, "{}").unwrap();

    // Root B is installed by a newer spawn; a same-id file under it must
    // survive the A task's removal.
    s.set_launch_context(agent_ctx(&bin, &root_b, dir.to_path_buf()));
    spawn(&mut s, "claude", dir.to_path_buf());
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

/// For a `codex` spawn, add `notify=[...]` with the executable capture script,
/// then the override for embedded mode, and name this process's own
/// executable in `FLEETCOM_BINARY` for the script's validation step.
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
    spawn(&mut s, "codex", dir.to_path_buf());

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
    spawn(&mut s, "grok", dir.to_path_buf());

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

    let text = save_and_read(&mut s, &config, "grokpin");
    assert!(
        text.contains(&format!("grok --resume '{id}'")),
        "the recipe must resume the pinned session; got {text}"
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
    spawn(&mut s, "omp", dir.to_path_buf());

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

/// Printed hints are display content even after exit and reader EOF. Named
/// saves, recovery snapshots, and reruns retain the capture or launch ID; an
/// uncaptured task retains its exact authored command.
#[test]
fn printed_resume_hints_do_not_change_saved_recovery_or_rerun_commands() {
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
        let authored = format!("  {}/{}\t", bin.display(), tool);
        spawn(&mut s, &authored, dir.to_path_buf());
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
        let command = match expected_id.as_deref() {
            Some(id) => format!("{}/{} --resume '{id}'", bin.display(), tool),
            None => authored,
        };
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
                cmd: command.clone(),
                group: Some("agents".into()),
                name: Some(tool.into()),
            }],
        )]);
        save_and_read(&mut s, &config, "hints");
        assert_eq!(
            session::load_in(&config.join("sessions"), "hints").unwrap(),
            expected,
            "{tool}: named save"
        );
        assert!(
            wait_until(Duration::from_secs(5), || {
                s.tick();
                !recovery_files(&config).is_empty()
            }),
            "{tool}: recovery snapshot never landed"
        );
        let files = recovery_files(&config);
        assert_eq!(files.len(), 1);
        let stem = files[0].strip_suffix(".json").unwrap();
        assert_eq!(
            session::load_recovery_in(&config.join("sessions/recovery"), stem).unwrap(),
            expected,
            "{tool}: recovery"
        );
        let id = s.tasks[0].id;
        s.apply(Command::Restart { id });
        assert_eq!(s.tasks[0].run, 1, "{tool}: rerun must replace the task");
        assert_eq!(s.tasks[0].command, command, "{tool}: rerun command");
        assert_eq!(s.tasks[0].resume_id, expected_id, "{tool}: rerun ID");
    }
}

/// Rerun latches a recent exit before checking eligibility and retains the
/// explicit launch ID when terminal output names another conversation.
#[test]
fn rerun_latches_exit_without_reap_and_preserves_the_launch_id() {
    use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};
    let dir = scratch("rerun_without_reap");
    let (bin, runtime) = (dir.join("bin"), dir.join("run"));
    install_script(
        &bin,
        "grok",
        &format!(
            "printf '%s\\n' \"$@\" > '{}/argv'\n\
             printf 'grok --resume {CAP_ID}\\n'",
            dir.display()
        ),
    );
    let mut s = sup_ctx(agent_ctx(&bin, &runtime, dir.to_path_buf()));
    spawn(
        &mut s,
        format!("grok --resume {CAP_OTHER}"),
        dir.to_path_buf(),
    );
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
    assert_eq!(s.tasks[0].command, format!("grok --resume '{CAP_OTHER}'"));
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
    spawn(&mut s, "claude", dir.to_path_buf());
    let injected = s.tasks[0]
        .resume_id
        .clone()
        .expect("a fresh claude launch pins an id");
    assert_ne!(injected.as_str(), CAP_ID);
    // Key the registry fixture to the spawned task.
    let pid = s.tasks[0].pid().expect("a live task has a pid");

    install_status_record(&claude_home, pid, &dir, r#""status":"idle""#);
    let text = save_and_read(&mut s, &config, "registry");
    assert!(
        text.contains(&format!("claude --resume '{CAP_ID}'")),
        "the registry must beat the injected id; got {text}"
    );
    assert!(
        !text.contains(&injected),
        "the injected id must not survive the registry; got {text}"
    );

    // A capture-file ID outranks the registry ID.
    let cap = s.tasks[0].capture_file.clone().expect("capture file set");
    std::fs::write(
        &cap,
        stamped(
            &s.tasks[0],
            &format!(
                r#"{{"session_id":"{CAP_OTHER}","hook_event_name":"SessionStart","source":"clear"}}"#
            ),
        ),
    )
    .unwrap();
    let text = save_and_read(&mut s, &config, "capture");
    assert!(
        text.contains(&format!("claude --resume '{CAP_OTHER}'")),
        "the capture file must beat the registry; got {text}"
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
    let json = |id: &str, source: &str| {
        format!(r#"{{"session_id":"{id}","hook_event_name":"SessionStart","source":"{source}"}}"#)
    };
    // The stub is the task leader only when `/bin/sh -c` replaces itself with
    // it, which Ubuntu's `/bin/sh` does not. The test therefore writes the
    // task's own capture itself, with the leader's stamp. The stub runs the
    // installed hook under an intermediate shell with the same inherited
    // environment, as in Claude's daemon. Append `:` to prevent an exec of
    // the pipeline's last command: the hook's `$PPID` must be the
    // intermediate shell, which is never the leader.
    install_script(
        &bin,
        "claude",
        &format!(
            r#"until [ -e '{d}/foreign' ]; do sleep 0.05; done
sh -c 'printf "%s\n" "$1" | sh "$0"; :' '{d}/hook' '{foreign}'
: > '{d}/foreign-done'
until [ -e '{d}/done' ]; do sleep 0.05; done"#,
            d = dir.display(),
            foreign = json(FOREIGN_ID, "startup"),
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

    std::fs::write(&cap, stamped(&s.tasks[0], &json(CAP_OTHER, "clear"))).unwrap();
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
        format!("{}\n", json(FOREIGN_ID, "startup")),
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
    let text = save_and_read(&mut s, &config, "foreign");
    assert!(
        text.contains(&format!("claude --resume '{CAP_ID}'")) && !text.contains(FOREIGN_ID),
        "the recipe must never resume the foreign session; got {text}"
    );
    std::fs::write(dir.join("done"), b"").unwrap();
}

/// Two tasks sharing a directory do not own a nearby rollout. Named saves
/// and recovery must preserve both authored commands when capture is silent.
#[test]
fn silent_codex_tasks_keep_authored_commands_in_saves_and_recovery() {
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
    let commands = ["codex".to_string(), format!("  {}/codex\t", bin.display())];
    for command in &commands {
        spawn(&mut s, command, dir.to_path_buf());
    }
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
        commands
            .iter()
            .map(|cmd| SessionEntry {
                cmd: cmd.clone(),
                group: None,
                name: None,
            })
            .collect(),
    )]);
    save_and_read(&mut s, &config, "silent");
    assert_eq!(
        session::load_in(&config.join("sessions"), "silent").unwrap(),
        expected
    );

    assert!(
        wait_until(Duration::from_secs(5), || {
            s.tick();
            !recovery_files(&config).is_empty()
        }),
        "the recovery snapshot never landed"
    );
    let files = recovery_files(&config);
    assert_eq!(files.len(), 1);
    let stem = files[0].strip_suffix(".json").unwrap();
    let recovery = config.join("sessions").join("recovery");
    assert_eq!(
        session::load_recovery_in(&recovery, stem).unwrap(),
        expected
    );
}

/// Thread IDs reported through one codex process's notifier: the
/// conversation, a spawned sub-agent, and the hidden title thread.
const CODEX_ROOT: &str = "019f5453-de22-7240-b2e5-0d32692aa6d9";
const CODEX_CHILD: &str = "019f5454-0c11-7b33-9a4e-5f0e6d7c8b9a";
const CODEX_TITLE: &str = "019f5453-de9f-7e61-8c0d-1a2b3c4d5e6f";

/// Notification JSON for a completed turn of `thread`, as codex passes it
/// to the injected notifier. `last` is the final assistant message, already
/// escaped for a JSON string.
fn turn_complete(thread: &str, last: &str) -> String {
    format!(
        r#"{{"type":"agent-turn-complete","thread-id":"{thread}","turn-id":"t","cwd":"/w","input-messages":["ping"],"last-assistant-message":"{last}"}}"#
    )
}

/// The title thread's notification: its final message is the generated title.
fn title_turn() -> String {
    turn_complete(CODEX_TITLE, r#"{\"title\":\"Ping the sub-agent\"}"#)
}

/// Run the `--codex-notify-v1` step in-process for the sole task, as the
/// injected script's binary call would: the task's capture file and
/// `codex_home` are its environment. Return the root it wrote.
fn arrive(s: &Supervisor, codex_home: &Path, payload: &str) -> Option<String> {
    let cap = s.tasks[0].capture_file.clone().expect("capture file set");
    crate::harness::record_arrival(payload, &|key| match key {
        crate::harness::CAPTURE_ENV => Some(cap.clone()),
        "CODEX_HOME" => Some(codex_home.to_path_buf()),
        _ => None,
    })
}

/// Save under `name` and return the persisted command of the sole task.
fn saved_command(s: &mut Supervisor, config: &Path, name: &str) -> String {
    save_and_read(s, config, name);
    session::load_in(&config.join("sessions"), name)
        .unwrap()
        .into_values()
        .flatten()
        .next()
        .expect("the recipe must hold the task")
        .cmd
}

/// Install a stand-in for `fleetcom --codex-notify-v1` at `<bin>/fleetcom`
/// that writes `CAP_ID` over the capture file. The real mode cannot run from
/// the unit-test binary, whose `main` is the test harness, so stubs that
/// invoke the injected script name this file in `FLEETCOM_BINARY`.
fn install_fake_fleetcom(bin: &Path) -> PathBuf {
    install_script(
        bin,
        "fleetcom",
        &format!("printf '%s' '{CAP_ID}' > \"$FLEETCOM_CAPTURE_FILE\""),
    );
    bin.join("fleetcom")
}

/// Report three threads from one codex process through the arrival step in
/// the order observed after the first prompt: the title thread, a sub-agent,
/// then the conversation, and the title thread again, which lands after the
/// root in most sessions. With no rollout for the title thread, nothing is
/// written and the authored command is preserved. Resolve the sub-agent to
/// its conversation and the conversation to itself, and keep that root
/// across the later title notification.
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
    spawn(&mut s, "codex", dir.to_path_buf());
    assert!(s.tasks[0].resume_id.is_none(), "codex pins no id at launch");

    assert_eq!(arrive(&s, &codex_home, &title_turn()), None);
    assert_eq!(
        saved_command(&mut s, &config, "title"),
        "codex",
        "the title thread must not become the resume target"
    );
    let resumes_root = format!("codex resume '{CODEX_ROOT}'");
    assert_eq!(
        arrive(&s, &codex_home, &turn_complete(CODEX_CHILD, "pong")).as_deref(),
        Some(CODEX_ROOT)
    );
    assert_eq!(
        saved_command(&mut s, &config, "child"),
        resumes_root,
        "a sub-agent's turn must resume the conversation that spawned it"
    );
    assert_eq!(
        arrive(&s, &codex_home, &turn_complete(CODEX_ROOT, "done")).as_deref(),
        Some(CODEX_ROOT)
    );
    assert_eq!(
        saved_command(&mut s, &config, "root"),
        resumes_root,
        "the conversation's own turn must resume it"
    );
    assert_eq!(arrive(&s, &codex_home, &title_turn()), None);
    assert_eq!(
        saved_command(&mut s, &config, "after_title"),
        resumes_root,
        "a title notification after the root's must leave the root in place"
    );
}

/// For `codex resume`, preserve the launch ID after rejecting notifications
/// for the title thread or a rollout outside the launch-time Codex home.
/// Prefer a captured root thread in the task's own home over the launch ID.
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
    let authored = format!("codex resume '{CAP_ID}'");
    spawn(&mut s, &authored, dir.to_path_buf());
    assert_eq!(s.tasks[0].resume_id.as_deref(), Some(CAP_ID));

    assert_eq!(arrive(&s, &codex_home, &title_turn()), None);
    assert_eq!(
        saved_command(&mut s, &config, "title"),
        authored,
        "the title thread must not displace the launch target"
    );
    assert_eq!(arrive(&s, &codex_home, &turn_complete(FOREIGN, "hi")), None);
    assert_eq!(
        saved_command(&mut s, &config, "foreign"),
        authored,
        "a thread outside the task's Codex home must not displace the launch target"
    );
    assert_eq!(
        arrive(&s, &codex_home, &turn_complete(CODEX_ROOT, "done")).as_deref(),
        Some(CODEX_ROOT)
    );
    assert_eq!(
        saved_command(&mut s, &config, "root"),
        format!("codex resume '{CODEX_ROOT}'"),
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

/// With only `HOME` in the launch environment, notify routing reads
/// `<home>/.codex/config.toml`: an unchainable route there leaves the launch
/// with the embedded override alone.
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
    spawn(&mut s, "codex", dir.to_path_buf());
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

    save_and_read(&mut s, &config, "homeonly");
    let cfg = session::load_in(&config.join("sessions"), "homeonly").unwrap();
    assert_eq!(
        cfg[&path::abbreviate(&dir)],
        vec![SessionEntry {
            cmd: "codex".into(),
            group: None,
            name: None,
        }]
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

    // The stub invokes the injected notify script the way codex would,
    // naming the fake binary for the script's validation step.
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
    spawn(&mut s, "codex", dir.to_path_buf());
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

/// Without a known ID, an agent recipe retains the original command.
#[test]
fn agent_save_without_any_id_keeps_the_plain_command() {
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
    spawn(&mut s, "codex", dir.to_path_buf());
    assert!(reap_until(&mut s, Duration::from_secs(5), |s| s.tasks[0]
        .finished
        .is_some()));

    let text = save_and_read(&mut s, &config, "plainagent");
    assert!(
        text.contains("\"codex\""),
        "the plain command must survive; got {text}"
    );
    assert!(
        !text.contains("resume"),
        "no id exists, so nothing may be rewritten; got {text}"
    );
}

/// A representable `notify` assignment runs through the injected notifier
/// after the validation step.
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

    // The stub records argv and the chain env, then invokes the notify
    // script with notification JSON as the final argument, naming the fake
    // binary for the script's validation step.
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
    spawn(&mut s, "codex", dir.to_path_buf());
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

/// An unrepresentable `notify` value disables capture injection: the launch
/// carries the embedded override alone and the status line says why. A
/// commented assignment defines no route, so injection returns and no notice
/// is sent.
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
    spawn(&mut s, "codex", dir.to_path_buf());
    assert_eq!(
        notices(&mut s),
        ["task 1: codex capture unavailable: `notify` config can't be chained"]
    );
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
    spawn(&mut s, "codex", dir.to_path_buf());
    assert_eq!(notices(&mut s), Vec::<String>::new());
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
        vec![SessionEntry {
            cmd: "sleep 30".into(),
            group: None,
            name: None,
        }]
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
    spawn(&mut s, "claude", dir.to_path_buf());
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
    std::fs::write(
        &cap,
        stamped(
            &s.tasks[0],
            &format!(
                r#"{{"session_id":"{CAP_OTHER}","hook_event_name":"SessionStart","source":"clear"}}"#
            ),
        ),
    )
    .unwrap();
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

/// Install a resident shell at `<dir>/resident-sh`: it runs its `-c` text as
/// a child and stays the leader, as tcsh and csh do. A launch context naming
/// it as `SHELL` proves the shell plays no part in a managed launch.
fn install_resident_shell(dir: &Path) -> PathBuf {
    let shell = dir.join("resident-sh");
    // `:` after the child keeps sh from exec'ing it as the script's last
    // command.
    write_executable(&shell, "[ \"$1\" = -c ] || exit 2\n/bin/sh -c \"$2\"\n:");
    shell
}

/// `agent_ctx` with `SHELL` pointing at the resident shell.
fn resident_shell_ctx(bin: &Path, runtime: &Path, dir: &Path) -> LaunchContext {
    let shell = install_resident_shell(dir);
    let mut ctx = agent_ctx(bin, runtime, dir.to_path_buf());
    ctx.env.retain(|(k, _)| k != "SHELL");
    ctx.env.push(("SHELL".into(), shell.into_os_string()));
    ctx
}

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

/// A managed claude is the task leader whatever `SHELL` names, so a capture
/// its own hook stamps with `$$` passes the gate; the same payload stamped
/// by any other process is refused and the pinned ID stands.
#[test]
fn managed_claude_accepts_its_own_capture_and_refuses_a_foreign_stamp() {
    let dir = scratch("managed_capture");
    let (bin, runtime) = (dir.join("bin"), dir.join("run"));
    // The stub plays claude running its SessionStart hook after `/clear`:
    // the capture names a drifted ID under the stub's own pid.
    install_script(
        &bin,
        "claude",
        &format!(
            "printf '%s\\n' \"$@\" > '{out}/argv'\n\
             printf '%s\\n{{\"session_id\":\"{CAP_OTHER}\",\"hook_event_name\":\"SessionStart\",\"source\":\"clear\"}}\\n' \"$$\" > \"$FLEETCOM_CAPTURE_FILE\"",
            out = dir.display()
        ),
    );
    let mut s = sup_ctx(resident_shell_ctx(&bin, &runtime, &dir));
    s.spawn_agent("claude", dir.to_path_buf(), None);
    assert!(acknowledged(&s.drain()), "a managed spawn is acknowledged");
    let argv = wait_argv(&mut s, &dir.join("argv"));
    let id = s.tasks[0].id;
    wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);

    let t = &s.tasks[0];
    assert!(t.managed);
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

    // Any other process's stamp over the same payload is refused.
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

/// An unknown word, a word with no binary on `PATH`, and a full fleet each
/// produce one status line and no task; the task ceiling is the one `Spawn`
/// uses.
#[test]
fn managed_spawn_refuses_an_unknown_word_a_missing_binary_and_a_full_fleet() {
    let dir = scratch("managed_refusals");
    let (bin, runtime) = (dir.join("bin"), dir.join("run"));
    let mut s = sup_ctx(agent_ctx(&bin, &runtime, dir.to_path_buf()));

    s.spawn_agent("vim", dir.to_path_buf(), None);
    let events = s.drain();
    assert!(!acknowledged(&events));
    assert_eq!(
        notices(&mut sup_with(events)),
        ["no agent named \"vim\", not spawning"]
    );
    s.spawn_agent("claude", dir.to_path_buf(), None);
    let events = s.drain();
    assert!(!acknowledged(&events));
    assert_eq!(
        notices(&mut sup_with(events)),
        ["claude not found on PATH, not spawning"]
    );
    assert!(s.tasks.is_empty(), "a refused launch creates nothing");
    assert!(!runtime.exists(), "a refused launch installs nothing");

    install_stub(&bin, "claude", &dir);
    s.set_max_tasks(1);
    s.spawn_agent("claude", dir.to_path_buf(), None);
    assert!(acknowledged(&s.drain()));
    assert_eq!(s.tasks.len(), 1);
    s.spawn_agent("claude", dir.to_path_buf(), None);
    assert_eq!(notices(&mut s), ["task limit reached (1), not spawning"]);
    spawn(&mut s, "sleep 1", dir.to_path_buf());
    assert_eq!(notices(&mut s), ["task limit reached (1), not spawning"]);
    assert_eq!(s.tasks.len(), 1, "the ceiling holds for both launch kinds");
}

/// A supervisor holding only `events`, so `notices` can filter them.
fn sup_with(events: Vec<Event>) -> Supervisor {
    let mut s = Supervisor::new(24, 80, 0);
    s.events = events;
    s
}

/// A managed rerun relaunches through the harness: it resumes the captured
/// ID ahead of the overlay, keeps id, tag, group, name and the program word,
/// bumps the run, and finds the binary on `PATH` again, so a binary removed
/// since the launch refuses the rerun and leaves the finished task alone.
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
        stamped(
            &s.tasks[0],
            &format!(
                r#"{{"session_id":"{CAP_OTHER}","hook_event_name":"SessionStart","source":"clear"}}"#
            ),
        ),
    )
    .unwrap();
    std::fs::remove_file(dir.join("argv")).unwrap();

    s.apply(Command::Restart { id });
    let argv = wait_argv(&mut s, &dir.join("argv"));
    assert_eq!(
        argv,
        ["--resume", CAP_OTHER, "--settings", &settings_beside(&s)],
        "the intent part leads, the overlay follows, no second pin"
    );
    let t = &s.tasks[0];
    assert!(t.managed, "a rerun keeps the task managed");
    assert_eq!((t.id, t.run, t.command.as_str()), (id, 1, "claude"));
    assert_eq!(t.resume_id.as_deref(), Some(CAP_OTHER));
    assert!(t.tagged);
    assert_eq!(t.group.as_deref(), Some("agents"));
    assert_eq!(t.name.as_deref(), Some("pilot"));

    // The binary is resolved again at each rerun: a removed one refuses.
    wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);
    std::fs::remove_file(bin.join("claude")).unwrap();
    s.apply(Command::Restart { id });
    assert_eq!(notices(&mut s), ["claude not found on PATH, not spawning"]);
    assert_eq!(s.tasks[0].run, 1, "the finished task is preserved");
}

/// Without any known ID, a managed rerun starts a fresh conversation: omp
/// pins nothing, so both runs carry the overlay alone.
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
    assert!(s.tasks[0].managed);
    assert_eq!(s.tasks[0].run, 1);
}

/// Session format v1 has no managed entry. A managed task saves as the bare
/// program word or its resume form, and that string loads as a detected
/// literal that resumes the same conversation.
#[test]
fn managed_task_saves_in_the_v1_resume_form_and_reloads_as_a_detected_literal() {
    let dir = scratch("managed_save");
    let (bin, runtime, config) = (dir.join("bin"), dir.join("run"), dir.join("config"));
    // One record directory per stub: the reloaded tasks launch together, and
    // a shared argv file would be truncated by one while the test reads the
    // other's.
    let (claude_out, omp_out) = (dir.join("claude-out"), dir.join("omp-out"));
    for out in [&claude_out, &omp_out] {
        std::fs::create_dir_all(out).unwrap();
    }
    install_stub(&bin, "claude", &claude_out);
    install_stub(&bin, "omp", &omp_out);
    let mut s = sup_ctx(agent_ctx_plus(
        &bin,
        &runtime,
        dir.to_path_buf(),
        &[("FLEETCOM_CONFIG_DIR", &config)],
    ));
    s.spawn_agent("claude", dir.to_path_buf(), Some("agents".into()));
    s.spawn_agent("omp", dir.to_path_buf(), None);
    assert!(s.tasks.iter().all(|t| t.managed));
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
                    cmd: format!("claude --resume '{CAP_ID}'"),
                    group: Some("agents".into()),
                    name: None,
                },
                SessionEntry {
                    cmd: "omp".into(),
                    group: None,
                    name: None,
                },
            ]
        )])
    );

    // The saved strings load as literal tasks that detection instruments.
    // Wait for both first-run records before clearing them, so a late write
    // from the original launch cannot pass for the reloaded one.
    wait_argv(&mut s, &claude_out.join("argv"));
    wait_argv(&mut s, &omp_out.join("argv"));
    std::fs::remove_file(claude_out.join("argv")).unwrap();
    std::fs::remove_file(omp_out.join("argv")).unwrap();
    s.apply(Command::LoadSession {
        name: "managed".into(),
    });
    assert_eq!(s.tasks.len(), 4);
    let reloaded = &s.tasks[2];
    assert!(!reloaded.managed, "v1 has no managed entry");
    assert_eq!(reloaded.command, format!("claude --resume '{CAP_ID}'"));
    assert!(reloaded.harness.is_some(), "detection instruments the form");
    assert_eq!(reloaded.resume_id.as_deref(), Some(CAP_ID));
    assert_eq!(reloaded.group.as_deref(), Some("agents"));
    assert!(!s.tasks[3].managed);
    assert_eq!(s.tasks[3].command, "omp");
    assert!(s.tasks[3].harness.is_some());
    let claude_argv = wait_argv(&mut s, &claude_out.join("argv"));
    assert!(
        claude_argv.starts_with(&["--resume".into(), CAP_ID.into()]),
        "the reloaded claude entry must resume its captured ID: {claude_argv:?}"
    );
    let omp_argv = wait_argv(&mut s, &omp_out.join("argv"));
    assert_eq!(
        omp_argv.first().map(String::as_str),
        Some("-e"),
        "the reloaded omp entry must load the capture extension: {omp_argv:?}"
    );
}

/// A managed codex carries the embedded override even when capture is off,
/// reports why, and leads a rerun with `resume <id>` ahead of the overrides.
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
    assert_eq!(
        notices(&mut s),
        ["task 1: codex capture unavailable: `notify` config can't be chained"]
    );
    let argv = wait_argv(&mut s, &dir.join("argv"));
    assert_eq!(argv, ["-c", "features.daemon_auto_start=false"]);
    let id = s.tasks[0].id;
    assert!(s.tasks[0].managed);
    assert!(s.tasks[0].resume_id.is_none(), "codex cannot pin an id");
    wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);

    // The v1 slot holds one bare root UUID.
    std::fs::write(s.tasks[0].capture_file.as_ref().unwrap(), CAP_ID).unwrap();
    std::fs::remove_file(dir.join("argv")).unwrap();
    s.apply(Command::Restart { id });
    assert_eq!(
        notices(&mut s),
        ["task 1: codex capture unavailable: `notify` config can't be chained"]
    );
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
    spawn(&mut s, "claude", dir.to_path_buf());
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
