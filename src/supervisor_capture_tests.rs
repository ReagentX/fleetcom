use super::*;
use crate::harness::fixtures::{ID as CAP_ID, OTHER as CAP_OTHER};

// --- session-capture wiring -------------------------------------------

/// Install an executable stub that records `FLEETCOM_CAPTURE_FILE` and
/// its argv, one token per line, then exits.
fn install_stub(bin: &Path, name: &str, out: &Path) {
    install_script(
        bin,
        name,
        &format!(
            "printf '%s' \"$FLEETCOM_CAPTURE_FILE\" > '{out}/capenv'\n\
             printf '%s\\n' \"$@\" > '{out}/argv'",
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

/// Save a recipe and return its persisted JSON.
fn save_and_read(s: &mut Supervisor, config: &Path, name: &str) -> String {
    s.apply(Command::SaveSession { name: name.into() });
    let _ = s.drain();
    std::fs::read_to_string(config.join("sessions").join(format!("{name}.json"))).unwrap()
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
    let mut s = sup_ctx(agent_ctx(&bin, &runtime, dir.clone()));
    spawn(&mut s, "claude", dir.clone());

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
    let _ = std::fs::remove_dir_all(&dir);
}

/// An unrecognized command spawns without capture state or assets.
#[test]
fn spawn_non_agent_command_is_not_instrumented() {
    let dir = scratch("cap_plain");
    let runtime = dir.join("run");
    let mut s = sup_ctx(agent_ctx(&dir.join("bin"), &runtime, dir.clone()));
    spawn(&mut s, "printf ok", dir.clone());
    let t = &s.tasks[0];
    assert!(t.harness.is_none());
    assert!(t.capture_file.is_none());
    assert!(t.resume_id.is_none());
    assert!(
        s.capture.is_empty(),
        "a non-agent spawn must not install capture assets"
    );
    assert!(!runtime.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A resuming `claude` launch retains its target ID and adds only the
/// capture overlay.
#[test]
fn spawn_resuming_claude_injects_only_the_capture_channel() {
    let dir = scratch("cap_resume");
    let (bin, runtime) = (dir.join("bin"), dir.join("run"));
    install_stub(&bin, "claude", &dir);
    let mut s = sup_ctx(agent_ctx(&bin, &runtime, dir.clone()));
    spawn(&mut s, format!("claude --resume {CAP_ID}"), dir.clone());

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
    let _ = std::fs::remove_dir_all(&dir);
}

/// Rerun prefers the capture-file ID, stores the resulting resume command,
/// and deletes the displaced run's capture after deriving the resume command.
#[test]
fn rerun_resumes_the_captured_conversation() {
    use crate::protocol::Lifecycle;
    let dir = scratch("cap_rerun");
    let (bin, runtime) = (dir.join("bin"), dir.join("run"));
    install_stub(&bin, "claude", &dir);
    let mut s = sup_ctx(agent_ctx(&bin, &runtime, dir.clone()));
    spawn(&mut s, "claude", dir.clone());
    let _ = wait_argv(&mut s, &dir.join("argv"));
    let id = s.tasks[0].id;
    wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);

    // The capture payload reports a different ID from the pinned one.
    let cap = s.tasks[0].capture_file.clone().expect("capture file set");
    std::fs::write(
        &cap,
        format!(
            r#"{{"session_id":"{CAP_OTHER}","hook_event_name":"SessionStart","source":"clear"}}"#
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
    let _ = std::fs::remove_dir_all(&dir);
}

/// A rerun uses a new capture path, so the displaced run's payload and
/// later writes cannot affect the replacement.
#[test]
fn rerun_cannot_read_the_old_runs_stale_capture() {
    let dir = scratch("cap_stale_run");
    let (bin, runtime, config) = (dir.join("bin"), dir.join("run"), dir.join("config"));
    // The session drifts to CAP_ID mid-run and the exit hint reports it.
    install_script(
        &bin,
        "claude",
        &format!("printf 'Resume this session with:\\nclaude --resume {CAP_ID}\\n'"),
    );
    let mut s = sup_ctx(agent_ctx_plus(
        &bin,
        &runtime,
        dir.clone(),
        &[("FLEETCOM_CONFIG_DIR", &config)],
    ));
    spawn(&mut s, "claude", dir.clone());
    let id = s.tasks[0].id;
    // The capture file still holds the pre-drift session.
    let stale = format!(
        r#"{{"session_id":"{CAP_OTHER}","hook_event_name":"SessionStart","source":"startup"}}"#
    );
    let old_cap = s.tasks[0].capture_file.clone().expect("capture file set");
    std::fs::write(&old_cap, &stale).unwrap();
    assert!(reap_until(&mut s, Duration::from_secs(5), |s| s.tasks[0]
        .scraped_id
        .is_some()));
    assert_eq!(s.tasks[0].scraped_id.as_deref(), Some(CAP_ID));

    s.apply(Command::Restart { id });
    let new_cap = s.tasks[0].capture_file.clone().expect("capture file set");
    assert_ne!(new_cap, old_cap, "the fresh run needs its own capture file");
    assert_eq!(s.tasks[0].command, format!("claude --resume '{CAP_ID}'"));
    assert!(
        !old_cap.exists(),
        "rerun must delete the displaced run's capture file"
    );

    // A late hook write can recreate the old path, but the new run cannot read it.
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
    let _ = std::fs::remove_dir_all(&dir);
}

/// Removing a task also removes its capture file.
#[test]
fn remove_deletes_the_capture_file() {
    use crate::protocol::Lifecycle;
    let dir = scratch("cap_remove");
    let (bin, runtime) = (dir.join("bin"), dir.join("run"));
    install_stub(&bin, "claude", &dir);
    let mut s = sup_ctx(agent_ctx(&bin, &runtime, dir.clone()));
    spawn(&mut s, "claude", dir.clone());
    let _ = wait_argv(&mut s, &dir.join("argv"));
    let id = s.tasks[0].id;
    wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);
    let cap = s.tasks[0].capture_file.clone().unwrap();
    std::fs::write(&cap, "{}").unwrap();

    s.apply(Command::Remove { id });
    assert!(!cap.exists(), "Remove must delete the task's capture file");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Reconnecting with the active root reuses installed assets and preserves
/// live capture files.
#[test]
fn reconnect_with_unchanged_root_preserves_capture_files() {
    let dir = scratch("cap_reconnect");
    let (bin, runtime) = (dir.join("bin"), dir.join("run"));
    install_stub(&bin, "claude", &dir);
    let mut s = sup_ctx(agent_ctx(&bin, &runtime, dir.clone()));
    spawn(&mut s, "claude", dir.clone());
    let cap = s.tasks[0].capture_file.clone().expect("capture file set");
    std::fs::write(&cap, "{}").unwrap();

    // The client reconnects with an identical env and spawns again.
    s.set_launch_context(agent_ctx(&bin, &runtime, dir.clone()));
    spawn(&mut s, "claude", dir.clone());
    assert_eq!(s.tasks.len(), 2);
    assert!(
        cap.exists(),
        "an unchanged root must not disturb live capture files"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Returning to an installed root preserves its live capture files.
#[test]
fn returning_to_a_prior_root_preserves_its_live_captures() {
    let dir = scratch("cap_aba");
    let (bin, root_a, root_b) = (dir.join("bin"), dir.join("run-a"), dir.join("run-b"));
    install_stub(&bin, "claude", &dir);
    let mut s = Supervisor::new(24, 80, 2000);
    s.set_launch_context(agent_ctx(&bin, &root_a, dir.clone()));
    spawn(&mut s, "claude", dir.clone());
    let cap_a = s.tasks[0].capture_file.clone().expect("capture file set");
    std::fs::write(&cap_a, "{}").unwrap();

    // The client reconnects under root B, spawns, then returns to A and
    // spawns again.
    s.set_launch_context(agent_ctx(&bin, &root_b, dir.clone()));
    spawn(&mut s, "claude", dir.clone());
    s.set_launch_context(agent_ctx(&bin, &root_a, dir.clone()));
    spawn(&mut s, "claude", dir.clone());

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
    let _ = std::fs::remove_dir_all(&dir);
}

/// Remove deletes the capture file under the root the task spawned in,
/// not under whichever root the current client presents.
#[test]
fn remove_deletes_the_capture_file_under_the_spawn_root() {
    use crate::protocol::Lifecycle;
    let dir = scratch("cap_remove_cross");
    let (bin, root_a, root_b) = (dir.join("bin"), dir.join("run-a"), dir.join("run-b"));
    install_stub(&bin, "claude", &dir);
    let mut s = Supervisor::new(24, 80, 2000);
    s.set_launch_context(agent_ctx(&bin, &root_a, dir.clone()));
    spawn(&mut s, "claude", dir.clone());
    let _ = wait_argv(&mut s, &dir.join("argv"));
    let id = s.tasks[0].id;
    wait_for_lifecycle(&mut s, id, |l| l == Lifecycle::Ok);
    let cap = s.tasks[0].capture_file.clone().unwrap();
    std::fs::write(&cap, "{}").unwrap();

    // Root B is installed by a newer spawn; a same-id file under it must
    // survive the A task's removal.
    s.set_launch_context(agent_ctx(&bin, &root_b, dir.clone()));
    spawn(&mut s, "claude", dir.clone());
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
    let _ = std::fs::remove_dir_all(&dir);
}

/// A `codex` spawn receives a `notify=[...]` override naming an executable
/// capture script.
#[test]
fn spawn_codex_installs_the_notify_override() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("cap_codex");
    let (bin, runtime) = (dir.join("bin"), dir.join("run"));
    install_stub(&bin, "codex", &dir);
    // Keep config lookup within this test's scratch directory.
    let mut s = sup_ctx(agent_ctx_plus(
        &bin,
        &runtime,
        dir.clone(),
        &[("CODEX_HOME", &dir.join("codex_home"))],
    ));
    spawn(&mut s, "codex", dir.clone());

    let argv = wait_argv(&mut s, &dir.join("argv"));
    let ci = argv
        .iter()
        .position(|a| a == "-c")
        .expect("the stub must receive -c");
    let script = argv[ci + 1]
        .strip_prefix("notify=[\"")
        .and_then(|t| t.strip_suffix("\"]"))
        .unwrap_or_else(|| panic!("malformed notify override: {:?}", argv[ci + 1]));
    let meta = std::fs::metadata(script).expect("the notify program must exist");
    assert!(
        meta.permissions().mode() & 0o111 != 0,
        "codex execs the notify program directly; it must be executable"
    );
    let t = &s.tasks[0];
    assert_eq!(t.command, "codex");
    assert!(t.resume_id.is_none(), "codex cannot pin an id at launch");
    assert!(t.capture_file.is_some());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A `grok` spawn receives exactly the pinned ID: no settings overlay,
/// no config override, and no capture environment (grok has no
/// injectable live channel). The saved recipe resumes the pinned ID.
#[test]
fn spawn_grok_pins_an_id_and_injects_nothing_else() {
    let dir = scratch("cap_grok");
    let (bin, runtime, config) = (dir.join("bin"), dir.join("run"), dir.join("config"));
    install_stub(&bin, "grok", &dir);
    // Keep save-time correlation inside the scratch tree.
    let mut s = sup_ctx(agent_ctx_plus(
        &bin,
        &runtime,
        dir.clone(),
        &[
            ("FLEETCOM_CONFIG_DIR", &config),
            ("GROK_HOME", &dir.join("grok_home")),
        ],
    ));
    spawn(&mut s, "grok", dir.clone());

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
    let _ = std::fs::remove_dir_all(&dir);
}

/// A `claude` exit hint becomes the session ID used by the saved recipe.
#[test]
fn exit_hint_is_scraped_and_saved_as_a_resume() {
    let dir = scratch("scrape_exit");
    let (bin, runtime, config) = (dir.join("bin"), dir.join("run"), dir.join("config"));
    install_script(
        &bin,
        "claude",
        &format!("printf 'Resume this session with:\\nclaude --resume {CAP_ID}\\n'"),
    );
    let mut s = sup_ctx(agent_ctx_plus(
        &bin,
        &runtime,
        dir.clone(),
        &[("FLEETCOM_CONFIG_DIR", &config)],
    ));
    spawn(&mut s, "claude", dir.clone());
    // No pre-exit synchronization: the scrape's reader-EOF gate means
    // reap can run against the exiting stub at any point and the hint
    // still lands.
    assert!(reap_until(&mut s, Duration::from_secs(5), |s| s.tasks[0]
        .scraped_id
        .is_some()));
    assert_eq!(s.tasks[0].scraped_id.as_deref(), Some(CAP_ID));

    let text = save_and_read(&mut s, &config, "hint");
    assert!(
        text.contains(&format!("claude --resume '{CAP_ID}'")),
        "the recipe must resume the scraped session; got {text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Saving between process exit and the next reap tick still captures the
/// exit hint because `save_session` performs its own ready scrape.
#[test]
fn save_scrapes_a_finished_task_without_reap() {
    let dir = scratch("save_sync_scrape");
    let (bin, runtime, config) = (dir.join("bin"), dir.join("run"), dir.join("config"));
    install_script(
        &bin,
        "claude",
        &format!("printf 'Resume this session with:\\nclaude --resume {CAP_ID}\\n'"),
    );
    let mut s = sup_ctx(agent_ctx_plus(
        &bin,
        &runtime,
        dir.clone(),
        &[("FLEETCOM_CONFIG_DIR", &config)],
    ));
    spawn(&mut s, "claude", dir.clone());

    // Wait out only the residual reader-drain race: after EOF the sole
    // remaining gate is the exit latch, which save's own pass must flip.
    assert!(
        wait_until(Duration::from_secs(5), || s.tasks[0].reader_done()),
        "the stub never reached EOF"
    );
    assert!(s.tasks[0].finished.is_none(), "no reap may have run yet");

    let text = save_and_read(&mut s, &config, "syncsave");
    assert!(
        text.contains(&format!("claude --resume '{CAP_ID}'")),
        "save must scrape the finished task itself; got {text}"
    );
    assert_eq!(s.tasks[0].scraped_id.as_deref(), Some(CAP_ID));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Rerunning between process exit and the next reap tick latches the exit,
/// scrapes the hint, and resumes that session.
#[test]
fn rerun_scrapes_a_finished_task_without_reap() {
    let dir = scratch("rerun_sync_scrape");
    let (bin, runtime) = (dir.join("bin"), dir.join("run"));
    install_script(
        &bin,
        "claude",
        &format!("printf 'Resume this session with:\\nclaude --resume {CAP_ID}\\n'"),
    );
    let mut s = sup_ctx(agent_ctx(&bin, &runtime, dir.clone()));
    spawn(&mut s, "claude", dir.clone());
    let id = s.tasks[0].id;

    assert!(
        wait_until(Duration::from_secs(5), || s.tasks[0].reader_done()),
        "the stub never reached EOF"
    );
    assert!(s.tasks[0].finished.is_none(), "no reap may have run yet");

    s.apply(Command::Restart { id });
    assert_eq!(
        s.tasks[0].command,
        format!("claude --resume '{CAP_ID}'"),
        "rerun must compute its resume command from the exit scrape"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Session-ID precedence is exit scrape, capture file, then spawn-time ID.
#[test]
fn resume_id_precedence_scrape_over_capture_over_spawn() {
    let dir = scratch("precedence");
    let (bin, runtime, config) = (dir.join("bin"), dir.join("run"), dir.join("config"));
    let (hinted, done) = (dir.join("hinted"), dir.join("done"));
    install_script(
        &bin,
        "claude",
        &format!(
            "until [ -e '{h}' ]; do sleep 0.05; done\n\
                 printf 'Resume this session with:\\nclaude --resume {CAP_ID}\\n'\n\
                 until [ -e '{d}' ]; do sleep 0.05; done",
            h = hinted.display(),
            d = done.display()
        ),
    );
    let mut s = sup_ctx(agent_ctx_plus(
        &bin,
        &runtime,
        dir.clone(),
        &[("FLEETCOM_CONFIG_DIR", &config)],
    ));
    spawn(&mut s, "claude", dir.clone());
    let injected = s.tasks[0]
        .resume_id
        .clone()
        .expect("a fresh claude launch pins an id");
    assert_ne!(injected.as_str(), CAP_OTHER);

    // The hook moved the session mid-run: pre-exit, the capture file
    // must beat the injected id.
    let cap = s.tasks[0].capture_file.clone().expect("capture file set");
    std::fs::write(
        &cap,
        format!(
            r#"{{"session_id":"{CAP_OTHER}","hook_event_name":"SessionStart","source":"clear"}}"#
        ),
    )
    .unwrap();
    let text = save_and_read(&mut s, &config, "mid");
    assert!(
        text.contains(&format!("claude --resume '{CAP_OTHER}'")),
        "pre-exit the capture file must beat the injected id; got {text}"
    );

    // Print the hint and let the task exit: post-exit, the scrape must
    // beat the capture file.
    std::fs::write(&hinted, b"").unwrap();
    std::fs::write(&done, b"").unwrap();
    assert!(reap_until(&mut s, Duration::from_secs(5), |s| s.tasks[0]
        .scraped_id
        .is_some()));
    assert_eq!(s.tasks[0].scraped_id.as_deref(), Some(CAP_ID));
    let text = save_and_read(&mut s, &config, "post");
    assert!(
        text.contains(&format!("claude --resume '{CAP_ID}'")),
        "post-exit the scraped hint must beat the capture file; got {text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A silent Codex task falls back to one matching rollout under
/// `CODEX_HOME` when live channels produce no ID.
#[test]
fn save_falls_back_to_fs_correlation_for_a_silent_codex() {
    let dir = scratch("correlate_save");
    let (bin, runtime, config) = (dir.join("bin"), dir.join("run"), dir.join("config"));
    let codex_home = dir.join("codex_home");
    install_stub(&bin, "codex", &dir);
    // Create a rollout with a current v7 instant and the task's cwd.
    let now_ms = now_ms();
    let id = write_rollout(&codex_home, now_ms, 1, &dir);

    let mut s = sup_ctx(agent_ctx_plus(
        &bin,
        &runtime,
        dir.clone(),
        &[
            ("FLEETCOM_CONFIG_DIR", &config),
            ("CODEX_HOME", &codex_home),
        ],
    ));
    spawn(&mut s, "codex", dir.clone());
    assert!(reap_until(&mut s, Duration::from_secs(5), |s| s.tasks[0]
        .finished
        .is_some()));
    assert!(s.tasks[0].scraped_id.is_none(), "a silent exit has no hint");
    assert!(
        current_resume_id(&s.tasks[0]).is_none(),
        "no capture channel fired"
    );

    let text = save_and_read(&mut s, &config, "corr");
    assert!(
        text.contains(&format!("codex resume '{id}'")),
        "save must fall back to filesystem correlation; got {text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Save-time correlation uses the task's spawn-time `CODEX_HOME`, even
/// after a reconnect supplies another home containing a matching rollout.
#[test]
fn save_correlates_against_the_spawn_time_home() {
    let dir = scratch("correlate_home");
    let (bin, runtime, config) = (dir.join("bin"), dir.join("run"), dir.join("config"));
    let (home_a, home_b) = (dir.join("codex_a"), dir.join("codex_b"));
    install_stub(&bin, "codex", &dir);
    let now_ms = now_ms();
    // One unique in-window rollout per store, both naming the task cwd.
    let id_a = write_rollout(&home_a, now_ms, 1, &dir);
    let id_b = write_rollout(&home_b, now_ms, 2, &dir);

    let mut s = sup_ctx(agent_ctx_plus(
        &bin,
        &runtime,
        dir.clone(),
        &[("FLEETCOM_CONFIG_DIR", &config), ("CODEX_HOME", &home_a)],
    ));
    spawn(&mut s, "codex", dir.clone());
    assert!(reap_until(&mut s, Duration::from_secs(5), |s| s.tasks[0]
        .finished
        .is_some()));

    // Reconnect under home B, then save.
    s.set_launch_context(agent_ctx_plus(
        &bin,
        &runtime,
        dir.clone(),
        &[("FLEETCOM_CONFIG_DIR", &config), ("CODEX_HOME", &home_b)],
    ));
    let text = save_and_read(&mut s, &config, "homepin");
    assert!(
        text.contains(&format!("codex resume '{id_a}'")),
        "the recipe must resolve from the launch-time store; got {text}"
    );
    assert!(
        !text.contains(&id_b),
        "the reconnect store's decoy must not correlate; got {text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Home resolution order: the tool's own var, then the launch env's HOME
/// joined with the tool's dot directory, then nothing.
#[test]
fn harness_home_prefers_the_tool_var_then_home() {
    use crate::harness::{Claude, Codex, Grok};
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
    assert_eq!(
        harness_home(&env, &Grok).as_deref(),
        Some(Path::new("/h/.grok"))
    );
    assert_eq!(harness_home(&[], &Codex), None);
}

/// With only `HOME` in the launch environment, both notify routing and
/// save-time correlation resolve through `<home>/.codex`.
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
    // A multi-line notify is Opaque: injection is suppressed only when
    // the guard reads the client's config.toml through HOME.
    std::fs::write(
        codex_home.join("config.toml"),
        "notify = [\n  \"/my/thing\",\n]\n",
    )
    .unwrap();
    install_stub(&bin, "codex", &dir);
    // A unique in-window rollout in the same tree for save-time
    // correlation: with injection suppressed, no capture channel fires.
    let now_ms = now_ms();
    let id = write_rollout(&codex_home, now_ms, 1, &dir);

    let mut s = sup_ctx(agent_ctx_plus(
        &bin,
        &runtime,
        dir.clone(),
        &[("FLEETCOM_CONFIG_DIR", &config), ("HOME", &home)],
    ));
    spawn(&mut s, "codex", dir.clone());
    assert_eq!(
        s.tasks[0].harness_home.as_deref(),
        Some(codex_home.as_path()),
        "HOME alone must resolve the harness home"
    );
    let argv = wait_argv(&mut s, &dir.join("argv"));
    assert!(
        !argv.iter().any(|a| a.contains("notify=")),
        "the guard must read <home>/.codex/config.toml; argv: {argv:?}"
    );
    assert!(reap_until(&mut s, Duration::from_secs(5), |s| s.tasks[0]
        .finished
        .is_some()));

    let text = save_and_read(&mut s, &config, "homeonly");
    assert!(
        text.contains(&format!("codex resume '{id}'")),
        "correlation must read <home>/.codex; got {text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// With no configured notifier, instrumentation clears an inherited
/// `FLEETCOM_NOTIFY_CHAIN` so the capture script cannot execute it.
#[test]
fn stale_inherited_notify_chain_is_never_executed() {
    let dir = scratch("stale_chain");
    let (bin, runtime) = (dir.join("bin"), dir.join("run"));
    // No config.toml exists: nothing routed, so nothing may be chained.
    let codex_home = dir.join("codex_home");
    let stale = dir.join("stale");
    let record = dir.join("stale-record");
    write_executable(&stale, &format!("touch '{}'", record.display()));

    // The stub invokes the injected notify script the way codex would.
    let payload = format!(r#"{{"type":"agent-turn-complete","thread-id":"{CAP_ID}"}}"#);
    // The notify script sits beside the capture file, in a namespace
    // whose nonce is unknowable before spawn: derive it from the env.
    install_script(
        &bin,
        "codex",
        &format!("\"${{FLEETCOM_CAPTURE_FILE%/*}}/codex-notify.sh\" '{payload}'"),
    );
    let mut s = Supervisor::new(24, 80, 2000);
    let mut ctx = agent_ctx_plus(&bin, &runtime, dir.clone(), &[("CODEX_HOME", &codex_home)]);
    ctx.env.push((
        crate::harness::NOTIFY_CHAIN_ENV.into(),
        stale.as_os_str().to_os_string(),
    ));
    s.set_launch_context(ctx);
    spawn(&mut s, "codex", dir.clone());
    assert!(reap_until(&mut s, Duration::from_secs(5), |s| s.tasks[0]
        .finished
        .is_some()));
    let cap = s.tasks[0].capture_file.clone().expect("capture file set");
    assert_eq!(
        std::fs::read_to_string(&cap).unwrap(),
        payload,
        "the capture write must land before the script exits"
    );
    assert!(
        !record.exists(),
        "the stale inherited chain must not execute"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Without a live or filesystem ID, an agent recipe retains the original
/// command.
#[test]
fn agent_save_without_any_id_keeps_the_plain_command() {
    let dir = scratch("no_id");
    let (bin, runtime, config) = (dir.join("bin"), dir.join("run"), dir.join("config"));
    // CODEX_HOME names a store that never exists: correlation has
    // nothing to find, and the notify routing nothing to read.
    let codex_home = dir.join("codex_home");
    install_stub(&bin, "codex", &dir);
    let mut s = sup_ctx(agent_ctx_plus(
        &bin,
        &runtime,
        dir.clone(),
        &[
            ("FLEETCOM_CONFIG_DIR", &config),
            ("CODEX_HOME", &codex_home),
        ],
    ));
    spawn(&mut s, "codex", dir.clone());
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
    let _ = std::fs::remove_dir_all(&dir);
}

/// A representable `notify` assignment runs through the injected notifier
/// after the capture write.
#[test]
fn config_toml_notify_chains_through_the_injected_script() {
    use crate::harness::NOTIFY_CHAIN_ENV;
    let dir = scratch("cfg_chain");
    let (bin, runtime) = (dir.join("bin"), dir.join("run"));
    let codex_home = dir.join("codex_home");
    std::fs::create_dir_all(&codex_home).unwrap();
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
    // script with notification JSON as the final argument.
    let payload = format!(r#"{{"type":"agent-turn-complete","thread-id":"{CAP_ID}"}}"#);
    install_script(
        &bin,
        "codex",
        &format!(
            "printf '%s\\n' \"$@\" > '{out}/argv'\n\
                 printf '%s' \"${chain}\" > '{out}/chainenv'\n\
                 \"${{FLEETCOM_CAPTURE_FILE%/*}}/codex-notify.sh\" '{payload}'",
            out = dir.display(),
            chain = NOTIFY_CHAIN_ENV,
        ),
    );
    let mut s = sup_ctx(agent_ctx_plus(
        &bin,
        &runtime,
        dir.clone(),
        &[("CODEX_HOME", &codex_home)],
    ));
    spawn(&mut s, "codex", dir.clone());
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
        "the chained notifier never wrote its complete record"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("chainenv")).unwrap(),
        format!("{}\nturn-ended", notifier.display()),
        "the child env must carry the displaced argv, newline-joined"
    );
    let cap = s.tasks[0].capture_file.clone().unwrap();
    assert_eq!(
        std::fs::read_to_string(&cap).unwrap(),
        payload,
        "the capture write must precede the chain handoff"
    );
    assert_eq!(
        std::fs::read_to_string(&record).unwrap(),
        format!("turn-ended\n{payload}\n"),
        "the notifier must receive its original args plus the payload"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// An unrepresentable `notify` value disables injection, while a commented
/// assignment defines no route and leaves injection enabled.
#[test]
fn unrepresentable_config_notify_suppresses_injection() {
    let dir = scratch("cfg_guard");
    let (bin, runtime) = (dir.join("bin"), dir.join("run"));
    let codex_home = dir.join("codex_home");
    std::fs::create_dir_all(&codex_home).unwrap();
    // A multi-line array is out of the line-based parser's reach.
    std::fs::write(
        codex_home.join("config.toml"),
        "notify = [\n  \"/my/thing\",\n]\n",
    )
    .unwrap();
    install_stub(&bin, "codex", &dir);
    let mut s = sup_ctx(agent_ctx_plus(
        &bin,
        &runtime,
        dir.clone(),
        &[("CODEX_HOME", &codex_home)],
    ));
    spawn(&mut s, "codex", dir.clone());
    let argv = wait_argv(&mut s, &dir.join("argv"));
    assert!(
        !argv.iter().any(|a| a.contains("notify=")),
        "fleetcom must not guess at an unparseable notify; argv: {argv:?}"
    );

    // The same route commented out is inert: the injection returns.
    std::fs::write(
        codex_home.join("config.toml"),
        "# notify = [\"/my/thing\"]\n",
    )
    .unwrap();
    std::fs::remove_file(dir.join("argv")).unwrap();
    spawn(&mut s, "codex", dir.clone());
    let argv = wait_argv(&mut s, &dir.join("argv"));
    assert!(
        argv.iter().any(|a| a.starts_with("notify=[")),
        "a commented notify must not suppress the injection; argv: {argv:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Non-agent commands remain plain string entries in persisted JSON.
#[test]
fn non_agent_entries_survive_save_as_plain_strings() {
    let dir = scratch("plain_save");
    let config = dir.join("config");
    let mut s = sup_ctx(config_ctx(&config, dir.clone(), &[("SHELL", "/bin/sh")]));
    spawn(&mut s, "sleep 30", dir.clone());
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
    let _ = std::fs::remove_dir_all(&dir);
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
        dir.clone(),
        &[("FLEETCOM_CONFIG_DIR", &config)],
    ));
    s.set_recovery_timing(Duration::from_millis(20), Duration::from_millis(100));
    spawn(&mut s, "claude", dir.clone());
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
        format!(
            r#"{{"session_id":"{CAP_OTHER}","hook_event_name":"SessionStart","source":"clear"}}"#
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
    let _ = std::fs::remove_dir_all(&dir);
}
