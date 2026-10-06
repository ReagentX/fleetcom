# Agent session resume

To reopen an agent conversation from a saved recipe, you need its session ID. No process state is stored in session files, and without an ID, a relaunched agent ordinarily starts fresh. Launch a managed task from the spawn prompt's Agent page to enable ID capture. The validated ID is then used for named saves, recovery snapshots, and reruns (`r`).

## Two kinds of task

The task kind is determined by how you start it, not by the command text:

- **Literal.** Type a command on the Command page or load a literal session entry to run its text unchanged under `$SHELL -c`. Even a bare `claude` is run without instrumentation, capture, registry reads, or automatic resume. The same text is used on rerun and save. Screen previews are still available through the display-only summary adapter selected by the command's first word.
- **Managed.** Pick an agent on the Agent page (`n`, then `Tab`) or load a managed session entry. The binary is resolved on the connected client's `PATH` and executed directly without a shell. Launch argv and capture are configured by `fleetcom`; the captured ID is used to reopen the conversation on rerun and reload. The program word is displayed in the task row.

There are no settings for managed launches. To supply flags, type a literal command and run without management.

## Workflow

1. Press `n`, or `@` to pick a directory first, then `Tab` for the Agent page. Choose among the supported agents found on the launch `PATH`: `claude`, `codex`, `grok`, `omp`. Select one and press `Enter`.
2. Work in it. Press `Enter` to attach; press `Ctrl-\` to return to the dashboard. Depending on the agent, an ID is pinned at launch and may be updated from a hook, notifier, extension, or matching live registry record.
3. Press `w`, enter a session name, and press `Enter` to save the task as a managed entry with the agent word and current session ID; see [Sessions](sessions.md#format).
4. Run `fleetcom <session>`, or press `o` in the dashboard, to start new processes from the recipe. For managed entries, the agents are relaunched with the stored resume IDs.

Press `r` on a finished managed task to rerun it with its best-known ID: capture, registry, then launch ID. A registry record is still eligible after exit if present. Without any ID (an `omp` saved before its first turn or a `codex` whose notifier could not be chained), you start a fresh conversation on rerun or reload. The task's ID, tag, group, and name are preserved.

## Process identity

A managed agent is the task leader because its binary is executed directly, without a shell. Claude capture and registry records are validated against that leader PID. This works regardless of your login shell, including csh and tcsh, under which the shell would remain the leader when using `$SHELL -c`. For a shebang script, the interpreter runs in the same process, preserving the PID. For a wrapper that starts the real binary as a child, the wrapper is the leader instead, just as with a resident shell.

Managed agents are launched with the connected client's environment, sent at connect and reconnect. That environment was captured after sourcing the client's shell startup files. To apply a dotfile edit, start a client from a new shell. On rerun, the current launch context is used. This was verified on 2026-10-05 with marker exports in a scratch `ZDOTDIR`: in Claude Code 2.1.289 tool commands, the launch environment and `.zshenv` exports were available; in codex-cli 0.160.1, each command was run as `/bin/zsh -lc`. In those probes, no environment additions from an intermediate shell were lost.

## Capture state and isolation

Hooks, notifiers, and extension modules are loaded by the agent rather than the supervisor, so they need stable paths. Assets are installed once per runtime root. An explicit `FLEETCOM_RUNTIME_DIR` is used as the root; otherwise, the platform runtime or cache directory is used, partitioned by session directory.

For each supervisor installation, a private mode-`0700` `<root>/<pid>-<nonce>` namespace is created with:

- `claude-settings.json`, mode `0600`
- `codex-notify.sh`, mode `0700`
- `omp-capture.js`, mode `0600`: imported as a module, so no executable bit is required
- `task-<id>-<run>.json` capture paths

Use a random nonce to separate concurrent supervisors and avoid reusing a namespace after PID reuse. Use a distinct run number on each rerun so a displaced process cannot overwrite the replacement's capture file. Leave other root entries unchanged during installation. Refuse a managed launch if its assets cannot be installed: without the overlay, a `codex` conversation would run in the shared server.

## Evidence sources

Each managed agent process is supervised by the `fleetcom` daemon. A conversation handed off to the agent CLI's own background service is outside that task. Keep the conversation in the task's process and accept captured IDs only from that process.

All validation on 2026-10-04 was performed on macOS; Linux was not tested.

### `claude`

You can specify a Claude ID at launch. For a fresh managed launch, a v4 UUID is generated and passed before the settings overlay:

```text
--session-id <uuid> --settings <namespace>/claude-settings.json
```

To resume, pass `--resume <uuid>` followed by `--settings`. Two keys are layered over your settings for the launched process alone; your settings files are not modified.

With the first key, `"disableAgentView": true`, agent view is disabled: `claude agents`, `--bg`, `/background`, and Claude's on-demand daemon. With agent view enabled, a conversation can be handed off to that daemon, outside the task's process. In a managed task, you therefore cannot background a conversation: `/background` is unavailable, you cannot park the conversation by pressing Left arrow on an empty prompt, and with `/fork` you start a sub-agent inside the task's process instead of a background session.

A `SessionStart` hook is registered through the second key:

```text
{ printf '%s\n' "$PPID"; cat; } > "$FLEETCOM_CAPTURE_FILE"
```

The first line of the capture file is the parent Claude process's PID; the rest is the hook's JSON payload. Read `session_id` from that payload only when the PID is the task leader's. Reject captures written by any other process.

Claude session records are also available at `<claude-home>/sessions/<pid>.json`, one per session. The direct path for the task leader's PID is read.

A record is accepted only when its `kind` is `interactive` and its `pid`, `cwd`, and `startedAt` match the task. The PID must match the filename, the working directories must be identical or resolve to the same path, and the process start must fall within 30 seconds of the task spawn. Missing, malformed, or mismatched records are ignored. A matching record's `waiting` status is mapped to the top tier of the [preview cascade](commands.md#peek); the preview is unchanged for other statuses. For literal `claude` tasks, blocked status is determined from the screen alone, without this registry tier.

Validate hook captures and look up registry records using the task leader's PID. With direct execution in a managed launch, this is the Claude process. Reject other PID stamps and try the next source, so you can still follow a conversation selected after launch, including through `/clear`, through its registry record.

Live validation on 2026-10-04 was performed with Claude Code 2.1.289. With agent view enabled, after using `/fork`, `/background`, or Left arrow on an empty prompt, the capture file was written by a process other than the task. When rerunning with such an ID, the exit status was 1 with "… is running in the background. Run `claude attach <id>` …". After pressing Left arrow, the original ID was still in the registry; on rerun with that ID, a snapshot was reopened without the later turns. With `disableAgentView` in the overlay, no handoffs were observed. The sub-agent started through `/fork <directive>` was run in-process, and the same ID was present in the capture file, registry, and spawn-time metadata. The task's PID was stamped in the capture at startup and again after `/clear`.

### `codex`

You cannot choose a Codex ID at launch. Add these overrides for managed launches, after `resume <uuid>` when resuming; omit the notify override when capture is unavailable, as described below:

```text
-c 'notify=["<namespace>/codex-notify.sh"]' -c 'features.daemon_auto_start=false'
```

Since Codex 0.157.0, running plain `codex` attaches to a shared background server. With the notify override alone, embedded mode is used and a startup warning is displayed. Set `features.daemon_auto_start=false` to explicitly use embedded mode without the warning.

With embedded mode, the conversation is kept in the task's process. For a managed task, the shared server is never used: you cannot find the task in `codex agents` or reach it through `codex queue`. The embedded override is therefore added on every managed launch, including those where capture is unavailable: without it, killing the task would kill only the TUI client and leave the conversation running in the shared server.

After each turn, the notify script is invoked with `agent-turn-complete` JSON as its argument. Notifications are emitted for several threads in one Codex process: the conversation on screen, each spawned sub-agent, and a hidden title thread. Only the first is the task's conversation, so do not use the notified `thread-id` directly. Validate notifications at arrival: title notifications have been observed both before and after root notifications, and writing a later title payload verbatim would overwrite the accepted root.

Validate through `"$FLEETCOM_BINARY" --codex-notify-v1 "$1"`, an internal mode of the daemon's binary. Accept only an `agent-turn-complete` payload with a strict UUID `thread-id`. Resolve the Codex home from the launch environment (`CODEX_HOME`, then `$HOME/.codex`), find the notified thread's rollout under `<codex-home>/sessions/<YYYY>/<MM>/<DD>/`, and read its first line. Use the thread's own ID for a root, or the root ID recorded in a sub-agent's header. The rollout and header are already on disk at notification time. Accept the thread only when exactly one rollout is named for it and the header matches one of those formats. No rollout is saved for the title thread, so always reject it.

On acceptance, write the bare root UUID without a newline to `<capture>.<pid>.tmp` beside the capture file, then rename it over `FLEETCOM_CAPTURE_FILE` to prevent reads of a partial file. On refusal, preserve the slot without writing. Print nothing and ignore the informational exit status in the calling script. In-TUI session changes are captured after a completed turn in the resumed conversation.

Keep the v1 capture format fixed: exactly one bare UUID. On read, require a valid UUID across the entire contents, without trimming. Do not reparse the notification or read the rollout: the root remains accepted after rollout compression, and no conversation text is written to the capture file. Reject an older script's JSON payload and fall back to the launch ID, or no ID if none is known.

Look up only the thread ID from the task's own notification. List the dated directories to find its rollout, then read only the first line. Do not infer ownership from other sessions in the store.

Set `FLEETCOM_BINARY` to the daemon's executable and probe it at each `codex` spawn for an existing regular file with an execute bit. On Linux, the path read through `/proc/self/exe` ends in ` (deleted)` after the binary is replaced under a running daemon; reject that nonexistent path. On macOS, the same path refers to the newly installed binary. If replaced with an older release, the notify flag is unrecognized and no capture is written. In either case, continue running the chained notifier.

To preserve a configured notifier, read bare top-level keys in `$CODEX_HOME/config.toml` up to the first table header. For a one-line `notify` array of non-empty basic strings, invoke the notifier after validation. Store its argv in `FLEETCOM_NOTIFY_CHAIN`, joined by newlines, and append the notification payload before invocation. Exec the chain even if the validation binary is missing, refuses the payload, or crashes. With an absent setting or empty array, run only validation.

Inject the notify override and environment only when the configured route can be chained and the binary passes the probe. Do not inject them for empty arguments, newlines, or NUL, which cannot be represented in the chain encoding. Also skip them when `config.toml` exists but cannot be read, or when a line above the first table header is not a bare key assigned a recognized single-line value. In each case, launch with only the embedded override and no capture. Report the reason on the status line: `` `notify` config can't be chained ``, or `fleetcom binary replaced; restart the daemon`.

Live validation on 2026-09-28 was performed with the official macOS arm64 Codex 0.157.1 and 0.158.0 executables, isolated homes, and a scripted loopback Responses server. In both releases, the notification JSON was written by the capture script of that date, which copied it verbatim, and passed to the configured notifier. After saving and loading through foreground fleetcom, the same thread ID was present and its history was reopened. Both releases were tested with `--no-daemon`. The ID was also captured and saved in separate default-launch probes, but no managed Codex daemon was installed or running in those fresh homes. Real-account behavior was not verified.

The [recorded terminal fixtures](../tests/corpus/codex_0158_terminal/README.md) preserve resumed-history and clipboard output from those runs. Offline replay checks the recorded display and OSC 52 forwarding; it does not run the notifier or execute save/load. The separate `tests/daemon_resume.rs` integration tests exercise session resume with stub agents.

Live validation on 2026-10-04 was performed with codex-cli 0.160.0 over nine TUI sessions. Three thread IDs were reported from one process: the unsaved title thread, a sub-agent, and the root. The title thread's notification arrived before the root's in three sessions and after it in six, and no rollout appeared for it even minutes later. For a root or sub-agent turn, the thread's rollout header was on disk at least 2.5 s before its notification, and `task_complete` was recorded 21–256 ms before it. When resuming the title thread, "No saved session found" was reported; for the sub-agent, "cannot resume an unloaded multi-agent v2 sub-agent through its parent" was reported. With the notify override alone, `⚠ 1 warning` was displayed in the footer; with both overrides, none was displayed. With an unknown `features.*` key, startup completed with an unrecognized-setting warning. No Codex version predating `daemon_auto_start` was tested. On the same date, 493 local rollout headers from Codex 0.135.0 through 0.160.0 were examined: `id` and `session_id` were present in each. In Codex 0.140.0 and 0.141.0, a sub-agent's own ID was stored as its `session_id`; that format is rejected.

### `grok`

You can specify a Grok ID at launch, but cannot inject a live-capture channel. Pass `--session-id <uuid>` for a fresh launch or `--resume <uuid>` to resume. No overlay is added.

### `omp`

You cannot pin an omp ID at launch: no `--session-id` flag is available, and an existing session is required for `--resume`. Load the extension on every managed launch without pinning an ID. To resume, pass `--resume <uuid>` before the extension argument:

```text
-e <namespace>/omp-capture.js
```

Load the JavaScript module into the agent process through `-e`, appending it to your extensions. On `session_start`, `session_switch`, `session_branch`, and `agent_end`, write `sessionId` as JSON to `FLEETCOM_CAPTURE_FILE`, including after in-TUI `/resume` changes. Report only IDs usable with `omp --resume`:

- The session is the top-level one: `ctx.agent.kind` is `"main"`. The same handlers are registered for sub-agent sessions, but you cannot resume a sub-agent by ID.
- The session file exists on disk. It is created after the first assistant message.

At `agent_end`, after every turn, report two changes without dedicated events: session file creation and, since omp 18.5.0, assignment of a new ID when the session lease is held by another live process.

As a result:

- For a fresh `omp` task, no ID is captured until the end of the first turn. If you save before then, no ID is stored, so you start fresh on reload.
- Before omp 18.3.2, `ctx.agent` is unavailable, so no session ID is captured: an `omp` task is always saved without an ID.
- After switching or branching to a session without a file on disk, continue using the previous ID until the end of the next turn.

Write each report to a temporary file beside the capture file, then rename it over the destination to avoid reads of partial payloads. Capture is best-effort: return immediately for an empty capture path and ignore write errors.

Live validation on 2026-10-04 was performed with omp 18.6.1 and a local model. Nothing was written before the first prompt. At the first `agent_end`, the top-level session's ID was written, with its session file present on disk. The handlers were also invoked for a sub-agent at `session_start` and `agent_end`, with the sub-agent's own ID; the top-level ID was preserved in the capture. After `/fork`, the new ID was reported through `session_switch`. After a rewind through `/branch`, the session ID was unchanged and no event was emitted. When resuming a session already leased by another live process, a new ID was assigned at the first write and reported at the next `agent_end`. No `session_branch` event was observed; its handling was determined from the oh-my-pi source at `v18.6.1`.

## ID precedence

Different conversation IDs may be available from different channels during one managed task. The first available ID is selected in this order:

1. The current capture-file payload, if accepted by the harness.
2. The live session registry, implemented by `claude`.
3. The ID pinned or targeted at spawn.

If a capture is rejected, try the next source. Use the same precedence for named saves, recovery snapshots, and reruns. Do not infer conversation ownership from session stores: you cannot determine the owning task from a nearby transcript or rollout.

Never read session IDs from terminal output: another conversation's valid UUID may appear in examples, quoted commands, or tool output. Do not recover an ID from an exit hint. Without a capture, registry, or launch ID, save no ID and start fresh on the next launch.

A registry ID is preferred over the spawn pin because it may have been selected after launch, including through `/clear`. A capture-file ID is preferred over the registry ID.

Save a managed task as `{ "agent": "<word>", "resume": "<uuid>" }`. On rerun and reload, place the selector and ID first in argv:

```text
claude --resume <uuid>
codex resume <uuid>
grok --resume <uuid>
omp --resume <uuid>
```

Pass the ID as one argv element without shell parsing. Before spawning a replacement on rerun, increment the run number to isolate its capture from the displaced run.

## Validation boundary

Pass a captured ID as one agent argv element and store it in the session file's `resume` field. Before either use, validate it as lowercase hexadecimal in the `8-4-4-4-12` UUID format. Validate capture payloads, Codex rollout headers, registry records, and session files on load. Ignore malformed values from capture channels; refuse session files with malformed IDs. UUID validity alone does not establish conversation ownership.

## Extending capture

Implement `Harness` in [`src/harness/mod.rs`](../src/harness/mod.rs) for each tool. Register it in that file's `AGENTS` table for use in the Agent page, session loading, and summary selection. Include a `SummaryAdapter` for the dashboard preview with each entry; see [`src/harness/summary.rs`](../src/harness/summary.rs). Separate launch construction from evidence collection through these methods:

- `shape`: supply the program word and resume selector. Use the word to identify the tool on the Agent page and in session files. In `harness::intent_args`, place the selector and ID first in resume argv.
- `session_flag`: specify the flag for pinning a session ID on a fresh launch, or return `None` (the default) for tool-assigned IDs. In `harness::plan`, build the conversation-selection argv from this flag and the resume selector.
- `overlay`: return argv elements to append after conversation selection, environment entries, and any launch notice. Pass the elements directly to the binary and report the notice through the supervisor after a successful spawn.
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
| `FLEETCOM_BINARY` | Daemon executable used for `--codex-notify-v1` validation by the injected notifier; absent when the path is unusable. |
| `CLAUDE_CONFIG_DIR` | Claude home holding the `sessions/<pid>.json` registry; defaults to `$HOME/.claude`. |
| `CODEX_HOME` | Codex home holding `config.toml` for notify routing and the `sessions/` rollouts; defaults to `$HOME/.codex`. |
