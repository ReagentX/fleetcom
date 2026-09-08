# Agent session resume

Launch commands are stored in session files; process state is not. If you relaunch a bare `claude`, `codex`, `grok`, or `omp` command, you ordinarily start another conversation. For accepted commands, a validated conversation ID is captured when available and used to construct a canonical resume command on session save or finished-task rerun (`r`).

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

With a random nonce, concurrent supervisors are isolated and an existing namespace cannot be selected after PID reuse. With a distinct run number per rerun, capture files are separate and the replacement run's session state cannot be overwritten by a displaced process. Every other root entry is left unchanged during installation.

## Evidence sources

### `claude`

For a bare Claude command, an ID can be specified at launch. A v4 UUID is therefore generated and added with the settings overlay:

```text
--session-id '<uuid>' --settings '<namespace>/claude-settings.json'
```

In a canonical resume command, the conversation ID is already specified; only `--settings` is added. A `SessionStart` hook is installed through the overlay. Its JSON payload is copied into `FLEETCOM_CAPTURE_FILE`, then `session_id` is read by the harness.

Claude session records are also available at `<claude-home>/sessions/<pid>.json`, one per session. The direct path for the task leader's PID is read. When the shell is retained as task leader under `$SHELL -c` instead of being replaced with Claude, no matching record is available and no ID is read from the registry.

A record is accepted only when its `kind` is `interactive` and its `pid`, `cwd`, and `startedAt` match the task. The PID must match the filename, the working directories must be identical or resolve to the same path, and the process start must fall within 30 seconds of the task spawn. Missing, malformed, or mismatched records are ignored. A matching record's `waiting` status is also mapped to the top tier of the [preview cascade](commands.md#peek); the preview is unchanged for other statuses.

### `codex`

You cannot choose a Codex ID at launch. A notify override is injected into both accepted forms:

```text
-c 'notify=["<namespace>/codex-notify.sh"]'
```

After each turn, the `agent-turn-complete` JSON argument is written to `FLEETCOM_CAPTURE_FILE` by the notifier; `thread-id` is then read by the harness. In-TUI session changes are captured after a turn is completed in the resumed conversation.

Replacing a configured notifier would change user behavior. Bare top-level keys in `$CODEX_HOME/config.toml` are read until the first table header. A one-line `notify` array of non-empty basic strings is chained after the capture write. Its argv is carried in `FLEETCOM_NOTIFY_CHAIN`, joined by newlines, and the notification payload is appended. With an absent setting or empty array, capture is run alone. Empty arguments, newlines, and NUL cannot be transported; injection is disabled for these values.

### `grok`

You can specify a Grok ID at launch, but cannot inject a live-capture channel. For a bare command, `--session-id '<uuid>'` is added; no instrumentation is required for a canonical resume command.

### `omp`

You cannot pin an omp ID at launch: no `--session-id` flag is available, and an existing session is required for `--resume`. The same injection is therefore added to both accepted forms, without a pinned ID:

```text
-e '<namespace>/omp-capture.js'
```

Use `-e` to load the JavaScript module into the agent process, appended to the user's extensions. In its `session_start` and `session_switch` handlers, `sessionId` is written as JSON to `FLEETCOM_CAPTURE_FILE`, including after in-TUI `/resume` changes. Capture writes are best-effort: with an empty capture path, return immediately; ignore write errors.

The aliases `-r`, `--session`, and `-c` remain opaque because only the exactly detected canonical form is rewritten.

## ID precedence

Different conversation IDs may be available from different channels during one task. The first available ID is selected in this order:

1. The current capture-file payload.
2. The live session registry, implemented by `claude`.
3. The ID pinned or targeted at spawn.

The same precedence is used for named saves, recovery snapshots, and reruns. Session stores are not scanned to infer conversation ownership: you cannot determine the owning task from a nearby transcript or rollout.

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

Every captured value is eventually inserted into a shell command. Validate at this boundary: only lowercase hexadecimal characters in the `8-4-4-4-12` UUID shape are accepted. The same check is applied to capture payloads, registry records, and final command construction. Malformed values are ignored rather than interpolated. You cannot establish conversation ownership from a valid UUID alone.

## Extending capture

Implement the `Harness` trait in [`src/harness/mod.rs`](../src/harness/mod.rs) for each tool. Separate detection, evidence collection, and command construction through these methods:

- `shape`: supply the program word and resume selector. Accepted and canonical forms are derived from that pair by the default `detect` and `resume_command` implementations.
- `instrument`: return spawn-time arguments, environment entries, and an optional pinned ID.
- `parse_capture`: read an ID from hook, notify, or extension JSON.
- `live_session_id`: read the ID published on disk for a live session. Return `None` by default when no registry is available.
- `live_blocked_status`: read that registry for blocked-on-user status. Return preview text, never an ID; return `None` by default.
- `resolve_home`: resolve configuration needed by instrumentation or the live registry. Return `None` by default.

The launch environment is passed to `resolve_home` by the supervisor. For Claude and Codex, the explicit override is read first, then `$HOME` plus the tool's dot directory. With neither supplied, the supervisor's platform home is used. The resolved path is stored with the task and used for Claude registry reads after reconnect, preserving the launch-time home. For Grok and omp, no home resolution is required; the environment is passed to the child unchanged.

## Environment variables

| Variable | Meaning |
| -- | -- |
| `FLEETCOM_RUNTIME_DIR` | Explicit capture-asset root as well as the daemon runtime override. |
| `FLEETCOM_CAPTURE_FILE` | Per-run capture file used by the injected hook, notifier, or extension module. |
| `FLEETCOM_NOTIFY_CHAIN` | Newline-joined argv for the configured Codex notifier; empty when none is active. |
| `CLAUDE_CONFIG_DIR` | Claude home holding the `sessions/<pid>.json` registry; defaults to `$HOME/.claude`. |
| `CODEX_HOME` | Codex home used for notify routing; defaults to `$HOME/.codex`. |
