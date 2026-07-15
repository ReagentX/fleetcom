# Agent session resume

Saving a `fleetcom` session preserves commands, not the application state behind them. For agent CLIs, that means relaunching a bare `claude`, `codex`, or `grok` command starts another conversation. `fleetcom` associates an ID with the task, then uses it when saving a [session recipe](../../docs/sessions.md) or rerunning a finished task with `r`.

For direct spawns and session loads, capture flags affect only the string passed to `$SHELL -c`; the task retains the requested command for display and later saves. A rerun stores and displays the generated resume command. When capture succeeds, saved and rerun commands use forms such as `claude --resume '<id>'`, `codex resume '<id>'`, or `grok --resume '<id>'`. When capture fails or produces an ambiguous result, `fleetcom` keeps the task's command unchanged.

## How capture works

Each instrumented task gets `task-<id>.json` under the capture root (see [environment variables](#environment-variables)). For Claude and Codex capture, `fleetcom` exposes the path through `FLEETCOM_CAPTURE_FILE`; the injected hook or notifier overwrites the file with JSON containing the conversation ID.

The first supported task for a capture root installs `claude-settings.json` with mode `0600` and `codex-notify.sh` with mode `0700`. The root uses mode `0700`. Installation also removes existing `task-*.json` files so reused numeric task IDs cannot consume payloads left by a stopped daemon.

### `claude`

A fresh launch without session-selection flags receives both arguments below when UUID generation succeeds. A resuming launch receives the `--settings` addition but no new session ID. `fleetcom` does not add the generated settings file when the command already has `--settings`.

```
--session-id '<new v4 UUID>' --settings '<root>/claude-settings.json'
```

`--session-id` pins the ID before the child produces output. `fleetcom` does not add it when the command already contains `--resume`, `--continue`, `--fork-session`, or `--session-id`. The generated settings file defines one `SessionStart` hook:

```json
{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"cat > \"$FLEETCOM_CAPTURE_FILE\""}]}]}}
```

`claude` runs the hook with the task environment and sends a JSON payload (`session_id`, `hook_event_name`, `source`, …) on stdin. The hook fires for `startup`, `resume`, `clear`, and `compact`, so each event replaces the capture file with the current ID. An existing `--settings` argument disables only this hook; launch-time pinning, exit scraping, and store correlation remain available.

Two fallback channels require no injection. After exit, `fleetcom` scans the final viewport and scrollback for the last `claude --resume <uuid>` hint. The scan waits for both the process-exit latch and reader-thread EOF; as a result, the terminal grid contains all child output before scraping begins. At save time, `fleetcom` can also inspect `<claude-home>/projects/<cwd-slug>/<uuid>.jsonl`, where `<cwd-slug>` is the absolute working directory with `/` and `.` replaced by `-`.

### `codex`

`codex` does not expose an ID that `fleetcom` can choose at launch. `fleetcom` instead appends a `notify` override:

```
-c 'notify=["<root>/codex-notify.sh"]'
```

After each turn, `codex` invokes the program with notification JSON as its final argument. The script writes that argument to `$FLEETCOM_CAPTURE_FILE`, replacing the previous payload; if the variable is unset, it skips the write. `fleetcom` accepts only `"type":"agent-turn-complete"` payloads and reads the ID from `thread-id`.

When `<codex-home>/config.toml` or the effective profile's `<codex-home>/<profile>.config.toml` assigns `notify` a one-line TOML array of basic strings, `fleetcom` injects its script and passes the displaced argv through `FLEETCOM_NOTIFY_CHAIN`, newline-joined. After writing the capture file, the script execs that argv with the notification JSON appended. The profile file overrides the base file; the last command-line `-p`/`--profile` value selects the profile, or the first line-based `profile = "name"` assignment in `config.toml` applies when the command has no profile flag.

Two cases skip the override and capture environment. A `-c notify=` or `--config notify=` in the command is not overridden. Configuration also remains untouched when `notify` is not a one-line array of basic strings, is empty, or contains an empty or newline-bearing element that cannot be represented by the chain encoding.

These checks are line-based, not TOML-aware. A `notify` or `profile` key inside a table therefore counts, and two `notify` assignment lines in one file read as ambiguous and skip injection. Wherever injection is skipped, exit scraping and store correlation remain available.

The exit scraper recognizes both `codex resume <uuid>` and `codex resume, then select <name> (<uuid>)`. It takes the last valid UUID, never the display name. Filesystem correlation searches `<codex-home>/sessions/YYYY/MM/DD/rollout-<local-ts>-<uuid>.jsonl`. Those directories use local dates, so `fleetcom` probes the UTC date ±2 days. A candidate survives only when the v7 UUID's embedded millisecond timestamp falls inside the correlation window and the rollout's first record contains the task's working directory.

### `grok`

`grok` mirrors `claude`'s launch semantics: a fresh launch is pinned with `--session-id '<new v4 UUID>'`, which `fleetcom` skips when the command already contains `--resume`, `--continue`, `--fork-session`, or `--session-id`. The Grok harness has no live capture channel, so an in-TUI `/resume` to another session is available only from the exit scrape.

After exit, `fleetcom` scans the final viewport and scrollback for the last `grok -r <uuid>` or `grok --resume <uuid>` hint. At save time, it can also inspect `<grok-home>/sessions/<encoded-cwd>/`, where `<encoded-cwd>` is the absolute working directory percent-encoded (`/` and `%` encode; `.` stays literal) and each session is a directory named by its UUID.

## Choosing an ID

Several channels can report different IDs during one task. `fleetcom` resolves that ambiguity by using the first available ID in this order:

1. Exit-hint scrape. Available after process exit and reader EOF.
2. Capture file. May be rewritten while the task runs.
3. Spawn-time ID. The `--session-id` `fleetcom` pinned, or the ID already present in the command.
4. On-disk store correlation (save-time only). `fleetcom` searches for a transcript created within ±30 s of the spawn from the task's working directory. Exactly one candidate must match.

## Saved commands

The harness rewrites only the command passed to `resume_command`; the [recipe format](../../docs/sessions.md#format) does not change:

- The stored entry is a plain runnable string: `claude --resume '<id>'` can run directly in a shell.
- `claude`: an existing UUID passed through `--resume` or `-r` is replaced in place; otherwise `--resume '<id>'` is appended. Any `--session-id` flag is removed.
- `codex`: an existing UUID target is replaced. Fresh commands insert `resume '<id>'` after the program, and bare `codex resume` gains the ID. Named targets (`codex resume my-thread`) and self-targeting forms (`codex resume --last`) remain unchanged.
- `grok`: like `claude`, but `grok` rejects a second `--resume` (`the argument '--resume [<SESSION_ID>]' cannot be used multiple times`). A UUID passed through `--resume`/`-r` is replaced in place. A bare `--resume`/`-r` (picker/most-recent form) receives the captured id on the existing flag rather than a duplicate; a bare flag already followed by a non-flag target `grok` would bind is left unchanged. Otherwise `--resume '<id>'` is appended. Any `--session-id` or `-s` flag is removed, because `grok` rejects it alongside `--resume` without `--fork-session`.
- Rerun (`r`) applies the same rewrite and stores the resuming command. Re-detection then treats the task as a resume and does not inject another ID.
- A stale ID fails inside the task's PTY, where the error remains visible. Since the recipe is ordinary JSON, the ID or resume flag can be edited by hand.

## Failure behavior

`fleetcom` preserves the original command whenever capture is unavailable or ambiguous. This retains the command's original behavior; a bare agent command starts a fresh conversation.

| Situation | Behavior |
| -- | -- |
| Outside quotes: <code>\| ; & < > $ # ` ( ) \\</code> or a newline; an unquoted `=` in the first word; unterminated quotes | Not detected. Spawns and saves as the plain command. |
| Inside double quotes: `$`, backtick, or `\` | Not detected. The shell expands or unescapes them, so the classified tokens would diverge from the CLI's argv (`codex "$MODE"` can run `codex exec`). Single-quoted content stays literal and is unaffected. |
| A word that resolves to a standalone `--` | Not detected because appended flags would become prompt text. |
| A top-level flag outside the tool's known flag table (`claude`, `codex`, `grok`) | Not detected because its value cannot be distinguished safely from a subcommand or prompt. A flag added upstream costs capture until the table learns it, never a broken command. |
| Non-conversation subcommands (`claude`: `mcp`, `doctor`, `config`, …; `codex`: `exec`, `login`, `apply`, …; `grok`: `sessions`, `login`, `mcp`, …) | Not detected. |
| Capture assets cannot be installed for the selected root | Spawns untouched. |
| `codex`: `-c notify=` in the command, or a config `notify` value the chain cannot carry | No injection; exit scrape and store correlation remain. A parseable config `notify` chains instead: capture plus the user's notifier. |
| `claude`: the user passes `--settings` | No overlay hook; the pinned ID, exit scrape, and store correlation remain. |
| `grok`: every command | No live capture channel is injected; the pinned ID, exit scrape, and store correlation remain. |
| No ID captured by save time | Store correlation is tried; on failure the original command is stored. |
| Store correlation is ambiguous, or a `claude` transcript lacks a creation time | The original command is stored. |

## Security boundary

Every captured ID eventually enters a shell command. Validation is therefore the security boundary. `fleetcom` accepts exactly `8-4-4-4-12` lowercase hexadecimal characters. Free-text names, paths, and malformed UUIDs return `None`; `resume_command` validates the value again before insertion.

Terminal output is untrusted because the child can print a forged resume hint. Shape validation limits a forgery to another UUID; it cannot introduce shell syntax. Capture payloads and store filenames pass through the same check.

## Adding another harness

Each supported tool implements the `Harness` trait ([`mod.rs`](mod.rs)) and registers in `HARNESSES`. Its eight methods separate detection, capture, correlation, and rewriting:

- `name`: registry identity.
- `home_env_var`: the env var overriding the tool's home root (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, or `GROK_HOME`); resolved from the connection's launch context during instrumentation and save-time correlation.
- `detect`: classify the command, any targeted ID, and whether launch-time pinning is allowed. Blocklisted subcommands and unsupported shell syntax return `None`.
- `instrument`: return the argument suffix, environment pairs, and optional pinned ID. This is also where configuration routing, such as the codex notify chain-or-skip classification, applies.
- `parse_capture`: extract an ID from a capture-file payload.
- `scrape_exit`: extract the last valid ID from final terminal text.
- `correlate_fs`: find a unique ID in the on-disk store. Ambiguity returns `None`.
- `resume_command`: rewrite a command into its resuming form while preserving unrelated bytes.

Each harness must define these tool-specific behaviors:

- ID stability under resume. Whether resume preserves or replaces an ID determines when the capture file must be rewritten and where the ID ranks in the precedence chain.
- Live capture support. Whether a hook, notify program, or equivalent can be injected without replacing the user's configuration.
- Exit-hint shape. The plain-text pattern recognized after terminal emulation.
- On-disk store layout. The path scheme, timestamp semantics, and working-directory metadata used for correlation.
- Launch-time pinning. Whether an ID can be fixed at spawn, and which flag combinations reject it.

The implementation has three test boundaries:

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
