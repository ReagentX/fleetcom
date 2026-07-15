# Agent session resume

Session files preserve launch commands, not application state. This is usually
the correct boundary, but it is problematic for agent CLIs: relaunching a bare
`claude`, `codex`, or `grok` command starts another conversation.

To preserve that conversation, `fleetcom` captures a validated ID and builds the
resume command used by session save or rerun (`r`). Instrumentation changes only
the string executed through `$SHELL -c`; direct spawns and session loads still
display the requested command. Rerun displays the generated resume command
because that command becomes the task's new launch recipe.

## Detection boundary

The capture boundary is intentionally narrow. Only these forms participate:

- `claude`, `codex`, or `grok`
- `claude --resume <uuid>`
- `codex resume <uuid>`
- `grok --resume <uuid>`

The program word may be a path such as `/usr/local/bin/claude` when its basename
matches and the token contains no shell syntax. A resume UUID may be bare or
single-quoted, but it must be the final argument.

Everything else remains opaque and runs, displays, and saves verbatim. This
includes prompts, flags, alternate resume spellings, subcommands, trailing
arguments, and shell syntax. The narrow boundary prevents injected arguments
from binding to a different shell command than the detector recognized.

## Capture state and isolation

Hooks and notifiers run outside the supervisor, so they need stable paths. The
supervisor installs those assets once for each runtime root. An explicit
`FLEETCOM_RUNTIME_DIR` becomes that root. Otherwise, `fleetcom` uses the platform
runtime or cache directory and partitions it by session directory.

Each supervisor installation creates a private mode-`0700`
`<root>/<pid>-<nonce>` namespace containing:

- `claude-settings.json`, mode `0600`
- `codex-notify.sh`, mode `0700`
- `task-<id>-<run>.json` capture paths

The random nonce separates concurrent supervisors and prevents PID reuse from
selecting an existing namespace. The run number gives each rerun a distinct
capture file; as a result, a displaced process cannot overwrite the replacement
run's session state. Installation leaves every other root entry unchanged.

## How each tool exposes an ID

### `claude`

A bare Claude command can accept an ID at launch. `fleetcom` therefore generates
a v4 UUID and adds the settings overlay:

```text
--session-id '<uuid>' --settings '<namespace>/claude-settings.json'
```

A canonical resume command already supplies its conversation ID, so adding a
second ID would be incorrect; it receives only `--settings`. The overlay installs
a `SessionStart` hook that copies its JSON payload into
`FLEETCOM_CAPTURE_FILE`, from which the harness reads `session_id`.

After the process exits and the PTY reader reaches EOF, the harness scans the
retained terminal text for the last `claude --resume <uuid>` hint. Save-time
filesystem correlation checks
`<claude-home>/projects/<cwd-slug>/<uuid>.jsonl`, where the slug replaces `/`
and `.` in the absolute working directory with `-`.

### `codex`

Codex does not let the caller choose an ID at launch. Both accepted forms instead
receive a notify override:

```text
-c 'notify=["<namespace>/codex-notify.sh"]'
```

After each turn, the notifier writes the `agent-turn-complete` JSON argument to
`FLEETCOM_CAPTURE_FILE`; the harness reads `thread-id`. This captures in-TUI
session changes after the resumed conversation completes a turn.

Replacing a configured notifier would change user behavior. When the effective
Codex configuration contains a one-line `notify` array of non-empty basic
strings, the capture script executes that notifier after writing the capture
file. Its argv is carried in
`FLEETCOM_NOTIFY_CHAIN`, joined by newlines, and the notification payload is
appended. An empty, multiline, ambiguous, or unsupported `notify` value disables
the injected override so the configured route remains unchanged. The line-based
configuration reader checks `config.toml` and the profile selected by its first
`profile = ...` assignment; the profile's notify assignment takes precedence.

The exit scraper accepts `codex resume <uuid>` and
`codex resume, then select <name> (<uuid>)`, using only the UUID. Save-time
filesystem correlation checks dated rollout directories under
`<codex-home>/sessions/YYYY/MM/DD/`. A rollout matches when its v7 UUID timestamp
is within 30 seconds of task spawn and the first record names the task's working
directory. The search covers the spawn's UTC date plus or minus two days because
the directory date is local time.

### `grok`

Grok accepts a launch-time ID but exposes no injectable live-capture channel. A
bare command therefore receives `--session-id '<uuid>'`, while a canonical
resume command needs no instrumentation.

After exit, the harness scans retained terminal text for the last
`grok -r <uuid>` or `grok --resume <uuid>` hint. Save-time filesystem
correlation checks `<grok-home>/sessions/<encoded-cwd>/<uuid>/`, where `/` is
encoded as `%2F` and `%` as `%25`.

## Resolving conflicting IDs

Several channels can identify different conversations during one task. To make
the result deterministic, `fleetcom` chooses the first available ID in this
order:

1. The exit hint scraped after process exit and PTY-reader EOF.
2. The current capture-file payload.
3. The ID pinned or targeted at spawn.
4. Save-time filesystem correlation, when exactly one store entry matches the
   task and the 30-second spawn window.

Saving and rerunning rewrite accepted commands to one of these forms:

```text
claude --resume '<uuid>'
codex resume '<uuid>'
grok --resume '<uuid>'
```

The program word is preserved as typed. If no valid ID is available, the
original command remains unchanged. A rerun increments the run number before
spawning its replacement, so capture data from the displaced run cannot affect
the new run.

## Security boundary

Every captured value eventually enters a shell command, which makes validation
the security boundary. Accepted IDs contain exactly lowercase hexadecimal
characters in the `8-4-4-4-12` UUID shape. Capture payloads, terminal hints,
store names, and the final command builder all apply the same check. Malformed
values are ignored rather than interpolated.

## Extending capture

Each tool implements the `Harness` trait in [`mod.rs`](mod.rs). The methods keep
detection, evidence collection, and command construction separate:

- `detect` classifies the accepted command shapes.
- `instrument` returns spawn-time arguments, environment entries, and an
  optional pinned ID.
- `parse_capture` reads an ID from hook or notify JSON.
- `scrape_exit` reads an ID from retained terminal text.
- `correlate_fs` finds one matching on-disk session.
- `resume_command` builds the canonical resume form.

The supervisor resolves each harness home from the task's launch environment:
the tool-specific variable first, then `$HOME` plus the tool's dot directory.
That resolved path remains attached to the task for later filesystem
correlation.

## Environment variables

| Variable | Meaning |
| -- | -- |
| `FLEETCOM_RUNTIME_DIR` | Explicit capture-asset root as well as the daemon runtime override. |
| `FLEETCOM_CAPTURE_FILE` | Per-run capture file used by the injected hook or notifier. |
| `FLEETCOM_NOTIFY_CHAIN` | Newline-joined argv for the configured Codex notifier; empty when none is active. |
| `CLAUDE_CONFIG_DIR` | Claude home used for transcript correlation; defaults to `$HOME/.claude`. |
| `CODEX_HOME` | Codex home used for notify routing and rollout correlation; defaults to `$HOME/.codex`. |
| `GROK_HOME` | Grok home used for session-directory correlation; defaults to `$HOME/.grok`. |
