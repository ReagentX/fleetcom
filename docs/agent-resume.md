# Agent session resume

Session files preserve launch commands, not process state. Relaunching a bare `claude`, `codex`, `grok`, or `omp` command ordinarily starts another conversation. For accepted commands, `fleetcom` captures a validated conversation ID when available and builds a canonical resume command when saving a session or rerunning a finished task (`r`).

## Workflow

Start a supported agent without flags:

1. Press `n` and run `claude`, `codex`, `grok`, or `omp`. The task appears in the dashboard under the command you typed. Instrumentation changes only the string executed through `$SHELL -c`, so a direct spawn still displays the requested command.
2. Work in it. `Enter` attaches; `Ctrl-\` returns to the dashboard. Depending on the agent, `fleetcom` pins an ID at launch and may update it from a hook, notifier, extension, or matching live registry record.
3. Press `w`, enter a session name, and press `Enter`. A captured bare command becomes its canonical resume form, such as `claude --resume '<uuid>'`. Without a known ID, the save preserves the authored command.
4. Run `fleetcom <session>`, or press `o` in the dashboard, to start new processes from the saved commands. A stored resume command reopens its captured conversation.

On a finished agent task, `r` uses the captured launch, hook, notifier, extension, or registry ID. A registry record remains eligible after exit if it is still present. The replacement keeps the task's ID, tag, group, and name. After a successful rewrite, the row shows the resume command because it has become the task's launch recipe; a [saved session](sessions.md) records the same string.

Capture is best-effort and narrow by design. A command carrying a prompt, extra flags, or shell syntax stays opaque and saves verbatim. An accepted command with no available ID also saves unchanged. In both cases, loading the recipe reruns the original command.

## Accepted command boundary

The capture boundary is intentionally narrow. Only these forms participate:

- `claude`, `codex`, `grok`, or `omp`
- `claude --resume <uuid>`
- `codex resume <uuid>`
- `grok --resume <uuid>`
- `omp --resume <uuid>`

The program word may be a path such as `/usr/local/bin/claude` when its basename matches and the token contains no shell syntax. A resume UUID may be bare or single-quoted, but it must be the final argument.

Everything else remains opaque and runs, displays, and saves verbatim. This includes prompts, flags, alternate resume spellings, subcommands, trailing arguments, and shell syntax. The narrow boundary prevents injected arguments from binding to a different shell command than the detector recognized.

## Capture state and isolation

Hooks, notifiers, and extension modules are loaded by the agent rather than the supervisor, so they need stable paths. The supervisor installs those assets once for each runtime root. An explicit `FLEETCOM_RUNTIME_DIR` becomes that root. Otherwise, `fleetcom` uses the platform runtime or cache directory and partitions it by session directory.

Each supervisor installation creates a private mode-`0700` `<root>/<pid>-<nonce>` namespace containing:

- `claude-settings.json`, mode `0600`
- `codex-notify.sh`, mode `0700`
- `omp-capture.js`, mode `0600`: omp imports the module rather than executing it, so it needs no executable bit
- `task-<id>-<run>.json` capture paths

The random nonce separates concurrent supervisors and prevents PID reuse from selecting an existing namespace. The run number gives each rerun a distinct capture file, so a displaced process cannot overwrite the replacement run's session state. Installation leaves every other root entry unchanged.

## Evidence sources

### `claude`

A bare Claude command can accept an ID at launch. `fleetcom` therefore generates a v4 UUID and adds the settings overlay:

```text
--session-id '<uuid>' --settings '<namespace>/claude-settings.json'
```

A canonical resume command already supplies its conversation ID, so adding a second ID would be incorrect; it receives only `--settings`. The overlay installs a `SessionStart` hook that copies its JSON payload into `FLEETCOM_CAPTURE_FILE`, from which the harness reads `session_id`.

Claude also publishes one `<claude-home>/sessions/<pid>.json` record per session. `fleetcom` reads the direct path for the task leader's PID. When `$SHELL -c` leaves the shell as the task leader instead of replacing it with Claude, no matching record exists and the registry contributes no ID.

A record counts only when its `kind` is `interactive` and its `pid`, `cwd`, and `startedAt` match the task. The PID must match the filename, the working directories must be identical or resolve to the same path, and the process start must fall within 30 seconds of the task spawn. Missing, malformed, or mismatched records contribute no evidence. The dashboard also maps a matching record's `waiting` status to the top tier of its [preview cascade](commands.md#peek); other statuses do not affect the preview.

### `codex`

Codex does not let the caller choose an ID at launch. Both accepted forms instead receive a notify override:

```text
-c 'notify=["<namespace>/codex-notify.sh"]'
```

After each turn, the notifier writes the `agent-turn-complete` JSON argument to `FLEETCOM_CAPTURE_FILE`; the harness reads `thread-id`. This captures in-TUI session changes after the resumed conversation completes a turn.

Replacing a configured notifier would change user behavior. `fleetcom` reads bare top-level keys in `$CODEX_HOME/config.toml` until the first table header. A one-line `notify` array of non-empty basic strings is chained after the capture write. Its argv is carried in `FLEETCOM_NOTIFY_CHAIN`, joined by newlines, and the notification payload is appended. An absent setting or empty array lets capture run alone; empty arguments, newlines, and NUL cannot be transported and disable injection.

### `grok`

Grok accepts a launch-time ID but exposes no injectable live-capture channel. A bare command therefore receives `--session-id '<uuid>'`, while a canonical resume command needs no instrumentation.

### `omp`

omp cannot pin an ID at launch: it has no `--session-id`, and `--resume` requires an existing session. Both accepted forms therefore receive the same injection and no pinned ID:

```text
-e '<namespace>/omp-capture.js'
```

`-e` loads the JavaScript module into the agent process and appends it to the user's extensions. Its `session_start` and `session_switch` handlers write `sessionId` as JSON to `FLEETCOM_CAPTURE_FILE`. The second handler follows in-TUI `/resume` changes. Capture writes are best-effort: the module returns when the capture path is empty and ignores write errors.

The aliases `-r`, `--session`, and `-c` remain opaque because `fleetcom` rewrites only the canonical form it detects exactly.

## ID precedence

Several channels can identify different conversations during one task. To make the result deterministic, `fleetcom` chooses the first available ID in this order:

1. The current capture-file payload.
2. The live session registry, implemented by `claude`.
3. The ID pinned or targeted at spawn.

Named saves, recovery snapshots, and reruns use this same precedence. `fleetcom` does not scan session stores to infer conversation ownership: a nearby transcript or rollout cannot identify which task owns it.

Terminal output never supplies a session ID: examples, quoted commands, and tool output can contain another conversation's valid UUID. An ID available only in an exit hint is not recovered. Without a capture, registry, or launch ID, the authored command remains unchanged.

The registry outranks the spawn pin because it can contain a session ID selected after launch, including one created by `/clear`. The capture file outranks the registry.

Saving and rerunning rewrite accepted commands to one of these forms:

```text
claude --resume '<uuid>'
codex resume '<uuid>'
grok --resume '<uuid>'
omp --resume '<uuid>'
```

The program word is preserved as typed. If no valid ID is available, the original command remains unchanged. A rerun increments the run number before spawning its replacement, so capture data from the displaced run cannot affect the new run.

## Validation boundary

Every captured value eventually enters a shell command, which makes validation the security boundary. Accepted IDs contain exactly lowercase hexadecimal characters in the `8-4-4-4-12` UUID shape. Capture payloads, registry records, and the final command builder all apply the same check. Malformed values are ignored rather than interpolated. A valid UUID alone does not establish conversation ownership.

## Extending capture

Each tool implements the `Harness` trait in [`src/harness/mod.rs`](../src/harness/mod.rs). The methods keep detection, evidence collection, and command construction separate:

- `shape` supplies the program word and resume selector. The default `detect` and `resume_command` methods derive the accepted and canonical forms from that pair.
- `instrument` returns spawn-time arguments, environment entries, and an optional pinned ID.
- `parse_capture` reads an ID from hook, notify, or extension JSON.
- `live_session_id` reads the ID a live session publishes on disk. It defaults to `None` for tools that publish no registry.
- `live_blocked_status` reads that same registry for one display fact: whether the tool says it is blocked on the user. It returns preview text, never an ID, and defaults to `None`.
- `resolve_home` resolves configuration needed by instrumentation or the live registry. It defaults to `None`.

The supervisor supplies the launch environment to `resolve_home`. Claude and Codex read their explicit override first, then `$HOME` plus their dot directory. When neither is supplied, configuration reads fall back to the supervisor's platform home. The resolved path stays attached to the task so Claude registry reads continue using its launch-time home after a reconnect. Grok and omp need no home resolution; their environment passes through to the child unchanged.

## Environment variables

| Variable | Meaning |
| -- | -- |
| `FLEETCOM_RUNTIME_DIR` | Explicit capture-asset root as well as the daemon runtime override. |
| `FLEETCOM_CAPTURE_FILE` | Per-run capture file used by the injected hook, notifier, or extension module. |
| `FLEETCOM_NOTIFY_CHAIN` | Newline-joined argv for the configured Codex notifier; empty when none is active. |
| `CLAUDE_CONFIG_DIR` | Claude home holding the `sessions/<pid>.json` registry; defaults to `$HOME/.claude`. |
| `CODEX_HOME` | Codex home used for notify routing; defaults to `$HOME/.codex`. |
