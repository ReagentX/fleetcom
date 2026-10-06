# Agent session resume

Session files store launch recipes, not process state. A relaunched agent ordinarily starts a new conversation. `fleetcom` keeps the conversation only for agents it launched itself: a managed task, started from the spawn prompt's Agent page, is launched with the argv and environment that let `fleetcom` capture a validated session ID, and that ID is what a save, a recovery snapshot, or a rerun (`r`) reopens.

## Two kinds of task

Every task is one of two kinds, decided by how it was started, never by what the command says.

- **Literal.** You typed a command on the Command page, or a session file stored one. `fleetcom` runs exactly that text under `$SHELL -c` and never edits it. That holds when the text is exactly `claude`: a typed agent gets no instrumentation, no capture, no registry read, and no resume. Rerun runs the same text, and a save stores it. The dashboard still reads its screen: the summary adapter chosen by the command's first word scrapes the same previews it always did.
- **Managed.** You picked an agent on the Agent page (`n`, then `Tab`), or a session file stored a managed entry. `fleetcom` resolves the program word on the connected client's `PATH`, runs the binary directly with no shell between, and owns the session: it builds the argv, injects its capture channel, reads the ID back, and reopens that conversation on rerun and reload. The row shows the program word.

There are no settings for managed launches. An agent that needs flags is a literal command, and gives up management.

## Workflow

1. Press `n`, or `@` to pick a directory first, then `Tab` for the Agent page. The page lists the supported agents found on the daemon's `PATH`: `claude`, `codex`, `grok`, `omp`. Pick one and press `Enter`.
2. Work in it. Press `Enter` to attach; press `Ctrl-\` to return to the dashboard. Depending on the agent, an ID is pinned at launch and may be updated from a hook, notifier, extension, or matching live registry record.
3. Press `w`, enter a session name, and press `Enter`. The task is saved as a managed entry naming the agent and its current session ID; see [Sessions](sessions.md#format).
4. Run `fleetcom <session>`, or press `o` in the dashboard, to start new processes from the recipe. A managed entry launches the agent again and resumes the stored ID.

Press `r` on a finished managed task to rerun it with its best-known ID: the capture, registry, or launch ID. A registry record is still eligible after exit if present. Without any ID (an `omp` saved before its first turn, a `codex` whose notifier could not be chained), the rerun or reload starts a fresh conversation. The task's ID, tag, group, and name are preserved.

## Process identity

A managed agent is the task leader by construction: `fleetcom` executes the binary found on `PATH` directly, with no shell between. Claude's capture gate and registry reader key on the leader PID, so this holds in every login shell, including csh and tcsh, which keep the leader for themselves when a launch goes through `$SHELL -c`. Shebang scripts keep the identity: the kernel runs the interpreter in the same process. A wrapper script that starts the real binary as a child breaks it, the same way a resident shell would.

A managed agent inherits the environment of the `fleetcom` client it was launched from. Every spawn runs with the connected client's environment, sent at connect and reconnect, and that client's shell sourced its startup files before the environment was captured. A dotfile edit takes effect only after a client is started from a new shell. Reruns use the context current at the rerun. Verified 2026-10-05 with marker exports in a scratch `ZDOTDIR`: Claude Code 2.1.289's tool commands saw the launch environment plus `.zshenv` exports, and codex-cli 0.160.1 ran each command as `/bin/zsh -lc`, so neither loses anything a shell trampoline would have added.

## Capture state and isolation

Hooks, notifiers, and extension modules are loaded by the agent rather than the supervisor, so they need stable paths. Assets are installed once per runtime root. An explicit `FLEETCOM_RUNTIME_DIR` is used as the root; otherwise, the platform runtime or cache directory is used, partitioned by session directory.

For each supervisor installation, a private mode-`0700` `<root>/<pid>-<nonce>` namespace is created with:

- `claude-settings.json`, mode `0600`
- `codex-notify.sh`, mode `0700`
- `omp-capture.js`, mode `0600`: imported as a module, so no executable bit is required
- `task-<id>-<run>.json` capture paths

The random nonce isolates concurrent supervisors and prevents PID reuse from selecting an existing namespace. Each rerun also gets a distinct run number, so a displaced process cannot overwrite the replacement run's capture file. Every other root entry is left unchanged during installation. A managed launch whose assets cannot be installed is refused: without the overlay, a `codex` would hand its conversation to the shared server.

## Evidence sources

Each managed agent process is supervised by the `fleetcom` daemon. A conversation handed off to the agent CLI's own background service is outside that task. Keep the conversation in the task's process and accept captured IDs only from that process.

All validation on 2026-10-04 was performed on macOS; Linux was not tested.

### `claude`

Claude accepts an ID at launch. For a fresh launch, the harness therefore generates a v4 UUID and passes it ahead of the settings overlay:

```text
--session-id <uuid> --settings <namespace>/claude-settings.json
```

A resume names its conversation with `--resume <uuid>` instead, and only `--settings` follows. Two keys are layered over your settings for the launched process alone; your settings files are not modified.

With the first key, `"disableAgentView": true`, agent view is disabled: `claude agents`, `--bg`, `/background`, and Claude's on-demand daemon. With agent view enabled, a conversation can be handed off to that daemon, outside the task's process. In a managed task, you therefore cannot background a conversation: `/background` is unavailable, you cannot park the conversation by pressing Left arrow on an empty prompt, and with `/fork` you start a sub-agent inside the task's process instead of a background session.

A `SessionStart` hook is registered through the second key:

```text
{ printf '%s\n' "$PPID"; cat; } > "$FLEETCOM_CAPTURE_FILE"
```

The first line of the capture file is the parent Claude process's PID; the rest is the hook's JSON payload. Read `session_id` from that payload only when the PID is the task leader's. Reject captures written by any other process.

Claude session records are also available at `<claude-home>/sessions/<pid>.json`, one per session. The direct path for the task leader's PID is read.

A record is accepted only when its `kind` is `interactive` and its `pid`, `cwd`, and `startedAt` match the task. The PID must match the filename, the working directories must be identical or resolve to the same path, and the process start must fall within 30 seconds of the task spawn. Missing, malformed, or mismatched records are ignored. A matching record's `waiting` status is also mapped to the top tier of the [preview cascade](commands.md#peek); the preview is unchanged for other statuses. A literal `claude` never gets this tier: its blocked status comes from the screen alone.

Both hook validation and registry lookup use the task leader's PID, which a managed launch makes the Claude process by construction. A stamp from any other PID is rejected and the next source is tried, so a conversation selected after launch, including through `/clear`, is followed through the registry.

Live validation on 2026-10-04 was performed with Claude Code 2.1.289. With agent view enabled, after using `/fork`, `/background`, or Left arrow on an empty prompt, the capture file was written by a process other than the task. When rerunning with such an ID, the exit status was 1 with "… is running in the background. Run `claude attach <id>` …". After pressing Left arrow, the original ID was still in the registry; on rerun with that ID, a snapshot was reopened without the later turns. With `disableAgentView` in the overlay, no handoffs were observed. The sub-agent started through `/fork <directive>` was run in-process, and the same ID was present in the capture file, registry, and spawn-time metadata. The task's PID was stamped in the capture at startup and again after `/clear`.

### `codex`

You cannot choose a Codex ID at launch. Two overrides ride every managed launch, after `resume <uuid>` when resuming:

```text
-c 'notify=["<namespace>/codex-notify.sh"]' -c 'features.daemon_auto_start=false'
```

Since Codex 0.157.0, running plain `codex` attaches to a shared background server. With the notify override alone, embedded mode is used and a startup warning is displayed. Set `features.daemon_auto_start=false` to explicitly use embedded mode without the warning.

With embedded mode, the conversation is kept in the task's process. For a managed task, the shared server is never used: you cannot find the task in `codex agents` or reach it through `codex queue`. The embedded override is therefore added on every managed launch, including those where capture is unavailable: without it, killing the task would kill only the TUI client and leave the conversation running in the shared server.

After each turn, Codex runs the notify script with the `agent-turn-complete` JSON as its argument. Notifications are emitted for several threads in one Codex process: the conversation on screen, each spawned sub-agent, and a hidden thread used to title a new session. Only the first is the task's conversation, so do not use the notified `thread-id` directly. The title thread's notification lands before the root's in some sessions and after it in others, so a notification must be validated when it arrives, not when the capture file is read: a later title notification written verbatim would erase the root.

The script therefore runs `"$FLEETCOM_BINARY" --codex-notify-v1 "$1"`, an internal mode of the daemon's own binary. The mode accepts only an `agent-turn-complete` payload with a strict UUID `thread-id`. It resolves the Codex home from its own environment as at launch (`CODEX_HOME`, then `$HOME/.codex`), finds the thread's rollout under `<codex-home>/sessions/<YYYY>/<MM>/<DD>/`, and reads its first line to resolve the root of the thread's session tree: the thread's own ID for a root, or the root ID recorded in a sub-agent's header. When the thread is notified, its rollout and header are already on disk. Accept a thread only when exactly one rollout is named for it, with a header in either the root format or the sub-agent format with a root ID. No rollout is saved for the title thread, so always reject it.

On acceptance, the mode writes the bare root UUID, with no newline, to `<capture>.<pid>.tmp` beside the capture file and renames it over `FLEETCOM_CAPTURE_FILE`, so a reader never sees a partial file. On refusal it writes nothing: the slot only ever moves from one accepted root to another. It prints nothing; its exit status is informational, and the script ignores it. After a completed turn in the resumed conversation, an in-TUI session change is captured this way.

The capture file format is frozen as v1: exactly one bare UUID. When the file is read, accept it only when its whole contents pass the UUID check, with no trimming. Nothing is reparsed and no rollout is read at that point, so an accepted root stays accepted after Codex compresses old rollouts, and the notification's conversation text never reaches disk. An older script's JSON payload fails that check and is refused: the safe fallback, since the launch ID or no ID is used instead.

Look up only the thread ID from the task's own notification. List the dated directories to find its rollout, then read only the first line. Do not infer ownership from other sessions in the store.

`FLEETCOM_BINARY` is the daemon's own executable, probed at each `codex` spawn: it must be an existing regular file with an execute bit. On Linux, `current_exe` reads `/proc/self/exe`, which becomes `<path> (deleted)` once the binary is replaced on disk under a running daemon; the path then fails the probe. On macOS the path survives a reinstall and names the new binary, so an older release there degrades to an unrecognized flag and no write. Either way, the chained notifier still runs.

To preserve a configured notifier, read bare top-level keys in `$CODEX_HOME/config.toml` up to the first table header. For a one-line `notify` array of non-empty basic strings, invoke the notifier after the validation step. Store its argv in `FLEETCOM_NOTIFY_CHAIN`, joined by newlines, and append the notification payload before invocation. The script `exec`s the chain whatever happens to fleetcom's part: a missing binary, a refusal, or a crash. With an absent setting or empty array, run only the validation step.

The notify override and its environment are injected only when the configured route can be chained and the binary passes the probe. Do not inject them for empty arguments, newlines, or NUL: these cannot be represented in the chain encoding. Also skip them when `config.toml` exists but cannot be read, or when a line above the first table header is not a bare key assigned a recognized single-line value. In each case the launch carries the embedded override alone, no ID is captured, and a status line says why: `` `notify` config can't be chained ``, or `fleetcom binary replaced; restart the daemon`.

Live validation on 2026-09-28 was performed with the official macOS arm64 Codex 0.157.1 and 0.158.0 executables, isolated homes, and a scripted loopback Responses server. In both releases, the notification JSON was written by the capture script of that date, which copied it verbatim, and passed to the configured notifier. After saving and loading through foreground fleetcom, the same thread ID was present and its history was reopened. Both releases were tested with `--no-daemon`. The ID was also captured and saved in separate default-launch probes, but no managed Codex daemon was installed or running in those fresh homes. Real-account behavior was not verified.

The [recorded terminal fixtures](../tests/corpus/codex_0158_terminal/README.md) preserve resumed-history and clipboard output from those runs. Offline replay checks the recorded display and OSC 52 forwarding; it does not run the notifier or execute save/load. The separate `tests/daemon_resume.rs` integration tests exercise session resume with stub agents.

Live validation on 2026-10-04 was performed with codex-cli 0.160.0 over nine TUI sessions. Three thread IDs were reported from one process: the unsaved title thread, a sub-agent, and the root. The title thread's notification arrived before the root's in three sessions and after it in six, and no rollout appeared for it even minutes later. For a root or sub-agent turn, the thread's rollout header was on disk at least 2.5 s before its notification, and `task_complete` landed 21–256 ms before it. When resuming the title thread, "No saved session found" was reported; for the sub-agent, "cannot resume an unloaded multi-agent v2 sub-agent through its parent" was reported. With the notify override alone, `⚠ 1 warning` was displayed in the footer; with both overrides, none was displayed. With an unknown `features.*` key, startup completed with an unrecognized-setting warning. No Codex version predating `daemon_auto_start` was tested. On the same date, 493 local rollout headers from Codex 0.135.0 through 0.160.0 were examined: `id` and `session_id` were present in each. In Codex 0.140.0 and 0.141.0, a sub-agent's own ID was stored as its `session_id`; that format is rejected.

### `grok`

You can specify a Grok ID at launch, but cannot inject a live-capture channel. A fresh launch passes `--session-id <uuid>`; a resume passes `--resume <uuid>`. Nothing else rides along.

### `omp`

You cannot pin an omp ID at launch: no `--session-id` flag is available, and an existing session is required for `--resume`. Every managed launch therefore loads the extension and pins nothing; a resume names its conversation with `--resume <uuid>` first:

```text
-e <namespace>/omp-capture.js
```

Load the JavaScript module into the agent process through `-e`, appending it to your extensions. On `session_start`, `session_switch`, `session_branch`, and `agent_end`, write `sessionId` as JSON to `FLEETCOM_CAPTURE_FILE`, including after in-TUI `/resume` changes. Report only IDs usable with `omp --resume`:

- The session is the top-level one: `ctx.agent.kind` is `"main"`. The same handlers are registered for sub-agent sessions, but you cannot resume a sub-agent by ID.
- The session file exists on disk. It is created after the first assistant message.

At `agent_end`, after every turn, report two changes without dedicated events: session file creation and, since omp 18.5.0, assignment of a new ID when the session lease is held by another live process.

As a result:

- For a fresh `omp` task, no ID is captured until the end of the first turn. Save before then and the entry carries no ID: it reloads fresh.
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

Session IDs are never read from terminal output: another conversation's valid UUID may be present in examples, quoted commands, or tool output. An ID available only in an exit hint is not recovered. Without a capture, registry, or launch ID, the entry carries no ID and the next launch is fresh.

A registry ID is preferred over the spawn pin because it may have been selected after launch, including through `/clear`. A capture-file ID is preferred over the registry ID.

On save, a managed task is written as `{ "agent": "<word>", "resume": "<uuid>" }`. On rerun and reload, the harness places the selector and the ID first in the argv:

```text
claude --resume <uuid>
codex resume <uuid>
grok --resume <uuid>
omp --resume <uuid>
```

The ID is one argv element; no shell parses it. On rerun, the run number is incremented before the replacement is spawned, isolating it from capture data for the displaced run.

## Validation boundary

A captured ID enters an agent's argv as one element and a session file as the `resume` field. Before either, accept only lowercase hexadecimal characters in the `8-4-4-4-12` UUID format. Validate capture payloads, Codex rollout headers, registry records, and session files on load. Ignore malformed values from capture channels; refuse a session file that carries one. Conversation ownership cannot be determined from UUID validity alone.

## Extending capture

Implement the `Harness` trait in [`src/harness/mod.rs`](../src/harness/mod.rs) for each tool. Register it in that file's `AGENTS` table: the one registry the Agent page, session files, and the summary adapters read. Include a `SummaryAdapter` for the dashboard preview with each entry; see the implementations in [`src/harness/summary.rs`](../src/harness/summary.rs). Separate launch construction from evidence collection through these methods:

- `shape`: supply the program word and resume selector. The word names the tool on the Agent page and in session files; `harness::intent_args` places the selector and the ID first in a resuming argv.
- `session_flag`: name the flag that pins a session ID on a fresh launch, or return `None` (the default) when the tool assigns its own. `harness::plan` builds the conversation-selecting argv from this flag and the resume selector.
- `overlay`: return the argv elements, environment entries, and optional notice every launch of the tool carries after the conversation selection; the supervisor reports the notice on the status line once the task spawns. The elements are passed to the binary directly.
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
| `FLEETCOM_BINARY` | The daemon's own executable, which the injected Codex notifier runs as `--codex-notify-v1` to validate each notification; absent when that path is unusable. |
| `CLAUDE_CONFIG_DIR` | Claude home holding the `sessions/<pid>.json` registry; defaults to `$HOME/.claude`. |
| `CODEX_HOME` | Codex home holding `config.toml` for notify routing and the `sessions/` rollouts; defaults to `$HOME/.codex`. |
