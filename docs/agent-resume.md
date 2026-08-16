# Agent session resume

Session files preserve launch commands, not process state. Relaunching a bare `claude`, `codex`, `grok`, or `omp` command ordinarily starts another conversation. For accepted commands, `fleetcom` captures a validated conversation ID when available and builds a canonical resume command when saving a session or rerunning a finished task (`r`).

## Workflow

Start a supported agent without flags:

1. Press `n` and run `claude`, `codex`, `grok`, or `omp`. The task appears in the dashboard under the command you typed. Instrumentation changes only the string executed through `$SHELL -c`, so a direct spawn still displays the requested command.
2. Work in it. `Enter` attaches; `Ctrl-\` returns to the dashboard. Depending on the agent, `fleetcom` pins an ID at launch and may update it from a hook, notifier, or extension while the task runs or from terminal output after it exits.
3. Press `w`, enter a session name, and press `Enter`. If the earlier sources produced no ID, the save also checks the agent's on-disk session store. A captured bare command becomes its canonical resume form, such as `claude --resume '<uuid>'`.
4. Run `fleetcom <session>`, or press `o` in the dashboard, to start new processes from the saved commands. A stored resume command reopens its captured conversation.

On a finished agent task, `r` uses the captured launch, hook, notifier, registry, or exit ID without performing save-time filesystem correlation. A registry record remains eligible after exit if it is still present. The replacement keeps the task's ID, tag, group, and name. After a successful rewrite, the row shows the resume command because it has become the task's launch recipe; a [saved session](sessions.md) records the same string.

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

### `omp`

omp can pin no ID at launch: it ships no `--session-id`, and `--resume` rejects an ID that does not already exist, so a generated UUID would name a session the resume command could never reach. Both accepted forms therefore take the same injection, and neither carries a pinned ID:

```text
-e '<namespace>/omp-capture.js'
```

That asset is a JavaScript module, and `-e` loads it into the agent's own process at startup, appending to the user's own extensions rather than replacing them. Its `session_start` and `session_switch` handlers write the session ID as JSON to `FLEETCOM_CAPTURE_FILE`; the harness reads `sessionId`. `session_switch` is what covers omp's in-TUI `/resume`, which changes the session ID of a process `fleetcom` has already launched.

No other harness runs code inside the agent: Claude's asset is a settings file and Codex's is a shell script the agent execs after a turn. Two things bound that. The module no-ops when `FLEETCOM_CAPTURE_FILE` is unset or empty, and it swallows every error it raises; omp's own extension runner then calls each handler under a timeout inside a `catch`, reporting a throw to its extension error channel instead of propagating it. `--trusted-extension` fills the same slot and is never used: it is mutually exclusive with `-e` and replaces the user's entire extension discovery, omp's own bridges included.

After exit, the harness scans retained terminal text for the last `omp --resume <uuid>` hint. omp prints it as `Resume this session with omp --resume <uuid>`, and a crash prints the same command inside a `[Recovery]` block, so one matcher reads both. That block carries one entry per live session, though — main first, then every subagent under its own agent ID — so a labelled entry counts only when its label is `Main`. A block naming no main session yields nothing: a subagent transcript lives below what omp's own resume lookup scans, so its ID cannot be resumed at all, and the capture file's ID is worth more. omp also resumes through `-r`, `--session`, and `-c`; those spellings stay opaque, because a command `fleetcom` cannot rewrite exactly is left verbatim.

Save-time filesystem correlation reads `<sessions root>/<encoded-cwd>/<iso-ts>_<uuid>.jsonl`, where the sessions root comes from omp's own variable chain rather than one home override; the [environment-variable table](#environment-variables) lists it. `PI_CODING_AGENT_SESSION_DIR` is the exception: omp passes that path straight through as the session file's parent and never computes a bucket, so the store is flat under it. The scan covers the root and one level below without inferring which layout is in play.

Correlation does not reproduce the bucket name. omp encodes a working directory through three scopes — under `$HOME`, under the temporary directory, otherwise absolute — after realpath-canonicalizing the working directory, `$HOME`, and `$TMPDIR`, and it changed that scheme three times inside the 17.2.x line, each change shipping an on-disk migration. The scan enumerates the buckets instead and confirms the directory from the session header's own `cwd` field. That field is not simply the resolved path: on macOS omp strips a `/private` prefix when both forms resolve alike, which is the opposite of what canonicalizing produces, so the header is compared against the task's directory as given, as canonicalized, and canonicalized itself. A candidate must be the sole file whose UUIDv7 creation instant falls within the 30-second spawn window; an ID that is not v7 is skipped rather than dated from metadata omp did not write. Duplicate IDs collapse first, because omp's bucket-rename migration preserves the legacy entry on a filename collision and one session can therefore appear under two bucket names.

A session with no assistant message leaves no file at all, because omp holds it in memory until the model replies. Correlation therefore cannot find a just-launched session, and an empty bucket is ordinary rather than an error: a session with no reply has nothing worth resuming.

## ID precedence

Several channels can identify different conversations during one task. To make the result deterministic, `fleetcom` chooses the first available ID in this order:

1. The exit hint scraped after process exit and PTY-reader EOF.
2. The current capture-file payload.
3. The live session registry, implemented by `claude`.
4. The ID pinned or targeted at spawn.
5. Save-time filesystem correlation, when exactly one store entry matches the task and the 30-second spawn window.

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

Every captured value eventually enters a shell command, which makes validation the security boundary. Accepted IDs contain exactly lowercase hexadecimal characters in the `8-4-4-4-12` UUID shape. Capture payloads, terminal hints, registry records, store names, and the final command builder all apply the same check. Malformed values are ignored rather than interpolated.

## Extending capture

Each tool implements the `Harness` trait in [`src/harness/mod.rs`](../src/harness/mod.rs). The methods keep detection, evidence collection, and command construction separate:

- `shape` supplies the program word and resume selector. The default `detect` and `resume_command` methods derive the accepted and canonical forms from that pair.
- `instrument` returns spawn-time arguments, environment entries, and an optional pinned ID.
- `parse_capture` reads an ID from hook, notify, or extension JSON.
- `scrape_exit` reads an ID from retained terminal text.
- `live_session_id` reads the ID a live session publishes on disk. It defaults to `None` for tools that publish no registry.
- `live_blocked_status` reads that same registry for one display fact: whether the tool says it is blocked on the user. It returns preview text, never an ID, and defaults to `None`.
- `correlate_fs` finds one matching on-disk session.
- `resolve_home` turns the task's launch environment into the tool's store root.

The supervisor supplies the launch environment and delegates the decision to `resolve_home`. Its default is the two-step rule three of the four tools follow: the tool-specific variable first, then `$HOME` plus the tool's dot directory. omp overrides it, because its store root comes from a chain of variables and a filesystem-conditional XDG branch that no single override can express. Either way, the resolved path remains attached to the task for later filesystem correlation.

## Environment variables

| Variable | Meaning |
| -- | -- |
| `FLEETCOM_RUNTIME_DIR` | Explicit capture-asset root as well as the daemon runtime override. |
| `FLEETCOM_CAPTURE_FILE` | Per-run capture file used by the injected hook, notifier, or extension module. |
| `FLEETCOM_NOTIFY_CHAIN` | Newline-joined argv for the configured Codex notifier; empty when none is active. |
| `CLAUDE_CONFIG_DIR` | Claude home holding the `sessions/<pid>.json` registry and the transcripts used for correlation; defaults to `$HOME/.claude`. |
| `CODEX_HOME` | Codex home used for notify routing and rollout correlation; defaults to `$HOME/.codex`. |
| `GROK_HOME` | Grok home used for session-directory correlation; defaults to `$HOME/.grok`. |
| `PI_CODING_AGENT_SESSION_DIR` | omp sessions root, used verbatim for correlation. The rest of omp's chain builds that path instead of naming it. |
| `PI_CODING_AGENT_DIR` | omp agent directory, whose `sessions` subdirectory is the store. A selected profile ignores it. |
| `PI_CONFIG_DIR` | omp config directory name under `$HOME`; defaults to `.omp`. An absolute value diverges from omp's own joining and correlation then finds nothing rather than the wrong session. |
| `OMP_PROFILE` | omp profile, read by presence: it selects a profile when non-empty and suppresses `PI_PROFILE` when empty. |
| `PI_PROFILE` | omp profile used only when `OMP_PROFILE` is absent. A profile inserts `profiles/<name>` under the config directory. |
| `XDG_DATA_HOME` | Redirects the still-default omp agent directory to `<value>/omp`, flattening the `agent/` level, and only when that directory already exists. |
