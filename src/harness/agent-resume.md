# Agent session resume

Saving a `fleetcom` session preserves commands, not the application state behind them. For agent CLIs, that means relaunching a bare `claude`, `codex`, or `grok` command starts another conversation. `fleetcom` associates an ID with the task, then uses it when saving a [session recipe](../../docs/sessions.md) or rerunning a finished task with `r`.

For direct spawns and session loads, capture flags affect only the string passed to `$SHELL -c`; the task retains the requested command for display and as the source for saved commands. A rerun stores and displays the generated resume command. When capture succeeds, saved and rerun commands use the forms `claude --resume '<id>'`, `codex resume '<id>'`, and `grok --resume '<id>'`. When capture fails or produces an ambiguous result, `fleetcom` keeps the task's command unchanged.

## What `fleetcom` rewrites

A command participates in capture only when it is a shape `fleetcom` itself authors:

- The bare program word: `claude`, `codex`, or `grok`. A path form (`/usr/local/bin/claude`) matches by basename when the word carries no shell syntax.
- The canonical resume form, ending the line: `claude --resume <uuid>`, `grok --resume <uuid>`, or `codex resume <uuid>`. The UUID may be single-quoted — `fleetcom`'s own output — or bare, as retyped from a hint. It must pass the strict validator below.

That is six shapes in total. Everything else is opaque: no instrumentation, no rewrite, and the recipe stores the user's bytes verbatim. The policy is deliberate — `fleetcom` only rewrites what it authored. Flagged launches (`claude --model opus`), prompt launches (`claude 'fix the tests'`), alternate resume spellings (`-r`, `--resume=<uuid>`), subcommands, and hand-augmented resume entries get no capture at all; earlier revisions parsed many of these through per-CLI flag tables and rewrote them in place. As the maintainer put it: "if a user wants a command that specific we probably shouldn't rewrite it anyway."

## How capture works

Each instrumented task gets `task-<id>.json` under the capture root (see [environment variables](#environment-variables)). For Claude and Codex capture, `fleetcom` exposes the path through `FLEETCOM_CAPTURE_FILE`; the injected hook or notifier overwrites the file with JSON containing the conversation ID.

The first supported task for a capture root installs `claude-settings.json` with mode `0600` and `codex-notify.sh` with mode `0700`. The root uses mode `0700`. Before allocating task paths, installation removes existing `task-*.json` files so reused numeric task IDs cannot consume old payloads.

### `claude`

A bare launch receives both arguments below when UUID generation succeeds. The canonical resume form receives only the `--settings` addition; it already targets its conversation.

```
--session-id '<new v4 UUID>' --settings '<root>/claude-settings.json'
```

`--session-id` pins the ID before the child produces output. The generated settings file defines one `SessionStart` hook:

```json
{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"cat > \"$FLEETCOM_CAPTURE_FILE\""}]}]}}
```

`claude` runs the hook with the task environment and sends a JSON payload (`session_id`, `hook_event_name`, `source`, …) on stdin. The hook fires for `startup`, `resume`, `clear`, and `compact`, so each event replaces the capture file with the current ID.

Two fallback channels require no injection. After exit, `fleetcom` scans the final viewport and scrollback for the last `claude --resume <uuid>` hint. The scan waits for both the process-exit latch and reader-thread EOF, so the terminal grid holds every child byte before scraping begins. At save time, `fleetcom` can also inspect `<claude-home>/projects/<cwd-slug>/<uuid>.jsonl`, where `<cwd-slug>` is the absolute working directory with `/` and `.` replaced by `-`.

### `codex`

`codex` does not expose an ID that `fleetcom` can choose at launch. `fleetcom` instead appends a `notify` override to both accepted shapes:

```
-c 'notify=["<root>/codex-notify.sh"]'
```

After each turn, `codex` invokes the program with notification JSON as its final argument. The script writes that argument to `$FLEETCOM_CAPTURE_FILE`, replacing the previous payload; if the variable is unset, it skips the write. `fleetcom` accepts only `"type":"agent-turn-complete"` payloads and reads the ID from `thread-id`.

When `<codex-home>/config.toml` or the effective profile's `<codex-home>/<profile>.config.toml` assigns `notify` a one-line TOML array of basic strings, `fleetcom` injects its script and passes the displaced argv through `FLEETCOM_NOTIFY_CHAIN`, newline-joined. After writing the capture file, the script execs that argv with the notification JSON appended. The profile file overrides the base file; the first line-based `profile = "name"` assignment in `config.toml` selects the profile.

Configuration remains untouched when `notify` is not a one-line array of basic strings, is empty, or contains an empty or newline-bearing element that cannot be represented by the chain encoding.

These checks are line-based, not TOML-aware: a `notify` or `profile` key inside a table counts, and two `notify` assignment lines in one file read as ambiguous and skip injection. Exit scraping and store correlation remain available without injection.

Per-turn notification is also the earliest possible signal after an in-TUI `/resume`: at the moment of the switch itself, `codex` records nothing observable outside the process. Verified against live state (2026-07-15, codex 0.144.3): the resumed thread's rollout file keeps its old mtime (the append-open is lazy), `threads.updated_at` in the state database (`state_5.sqlite`) keeps its old value, hooks are trust-gated, and no query interface exists. A save between the switch and the next completed turn therefore stores the plain command by construction — do not re-chase these channels.

The exit scraper recognizes both `codex resume <uuid>` and `codex resume, then select <name> (<uuid>)`. It takes the last valid UUID, never the display name. Filesystem correlation searches `<codex-home>/sessions/YYYY/MM/DD/rollout-<local-ts>-<uuid>.jsonl`. Those directories use local dates, so `fleetcom` probes the UTC date ±2 days. A candidate survives only when the v7 UUID's embedded millisecond timestamp falls inside the correlation window and the rollout's first record contains the task's working directory.

### `grok`

Like `claude`, a bare `grok` launch is pinned with `--session-id '<new v4 UUID>'`. The canonical resume form receives nothing. The Grok harness has no live capture channel, so an in-TUI `/resume` to another session is available only from the exit scrape.

After exit, `fleetcom` scans the final viewport and scrollback for the last `grok -r <uuid>` or `grok --resume <uuid>` hint. At save time, it can also inspect `<grok-home>/sessions/<encoded-cwd>/`, where `<encoded-cwd>` is the absolute working directory percent-encoded (`/` and `%` encode; `.` stays literal) and each session is a directory named by its UUID.

## Choosing an ID

Several channels can report different IDs during one task. `fleetcom` resolves that ambiguity by using the first available ID in this order:

1. Exit-hint scrape. Available after process exit and reader EOF.
2. Capture file. May be rewritten while the task runs.
3. Spawn-time ID. The `--session-id` `fleetcom` pinned, or the ID targeted by the canonical resume form.
4. On-disk store correlation (save-time only). `fleetcom` searches for a transcript created within ±30 s of the spawn from the task's working directory. Exactly one candidate must match.

## Saved commands

Saving and rerunning use `resume_command` to rewrite the task command; the [recipe format](../../docs/sessions.md#format) remains unchanged:

- The stored entry is a plain runnable string: `claude --resume '<id>'` can run directly in a shell.
- Rewriting is pure string construction: the program word as typed, the resume selector, and the quoted ID — `claude --resume '<id>'`, `codex resume '<id>'`, `grok --resume '<id>'`. A bare command gains the selector and ID; a canonical resume form regenerates with the new ID.
- Rerun (`r`) applies the same rewrite and stores the resuming command. Re-detection then reads the stored command as the canonical resume form and does not inject another ID.
- A stale ID fails inside the task's PTY, where the error remains visible. Since the recipe is ordinary JSON, the ID can be edited by hand — but a hand-augmented entry (extra flags, a prompt) is no longer a shape `fleetcom` authored, so it runs and saves verbatim from then on.

## Failure behavior

`fleetcom` preserves the original command whenever a command is opaque or capture is unavailable or ambiguous: a bare agent command starts a fresh conversation.

| Situation | Behavior |
| -- | -- |
| Not one of the six authored shapes (bare `claude`/`codex`/`grok`, or their canonical resume forms) | Opaque: spawns and saves verbatim, with no instrumentation and no rewrite. This covers flagged launches, prompt launches, alternate resume spellings (`-r`, `--resume=`), subcommands, shell syntax, and hand-augmented resume entries. |
| Capture assets cannot be installed for the selected root | Spawns untouched. |
| `codex`: a config `notify` value the chain cannot carry | No injection; exit scrape and store correlation remain. A parseable config `notify` chains instead: capture plus the user's notifier. |
| `claude`: every accepted launch | The overlay hook, pinned ID (bare only), exit scrape, and store correlation apply. |
| `grok`: every accepted launch | No live capture channel is injected; the pinned ID (bare only), exit scrape, and store correlation remain. |
| No ID captured by save time | Store correlation is tried; on failure the original command is stored. |
| Store correlation is ambiguous, or a `claude` transcript lacks a creation time | The original command is stored. |

## Security boundary

Every captured ID eventually enters a shell command: validation is the security boundary. `fleetcom` accepts exactly `8-4-4-4-12` lowercase hexadecimal characters. Free-text names, paths, and malformed UUIDs return `None`; `resume_command` validates the value again before insertion.

Terminal output is untrusted because the child can print a forged resume hint. Shape validation limits a forgery to another UUID; it cannot introduce shell syntax. Capture payloads and store filenames pass through the same check.

## Adding another harness

Each supported tool implements the `Harness` trait ([`mod.rs`](mod.rs)) and registers in `HARNESSES`. Its eight methods separate detection, capture, correlation, and rewriting:

- `name`: registry identity.
- `home_env_var`: the env var overriding the tool's home root (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, or `GROK_HOME`); resolved from the connection's launch context during instrumentation and save-time correlation.
- `detect`: classify the command as the bare program word or the canonical resume form; anything else returns `None`.
- `instrument`: return the argument suffix, environment pairs, and optional pinned ID. This is also where configuration routing, such as the codex notify chain-or-skip classification, applies.
- `parse_capture`: extract an ID from a capture-file payload.
- `scrape_exit`: extract the last valid ID from final terminal text.
- `correlate_fs`: find a unique ID in the on-disk store. Ambiguity returns `None`.
- `resume_command`: rewrite an accepted command into its canonical resuming form.

Each harness must define these tool-specific behaviors:

- ID stability under resume. Whether resume preserves or replaces an ID determines when the capture file must be rewritten and where the ID ranks in the precedence chain.
- Live capture support. Whether a hook, notify program, or equivalent can be injected without replacing the user's configuration.
- Exit-hint shape. The plain-text pattern recognized after terminal emulation.
- On-disk store layout. The path scheme, timestamp semantics, and working-directory metadata used for correlation.
- Launch-time pinning. Whether an ID can be fixed at the spawn of a bare launch.

Tests cover three boundaries:

- Corpus fixture: replay a recorded PTY session ending in the tool's exit hint ([`tests/corpus/README.md`](../../tests/corpus/README.md)) and assert that the scraper recovers the ID from retained terminal text.
- Unit: the supervisor tests drive spawn/save/rerun against stub scripts and scratch home dirs ([`src/supervisor.rs`](../supervisor.rs), test module).
- Daemon: [`tests/daemon_resume.rs`](../../tests/daemon_resume.rs) covers spawn, save, and load through the daemon protocol, including a daemon restart.

## Environment variables

| Variable | Read from | Meaning |
| -- | -- | -- |
| `FLEETCOM_RUNTIME_DIR` | client env (hello) | Capture-asset root, used verbatim. When unset, `fleetcom` uses the platform runtime directory (or `<cache>/fleetcom/run`) plus a discriminator derived from the sessions root. |
| `FLEETCOM_CAPTURE_FILE` | internal child env | Task capture file used by the injected hook or notifier. |
| `FLEETCOM_NOTIFY_CHAIN` | internal child env | Displaced `codex` notify argv, newline-joined. The injected notify script execs it, payload appended, after the capture write. |
| `CLAUDE_CONFIG_DIR` | client env (hello) | `claude` home override used for transcript correlation. Defaults to `~/.claude`. |
| `CODEX_HOME` | client env (hello) | `codex` home override used for notify routing and rollout correlation. Defaults to `~/.codex`. |
| `GROK_HOME` | client env (hello) | `grok` home override used for session-store correlation. Defaults to `~/.grok`. |
