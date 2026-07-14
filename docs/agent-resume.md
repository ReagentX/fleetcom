# Agent session resume

Saved sessions normally relaunch commands, but relaunching an agent CLI does not identify its conversation. Fleetcom addresses this for `claude` and `codex` by capturing a conversation ID during execution. Saving a [session recipe](sessions.md) or rerunning a finished task with `r` then uses that ID.

The instrumentation exists only in the string passed to `$SHELL -c`; the dashboard still shows the command the user entered. When an ID is available, the saved recipe contains `claude --resume '<id>'` or `codex resume '<id>'`. Without one, Fleetcom retains the original command.

## How capture works

An instrumented task gets `task-<id>.json` under the capture root (see [environment variables](#environment-variables)). `FLEETCOM_CAPTURE_FILE` points the child at this file, and the injected hook or notifier overwrites it with JSON containing the conversation ID.

Fleetcom installs two shared assets when the first supported task needs them: `claude-settings.json` with mode `0600`, and `codex-notify.sh` with mode `0700`. The root uses mode `0700`. Installation also removes existing `task-*.json` files so reused numeric task IDs cannot read an earlier payload.

### claude

By default, a fresh launch with no session-selection flags gets both additions below. A resuming launch gets only `--settings`. If the command already contains `--settings`, Fleetcom does not add the overlay.

```
--session-id '<new v4 UUID>' --settings '<root>/claude-settings.json'
```

`--session-id` pins the ID at launch, before the child produces output. `claude` rejects this flag alongside `--resume` or `--continue`, so Fleetcom adds it only to fresh launches. The `--settings` overlay is additive: the user's hooks remain active, and Fleetcom adds one `SessionStart` hook:

```json
{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"cat > \"$FLEETCOM_CAPTURE_FILE\""}]}]}}
```

`claude` passes the hook a JSON payload (`session_id`, `hook_event_name`, `source`, …) on stdin and runs it with the task environment. The hook fires for `startup`, `resume`, `clear`, and `compact`, replacing the capture file with the current ID. This is how Fleetcom tracks conversation changes during a task. If the command already contains `--settings`, Fleetcom leaves that setting alone; launch-time pinning, exit scraping, and store correlation still apply.

Two fallback channels require no injection. After a clean exit, Fleetcom scans the final viewport and scrollback for the last `Resume this session with:` / `claude --resume <uuid>` hint. It waits for both the process-exit latch and reader-thread EOF, so the terminal grid contains all output before the scan begins. Save-time correlation can also inspect `<claude-home>/projects/<cwd-slug>/<uuid>.jsonl`, where `<cwd-slug>` is the absolute working directory with `/` and `.` replaced by `-`.

### codex

Fleetcom cannot choose a codex ID at launch. Instead, it appends a `notify` override:

```
-c 'notify=["<root>/codex-notify.sh"]'
```

`codex` invokes the program after each turn and passes notification JSON as the final argument. The script replaces `$FLEETCOM_CAPTURE_FILE` with that argument. If the variable is unset, the script exits successfully without writing. Fleetcom parses only `"type":"agent-turn-complete"` payloads and reads the ID from `thread-id`.

Fleetcom skips the override and capture environment when the command already contains `-c notify=` or `--config notify=`, when `<codex-home>/config.toml` contains an uncommented `notify` assignment, or when the effective profile's `<codex-home>/<profile>.config.toml` does. The effective profile is the command line's `-p`/`--profile` value, or failing that a top-level `profile = "name"` key in `config.toml`. Because the checks are line-based rather than TOML-aware, they also treat a `notify` or `profile` key inside a table as configured. Exit scraping and store correlation remain available.

The exit scraper recognizes `codex resume <uuid>` and `codex resume, then select <name> (<uuid>)`, taking the last valid UUID rather than the display name. Filesystem correlation searches `<codex-home>/sessions/YYYY/MM/DD/rollout-<local-ts>-<uuid>.jsonl`. Because the directories use local dates, Fleetcom probes the UTC date ±2 days. It then compares the v7 UUID's embedded millisecond timestamp and requires the first `session_meta` record to contain the task's working directory.

## ID precedence

When Fleetcom saves a recipe or reruns a finished task, it uses the first available ID in this order:

1. **Exit-hint scrape.** Authored by the tool *as it exits*, so it postdates every capture-file write: SessionStart hooks and per-turn notify all land mid-run.
2. **Capture file.** Outranks the spawn-time ID because the hook rewrites it on every session move: resume, `/clear`, compact.
3. **Spawn-time ID.** The `--session-id` Fleetcom pinned, or the ID already present in the command.
4. **On-disk store correlation** (save-time only). The tool's session store is searched for a transcript created within ±30 s of the spawn, from the task's working directory. One match is required: several in-window candidates cannot be told apart, and resuming the *wrong* conversation is worse than resuming none.

## Recipe semantics

Save rewrites the command through the harness's `resume_command`; nothing else about the [recipe format](sessions.md#format) changes:

- The stored entry is a plain runnable string: `claude --resume '<id>'` can run directly in a shell.
- `claude`: an existing `--resume` value is replaced in place; otherwise ` --resume '<id>'` is appended. A user-pinned `--session-id` is removed because `claude` rejects it alongside `--resume`.
- `codex`: an existing UUID target is replaced; otherwise `resume '<id>'` is inserted after the program, ahead of flags and the prompt. Named targets (`codex resume my-thread`) and self-targeting forms (`codex resume --last`) remain unchanged.
- Rerun (`r`) applies the same rewrite and stores the resuming command. Re-detection then treats the task as a resume and does not inject another ID.
- A stale ID fails inside the task's PTY, where the tool's error remains visible. The recipe is ordinary JSON, so the ID or resume flag can be edited by hand.

## Degradation

Fleetcom preserves the original command whenever capture is unavailable or ambiguous. The command still runs, but it starts a fresh conversation.

| Situation | Behavior |
| -- | -- |
| Shell constructs in the command: <code>\| ; & < > $ # ` ( ) \\</code>, newlines, a leading `VAR=` prefix, unterminated quotes | Not detected. Spawns and saves as the plain command; no instrumentation at all. |
| An unquoted `#`, or a word that resolves to a standalone `--` | Not detected. `#` comments out appended flags; `--` turns them into prompt text. Either would record a session the command never ran. |
| codex: a top-level flag outside the known table (e.g. one added upstream after this release) | Not detected. Prevents misreading the flag's value as the subcommand and corrupting the rewrite; capture returns once the table learns the flag. |
| Non-conversation subcommands (claude: `mcp`, `doctor`, `config`, …; codex: `exec`, `login`, `apply`, …) | Not detected. |
| Capture assets cannot install and no asset set is active | Spawns untouched. |
| codex: the user routes `notify` (command line or `config.toml`) | No injection; exit scrape and store correlation remain. |
| `claude`: the user passes `--settings` | No overlay hook; the pinned ID, exit scrape, and store correlation remain. |
| No ID captured by save time | Store correlation is tried; on failure the original command is stored. |
| Store correlation is ambiguous, or a `claude` transcript lacks a creation time | The original command is stored. |

## Security

Every captured ID eventually enters a shell command, so validation is the security boundary. Fleetcom accepts exactly `8-4-4-4-12` lowercase hexadecimal characters. Free-text names, paths, and malformed UUIDs return `None`, and `resume_command` validates the value again before inserting it.

Terminal output is untrusted because the child can print a forged resume hint. Shape validation limits a forgery to another UUID; it cannot introduce shell syntax. Capture payloads and store filenames pass through the same check.

## Adding a harness

Each supported tool implements the `Harness` trait ([`src/harness/mod.rs`](../src/harness/mod.rs)) and registers in `HARNESSES`. The eight methods divide responsibility as follows:

- `name`: registry identity (test routing assertions).
- `home_env_var`: the env var overriding the tool's home root (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`); resolved from the *task's* launch env so instrumentation and correlation inspect the store the child actually uses, not the daemon's.
- `detect`: classify the command, any targeted ID, and whether launch-time pinning is allowed. Blocklisted subcommands and unsupported shell syntax return `None`.
- `instrument`: return the argument suffix, environment pairs, and optional pinned ID. This is also where configuration guards, such as the codex notifier check, apply.
- `parse_capture`: extract an ID from a capture-file payload.
- `scrape_exit`: extract the last valid ID from final terminal text.
- `correlate_fs`: find a unique ID in the on-disk store. Ambiguity returns `None`.
- `resume_command`: rewrite a command into its resuming form while preserving unrelated bytes.

Each harness must define these tool-specific behaviors:

- **ID stability under resume.** Whether resume preserves or replaces an ID determines when the capture file must be rewritten and where the ID ranks in the precedence chain.
- **A live capture channel.** A hook, notify program, or equivalent that Fleetcom can inject without replacing the user's configuration.
- **Exit-hint shape.** What the tool prints on a clean exit, byte for byte, SGR included.
- **On-disk store layout.** The path scheme, timestamp semantics, and working-directory metadata used for correlation.
- **Launch-time pinning.** Whether an ID can be fixed at spawn, and which flag combinations reject it.

The implementation is tested at three levels:

- **Corpus fixture:** replay a recorded PTY session ending in the tool's exit hint ([`tests/corpus/README.md`](../tests/corpus/README.md)) and assert that the scraper recovers the ID from retained terminal text.
- **Unit:** the supervisor tests drive spawn/save/rerun against stub scripts and scratch home dirs ([`src/supervisor.rs`](../src/supervisor.rs), test module).
- **Daemon:** [`tests/daemon_resume.rs`](../tests/daemon_resume.rs) covers spawn, save, and load through the daemon protocol, including a daemon restart.

## Environment variables

| Variable | Read from | Meaning |
| -- | -- | -- |
| `FLEETCOM_RUNTIME_DIR` | client env (hello) | Capture-asset root, used verbatim. When unset, Fleetcom uses the platform runtime directory (or `<cache>/fleetcom/run`) plus a discriminator derived from the sessions root. |
| `FLEETCOM_CAPTURE_FILE` | internal child env | Task capture file used by the injected hook or notifier. |
| `CLAUDE_CONFIG_DIR` | client env (hello) | `claude` home override used for transcript correlation. Defaults to `~/.claude`. |
| `CODEX_HOME` | client env (hello) | `codex` home override used for the notifier guard and rollout correlation. Defaults to `~/.codex`. |
