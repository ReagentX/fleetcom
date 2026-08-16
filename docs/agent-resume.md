# Agent session resume

Session files preserve launch commands, not process state. Relaunching a bare `claude`, `codex`, or `grok` command ordinarily starts another conversation. For accepted commands, `fleetcom` captures a validated conversation ID when available and builds a canonical resume command when saving a session or rerunning a finished task (`r`).

## Workflow

Start a supported agent without flags:

1. Press `n` and run `claude`, `codex`, or `grok`. The task appears in the dashboard under the command you typed. Instrumentation changes only the string executed through `$SHELL -c`, so a direct spawn still displays the requested command.
2. Work in it. `Enter` attaches; `Ctrl-\` returns to the dashboard. Depending on the agent, `fleetcom` pins an ID at launch and may update it from a hook or notifier while the task runs or from terminal output after it exits.
3. Press `w`, enter a session name, and press `Enter`. If the earlier sources produced no ID, the save also checks the agent's on-disk session store. A captured bare command becomes its canonical resume form, such as `claude --resume '<uuid>'`.
4. Run `fleetcom <session>`, or press `o` in the dashboard, to start new processes from the saved commands. A stored resume command reopens its captured conversation.

On a finished agent task, `r` uses the captured launch, hook, notifier, registry, or exit ID without performing save-time filesystem correlation. The registry earns its place at save (`w`) rather than rerun: it names the conversation a *live* session is running even when the `SessionStart` hook never fired, as with hooks disabled. A rerun reads it only after an unclean exit, since a clean exit removes the record. The replacement keeps the task's ID, tag, group, and name. After a successful rewrite, the row shows the resume command because it has become the task's launch recipe; a [saved session](sessions.md) records the same string.

Capture is best-effort and narrow by design. A command carrying a prompt, extra flags, or shell syntax stays opaque and saves verbatim. An accepted command with no available ID also saves unchanged. In both cases, loading the recipe reruns the original command.

## Accepted command boundary

The capture boundary is intentionally narrow. Only these forms participate:

- `claude`, `codex`, or `grok`
- `claude --resume <uuid>`
- `codex resume <uuid>`
- `grok --resume <uuid>`

The program word may be a path such as `/usr/local/bin/claude` when its basename matches and the token contains no shell syntax. A resume UUID may be bare or single-quoted, but it must be the final argument.

Everything else remains opaque and runs, displays, and saves verbatim. This includes prompts, flags, alternate resume spellings, subcommands, trailing arguments, and shell syntax. The narrow boundary prevents injected arguments from binding to a different shell command than the detector recognized.

## Capture state and isolation

Hooks and notifiers run outside the supervisor, so they need stable paths. The supervisor installs those assets once for each runtime root. An explicit `FLEETCOM_RUNTIME_DIR` becomes that root. Otherwise, `fleetcom` uses the platform runtime or cache directory and partitions it by session directory.

Each supervisor installation creates a private mode-`0700` `<root>/<pid>-<nonce>` namespace containing:

- `claude-settings.json`, mode `0600`
- `codex-notify.sh`, mode `0700`
- `task-<id>-<run>.json` capture paths

The random nonce separates concurrent supervisors and prevents PID reuse from selecting an existing namespace. The run number gives each rerun a distinct capture file, so a displaced process cannot overwrite the replacement run's session state. Installation leaves every other root entry unchanged.

## Evidence sources

### `claude`

A bare Claude command can accept an ID at launch. `fleetcom` therefore generates a v4 UUID and adds the settings overlay:

```text
--session-id '<uuid>' --settings '<namespace>/claude-settings.json'
```

A canonical resume command already supplies its conversation ID, so adding a second ID would be incorrect; it receives only `--settings`. The overlay installs a `SessionStart` hook that copies its JSON payload into `FLEETCOM_CAPTURE_FILE`, from which the harness reads `session_id`.

Claude also publishes a live session registry: one `<claude-home>/sessions/<pid>.json` record per session, written and rewritten by the CLI itself with no instrumentation. `sh`, `bash`, `zsh`, and `dash` each exec a single simple `-c` command in place rather than forking, so a task's own PID names its record and the lookup is a direct path, not a search. That exec is a shell optimization, not a guarantee: under a `$SHELL` that forks and waits instead, the task's leader is the shell, no record is filed under its PID, and the registry simply goes unused rather than wrong.

A record counts only when its `kind` is `interactive` and its `pid`, `cwd`, and `startedAt` all match the task. A live task's PID cannot be reissued — `fleetcom` reaps with `WNOWAIT`, leaving the exited leader a zombie that holds the PID for the task's whole life — so the file is that task's own record or nothing. The `cwd` and `startedAt` guards close what the reservation cannot: the CLI removes its record on a clean exit but leaves it behind when the process dies on a signal, and only the next `claude` launch sweeps it, so a record from an *earlier* process at that PID can otherwise be read as this task's. `cwd` matches through symlink aliases, because claude records the `getcwd(3)` form while a task carries the path it was spawned with. `startedAt` names the process start, which `/clear` leaves untouched, so the 30-second match does not decay as a session ages. `/cd` inside claude moves the session's directory and loses the record. The CLI rewrites the file in place rather than renaming a temporary, so a torn read yields no evidence rather than bad evidence. The same record also carries the session's live status, from which the dashboard reads one state — `waiting`, the CLI blocked on the user — as the top tier of its [preview cascade](commands.md#peek).

After the process exits and the PTY reader reaches EOF, the harness scans the retained terminal text for the last `claude --resume <uuid>` hint. Save-time filesystem correlation checks `<claude-home>/projects/<cwd-slug>/<uuid>.jsonl`, where the slug replaces `/` and `.` in the absolute working directory with `-`.

### `codex`

Codex does not let the caller choose an ID at launch. Both accepted forms instead receive a notify override:

```text
-c 'notify=["<namespace>/codex-notify.sh"]'
```

After each turn, the notifier writes the `agent-turn-complete` JSON argument to `FLEETCOM_CAPTURE_FILE`; the harness reads `thread-id`. This captures in-TUI session changes after the resumed conversation completes a turn.

Replacing a configured notifier would change user behavior. When the effective Codex configuration contains a one-line `notify` array of non-empty basic strings, the capture script executes that notifier after writing the capture file. Its argv is carried in `FLEETCOM_NOTIFY_CHAIN`, joined by newlines, and the notification payload is appended. An empty, multiline, ambiguous, or unsupported `notify` value disables the injected override so the configured route remains unchanged. The line-based configuration reader checks `config.toml` and the profile selected by its first `profile = ...` assignment; the profile's notify assignment takes precedence.

The exit scraper accepts `codex resume <uuid>` and `codex resume, then select <name> (<uuid>)`, using only the UUID. Save-time filesystem correlation checks dated rollout directories under `<codex-home>/sessions/YYYY/MM/DD/`. A rollout matches when its v7 UUID timestamp is within 30 seconds of task spawn and the first record names the task's working directory. The search covers the spawn's UTC date plus or minus two days because the directory date is local time.

### `grok`

Grok accepts a launch-time ID but exposes no injectable live-capture channel. A bare command therefore receives `--session-id '<uuid>'`, while a canonical resume command needs no instrumentation.

After exit, the harness scans retained terminal text for the last `grok -r <uuid>` or `grok --resume <uuid>` hint. Save-time filesystem correlation checks `<grok-home>/sessions/<encoded-cwd>/<uuid>/`, percent-encoding the canonical working directory, falling back to a group whose `.cwd` file names that path when the encoded name is too long, and ignoring `session_kind: subagent` directories.

## ID precedence

Several channels can identify different conversations during one task. To make the result deterministic, `fleetcom` chooses the first available ID in this order:

1. The exit hint scraped after process exit and PTY-reader EOF.
2. The current capture-file payload.
3. The live session registry, currently `claude` only.
4. The ID pinned or targeted at spawn.
5. Save-time filesystem correlation, when exactly one store entry matches the task and the 30-second spawn window.

The registry outranks the spawn pin because the pin records what `fleetcom` asked for while the registry records what the CLI is running, and those diverge the moment a user runs `/clear`, which mints a fresh ID mid-session. It ranks below the capture file only because that file is `fleetcom`'s own hook output, and the two agree whenever both exist.

Saving and rerunning rewrite accepted commands to one of these forms:

```text
claude --resume '<uuid>'
codex resume '<uuid>'
grok --resume '<uuid>'
```

The program word is preserved as typed. If no valid ID is available, the original command remains unchanged. A rerun increments the run number before spawning its replacement, so capture data from the displaced run cannot affect the new run.

## Validation boundary

Every captured value eventually enters a shell command, which makes validation the security boundary. Accepted IDs contain exactly lowercase hexadecimal characters in the `8-4-4-4-12` UUID shape. Capture payloads, terminal hints, registry records, store names, and the final command builder all apply the same check. Malformed values are ignored rather than interpolated.

## Extending capture

Each tool implements the `Harness` trait in [`src/harness/mod.rs`](../src/harness/mod.rs). The methods keep detection, evidence collection, and command construction separate:

- `shape` supplies the program word and resume selector. The default `detect` and `resume_command` methods derive the accepted and canonical forms from that pair.
- `instrument` returns spawn-time arguments, environment entries, and an optional pinned ID.
- `parse_capture` reads an ID from hook or notify JSON.
- `scrape_exit` reads an ID from retained terminal text.
- `live_session_id` reads the ID a live session publishes on disk. It defaults to `None` for tools that publish no registry.
- `live_blocked_status` reads that same registry for one display fact: whether the tool says it is blocked on the user. It returns preview text, never an ID, and defaults to `None`.
- `correlate_fs` finds one matching on-disk session.

The supervisor resolves each harness home from the task's launch environment: the tool-specific variable first, then `$HOME` plus the tool's dot directory. That resolved path remains attached to the task for later filesystem correlation.

## Environment variables

| Variable | Meaning |
| -- | -- |
| `FLEETCOM_RUNTIME_DIR` | Explicit capture-asset root as well as the daemon runtime override. |
| `FLEETCOM_CAPTURE_FILE` | Per-run capture file used by the injected hook or notifier. |
| `FLEETCOM_NOTIFY_CHAIN` | Newline-joined argv for the configured Codex notifier; empty when none is active. |
| `CLAUDE_CONFIG_DIR` | Claude home holding the `sessions/<pid>.json` registry and the transcripts used for correlation; defaults to `$HOME/.claude`. |
| `CODEX_HOME` | Codex home used for notify routing and rollout correlation; defaults to `$HOME/.codex`. |
| `GROK_HOME` | Grok home used for session-directory correlation; defaults to `$HOME/.grok`. |
