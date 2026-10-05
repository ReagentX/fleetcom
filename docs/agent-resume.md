# Agent session resume

Session files store launch commands, not process state. If you relaunch a bare `claude`, `codex`, `grok`, or `omp` command, you ordinarily start another conversation. To preserve that conversation, `fleetcom` captures a validated ID when one is available for an accepted command. It uses that ID to construct a canonical resume command when you save a session or rerun a finished task (`r`).

## Workflow

Start a supported agent without flags:

1. Press `n` and run `claude`, `codex`, `grok`, or `omp`. The task is listed under the command you typed. Only the string executed through `$SHELL -c` is instrumented; the requested command is still displayed for a direct spawn.
2. Work in it. Press `Enter` to attach; press `Ctrl-\` to return to the dashboard. Depending on the agent, an ID is pinned at launch and may be updated from a hook, notifier, extension, or matching live registry record.
3. Press `w`, enter a session name, and press `Enter`. A captured bare command is saved in canonical resume form, such as `claude --resume '<uuid>'`. Without a known ID, the authored command is preserved.
4. Run `fleetcom <session>`, or press `o` in the dashboard, to start new processes from the saved commands. Use a stored resume command to reopen the captured conversation.

Press `r` on a finished agent task to rerun it with the captured launch, hook, notifier, extension, or registry ID. A registry record is still eligible after exit if present. The task's ID, tag, group, and name are preserved. After a successful rewrite, the resume command is displayed in the row and stored as the launch recipe. The same string is written to a [saved session](sessions.md).

Capture is best-effort and narrow by design. Commands with prompts, extra flags, or shell syntax are treated as opaque and saved verbatim. Accepted commands with no available ID are also saved unchanged. In both cases, the original command is rerun on load.

## Accepted command boundary

The capture boundary is intentionally narrow. Only these forms are accepted:

- `claude`, `codex`, `grok`, or `omp`
- `claude --resume <uuid>`
- `codex resume <uuid>`
- `grok --resume <uuid>`
- `omp --resume <uuid>`

The program word may be a path such as `/usr/local/bin/claude` when its basename matches and the token contains no shell syntax. A resume UUID may be bare or single-quoted, but it must be the final argument.

Everything else is treated as opaque and executed, displayed, and saved verbatim. This includes prompts, flags, alternate resume spellings, subcommands, trailing arguments, and shell syntax. With this boundary, injected arguments cannot be bound to a different shell command than the one recognized during detection.

## Capture state and isolation

Hooks, notifiers, and extension modules are loaded by the agent rather than the supervisor, so they need stable paths. Assets are installed once per runtime root. An explicit `FLEETCOM_RUNTIME_DIR` is used as the root; otherwise, the platform runtime or cache directory is used, partitioned by session directory.

For each supervisor installation, a private mode-`0700` `<root>/<pid>-<nonce>` namespace is created with:

- `claude-settings.json`, mode `0600`
- `codex-notify.sh`, mode `0700`
- `omp-capture.js`, mode `0600`: imported as a module, so no executable bit is required
- `task-<id>-<run>.json` capture paths

The random nonce isolates concurrent supervisors and prevents PID reuse from selecting an existing namespace. Each rerun also gets a distinct run number, so a displaced process cannot overwrite the replacement run's capture file. Every other root entry is left unchanged during installation.

## Evidence sources

The `fleetcom` daemon owns the lifecycle of each agent process. A conversation that an agent CLI hands to its own background service is not the `fleetcom` task. For an accepted command, `fleetcom` therefore keeps the conversation in the task's process and accepts a captured ID only from that process.

The 2026-10-04 validation runs cited below ran on macOS; none ran on Linux.

### `claude`

Claude accepts an ID at launch. For a bare command, the harness therefore generates a v4 UUID and adds it with the settings overlay:

```text
--session-id '<uuid>' --settings '<namespace>/claude-settings.json'
```

Since a canonical resume command already specifies the conversation ID, the harness adds only `--settings`. The overlay layers two keys over your settings for the launched process alone; your settings files are not modified.

The first key, `"disableAgentView": true`, turns off agent view: `claude agents`, `--bg`, `/background`, and Claude's on-demand daemon. With agent view on, Claude can hand a conversation to that daemon, and the task's process no longer holds it. In an instrumented task, you therefore cannot background a conversation: `/background` is unavailable, Left arrow on an empty prompt does not park the conversation, and `/fork` runs as a sub-agent inside the task's process instead of a background session.

The second key installs a `SessionStart` hook:

```text
{ printf '%s\n' "$PPID"; cat; } > "$FLEETCOM_CAPTURE_FILE"
```

The first line of the capture file is the PID of the Claude process that ran the hook; the rest is the hook's JSON payload. The harness reads `session_id` from that payload only when the PID is the task leader's. A capture written by any other process is refused.

Claude session records are also available at `<claude-home>/sessions/<pid>.json`, one per session. The direct path for the task leader's PID is read.

A record is accepted only when its `kind` is `interactive` and its `pid`, `cwd`, and `startedAt` match the task. The PID must match the filename, the working directories must be identical or resolve to the same path, and the process start must fall within 30 seconds of the task spawn. Missing, malformed, or mismatched records are ignored. A matching record's `waiting` status is also mapped to the top tier of the [preview cascade](commands.md#peek); the preview is unchanged for other statuses.

Both the hook check and the registry read use the task leader's PID, so both require `$SHELL -c` to replace itself with Claude. When the shell is retained as task leader instead, the hook records Claude's PID, not the leader's, and no registry record is named for the leader: the capture is refused, no ID is read from the registry, and the spawn-time ID stands. A conversation selected after launch, including through `/clear`, is then not followed.

Live validation on 2026-10-04 used Claude Code 2.1.289. With agent view enabled, `/fork`, `/background`, and Left arrow on an empty prompt each caused a process other than the task to write the capture file. A rerun with such an ID exited 1 with "… is running in the background. Run `claude attach <id>` …". After Left arrow, the registry still named the original ID, and a rerun with it reopened a snapshot without the later turns. With `disableAgentView` in the overlay, those handoffs were absent, `/fork <directive>` ran in-process, and the capture file, the registry, and the spawn-time ID agreed. The hook recorded the task's PID at startup and again after `/clear`.

### `codex`

You cannot choose a Codex ID at launch. Two overrides are injected into both accepted forms:

```text
-c 'notify=["<namespace>/codex-notify.sh"]' -c 'features.daemon_auto_start=false'
```

Since Codex 0.157.0, a plain `codex` attaches to a shared background server, and the notify override alone forces embedded mode and raises a startup warning. `features.daemon_auto_start=false` makes embedded mode explicit and removes the warning.

Embedded mode is deliberate: the conversation stays in the task's process. An instrumented task never uses the shared server, so it does not appear in `codex agents`, and `codex queue` cannot reach it.

After each turn, the notifier writes the `agent-turn-complete` JSON argument to `FLEETCOM_CAPTURE_FILE`. Several threads notify from one Codex process: the conversation on screen, each sub-agent it spawns, and a hidden thread that the TUI starts to title a new session. Only the first is the task's conversation, so the harness does not use the notified `thread-id` as written. It finds that thread's rollout under `<codex-home>/sessions/<YYYY>/<MM>/<DD>/`, reads the first line, and uses the root thread of that session tree: the thread itself when it is the root, or the root named in a sub-agent's header. This captures an in-TUI session change after a turn completes in the resumed conversation.

A thread is refused unless exactly one rollout is named for it and that rollout's header reads as a root's or as a sub-agent's that names its root. The title thread has no rollout, so it is always refused. A refused capture contributes no ID, so the spawn-time ID or the authored command stands.

This lookup does not scan the store to infer ownership. The task's own notifier names one thread; the harness lists the dated directories to find that thread's rollout and reads only its first line.

To preserve a configured notifier, read bare top-level keys in `$CODEX_HOME/config.toml` up to the first table header. For a one-line `notify` array of non-empty basic strings, invoke the notifier after writing the capture file. Store its argv in `FLEETCOM_NOTIFY_CHAIN`, joined by newlines, and append the notification payload before invocation. With an absent setting or empty array, run only the capture hook.

Do not inject the capture hook for empty arguments, newlines, or NUL: these cannot be represented in the chain encoding. Also skip injection when `config.toml` exists but cannot be read, or when a line above the first table header is not a bare key assigned a recognized single-line value. Neither override is added in these cases: no ID is captured, and Codex selects its own mode.

Live validation on 2026-09-28 used the official macOS arm64 Codex 0.157.1 and 0.158.0 executables with isolated homes and a scripted loopback Responses server. In both releases, the real capture script preserved the notification JSON and passed it to the configured notifier. Saving and loading through foreground fleetcom preserved the thread ID and reopened its history. These runs used `--no-daemon`; separate default-launch probes also captured and saved the ID, but their fresh homes had no managed Codex daemon installed or running. Real-account behavior was not verified in these runs.

The [recorded terminal fixtures](../tests/corpus/codex_0158_terminal/README.md) preserve resumed-history and clipboard output from those runs. Offline replay checks the recorded display and OSC 52 forwarding; it does not run the notifier or execute save/load. The separate `tests/daemon_resume.rs` integration tests exercise session resume with stub agents.

Live validation on 2026-10-04 used codex-cli 0.160.0. One Codex process notified three thread IDs: the title thread, which has no rollout and for which `codex resume` reports "No saved session found"; a sub-agent, for which `codex resume` reports "cannot resume an unloaded multi-agent v2 sub-agent through its parent"; and the root. With the notify override alone, the footer showed `⚠ 1 warning`; with both overrides, it showed none. Given an unknown `features.*` key, 0.160.0 added an unrecognized-setting warning and still started; no Codex that predates `daemon_auto_start` was tested. On the same date, 493 local rollout headers written by Codex 0.135.0 through 0.160.0 were examined: each carries `id` and `session_id`. Codex 0.140.0 and 0.141.0 wrote a sub-agent's own ID as its `session_id`; that shape is refused.

### `grok`

You can specify a Grok ID at launch, but cannot inject a live-capture channel. For a bare command, `--session-id '<uuid>'` is added; no instrumentation is required for a canonical resume command.

### `omp`

You cannot pin an omp ID at launch: no `--session-id` flag is available, and an existing session is required for `--resume`. The same injection is therefore added to both accepted forms, without a pinned ID:

```text
-e '<namespace>/omp-capture.js'
```

The `-e` flag loads the JavaScript module into the agent process, appending it to the user's extensions. Its `session_start`, `session_switch`, `session_branch`, and `agent_end` handlers write `sessionId` as JSON to `FLEETCOM_CAPTURE_FILE`, including after in-TUI `/resume` changes. A handler reports only when `omp --resume` can open the ID:

- The session is the top-level one: `ctx.agent.kind` is `"main"`. omp binds the same handlers to each sub-agent session, and `omp --resume` cannot open a sub-agent's ID.
- The session file exists on disk. omp creates it after the first assistant message.

`agent_end` fires at the end of every turn. It reports two changes that raise no event of their own: the session file's creation and, since omp 18.5.0, a move to a new ID when another live process holds the session's lease.

Three consequences follow:

- A bare `omp` task has no captured ID until its first turn ends. Save before then, and the authored `omp` is kept.
- An omp older than 18.3.2 has no `ctx.agent`, so its session is never captured: a bare `omp` there always saves as authored.
- After a switch or branch to a session whose file does not exist yet, the previous ID remains until the next turn ends.

Each report is written to a temporary file beside the capture file and renamed over it, so a reader never sees a partial payload. Capture writes are best-effort: the handlers return immediately for an empty capture path and ignore write errors.

Live validation on 2026-10-04 used omp 18.6.1 with a local model. Nothing was written before the first prompt, and the first `agent_end` wrote the top-level session's ID with its session file on disk. omp invoked the handlers for a sub-agent with the sub-agent's own ID, at its `session_start` and again at its `agent_end`; the capture stayed on the top-level ID. `/fork` reported the new ID through `session_switch`. A rewind through `/branch` kept the session's ID and raised no event. A second process that resumed a session the first still held moved to a new ID at its first write, and its next `agent_end` reported that ID. No `session_branch` event was observed: its handling is read from the oh-my-pi source at `v18.6.1`.

The aliases `-r`, `--session`, and `-c` remain opaque because only the exactly detected canonical form is rewritten.

## ID precedence

Different conversation IDs may be available from different channels during one task. The first available ID is selected in this order:

1. The current capture-file payload, when the harness accepts it.
2. The live session registry, implemented by `claude`.
3. The ID pinned or targeted at spawn.

A capture the harness refuses contributes nothing: the next source decides. The same precedence is used for named saves, recovery snapshots, and reruns. Session stores are not scanned to infer conversation ownership: you cannot determine the owning task from a nearby transcript or rollout.

Session IDs are never read from terminal output: another conversation's valid UUID may be present in examples, quoted commands, or tool output. An ID available only in an exit hint is not recovered. Without a capture, registry, or launch ID, the authored command remains unchanged.

A registry ID is preferred over the spawn pin because it may have been selected after launch, including through `/clear`. A capture-file ID is preferred over the registry ID.

On save and rerun, accepted commands are rewritten to one of these forms:

```text
claude --resume '<uuid>'
codex resume '<uuid>'
grok --resume '<uuid>'
omp --resume '<uuid>'
```

The program word is preserved as typed. If no valid ID is available, the original command remains unchanged. On rerun, the run number is incremented before the replacement is spawned, isolating it from capture data for the displaced run.

## Validation boundary

Every captured value eventually enters a shell command, so validation accepts only lowercase hexadecimal characters in the `8-4-4-4-12` UUID shape. The same check applies to capture payloads, Codex rollout headers, registry records, and final command construction. Malformed values are ignored rather than interpolated. A valid UUID alone does not establish conversation ownership.

## Extending capture

Implement the `Harness` trait in [`src/harness/mod.rs`](../src/harness/mod.rs) for each tool. Register it in that file's `AGENTS` table, the sole registry for detection. Include a `SummaryAdapter` for the dashboard preview with each entry; see the implementations in [`src/harness/summary.rs`](../src/harness/summary.rs). Separate detection, evidence collection, and command construction through these methods:

- `shape`: supply the program word and resume selector. Accepted and canonical forms are derived from that pair by the default `detect` and `resume_command` implementations.
- `instrument`: return spawn-time arguments, environment entries, and an optional pinned ID.
- `parse_capture`: read an ID from the capture file's contents. The task leader's PID and the launch-time home are supplied; when other processes can reach the capture channel, use them to refuse a payload the task's own process did not write.
- `live_session_id`: read the ID published on disk for a live session. Return `None` by default when no registry is available.
- `live_blocked_status`: read that registry for blocked-on-user status. Return preview text, never an ID; return `None` by default.
- `resolve_home`: resolve configuration needed by instrumentation, capture parsing, or the live registry. Return `None` by default.

The launch environment is passed to `resolve_home` by the supervisor. For Claude and Codex, the explicit override is read first, then `$HOME` plus the tool's dot directory. With neither supplied, the supervisor's platform home is used. The resolved path is stored with the task and used for Claude registry reads and Codex rollout lookups after reconnect, preserving the launch-time home. For Grok and omp, no home resolution is required; the environment is passed to the child unchanged.

## Environment variables

| Variable | Meaning |
| -- | -- |
| `FLEETCOM_RUNTIME_DIR` | Explicit capture-asset root as well as the daemon runtime override. |
| `FLEETCOM_CAPTURE_FILE` | Per-run capture file used by the injected hook, notifier, or extension module. |
| `FLEETCOM_NOTIFY_CHAIN` | Newline-joined argv for the configured Codex notifier; empty when none is active. |
| `CLAUDE_CONFIG_DIR` | Claude home holding the `sessions/<pid>.json` registry; defaults to `$HOME/.claude`. |
| `CODEX_HOME` | Codex home holding `config.toml` for notify routing and the `sessions/` rollouts; defaults to `$HOME/.codex`. |
