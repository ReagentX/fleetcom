# Agent session resume

A recipe that stores `claude` restarts the program, not the conversation: loading it opens an empty session, and the work the agent had in flight is a manual `--resume` away. Fleetcom closes that gap for the AI-agent CLIs it recognizes — `claude` and `codex`. Spawning one instruments the launch so the conversation's session id is captured while the task runs; saving a [session recipe](sessions.md) rewrites the command into the one that resumes that conversation (`claude --resume '<id>'`, `codex resume '<id>'`); loading the recipe — or rerunning the finished task with `r` — re-enters it.

The instrumentation is invisible: only the string handed to `$SHELL -c` carries it. The command the dashboard displays and the recipe stores stay exactly what the user typed, and every step degrades to the plain command when capture is impossible.

## How capture works

Both tools get a per-task capture file, `task-<id>.json`, under the capture root (see [environment variables](#environment-variables)). The child's environment names it in `FLEETCOM_CAPTURE_FILE`; an injected hook or notify program overwrites it with a JSON payload carrying the current session id. Two shared assets — `claude-settings.json` (mode `0600`) and `codex-notify.sh` (mode `0700`) — are installed once per daemon start under the root (mode `0700`), which also sweeps stale `task-*.json` files: task ids restart at 1 per daemon, so an orphaned capture file would be misread as a *new* task's id.

### claude

A fresh launch — no `--resume`/`--continue`/`--fork-session`/`--session-id` of the user's own — gets two appended flags; a resuming one gets only the second:

```
--session-id '<new v4 uuid>' --settings '<root>/claude-settings.json'
```

`--session-id` pins the id at launch, so it is known before the child prints a byte. claude rejects it alongside `--resume`/`--continue`, which is why resuming launches never receive one. The `--settings` overlay layers *additively* onto the user's configuration — their own hooks still fire — and delivers one SessionStart hook:

```json
{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"cat > \"$FLEETCOM_CAPTURE_FILE\""}]}]}}
```

claude pipes the hook a JSON payload (`session_id`, `hook_event_name`, `source`, …) on stdin and runs it with the task's environment. The hook fires on the sources `startup`, `resume`, `clear`, and `compact`, each time overwriting the capture file with the now-current id. That is what tracks drift: `/clear` mints a new session id mid-task, an in-tool resume switches conversations, and the capture file always names the one the task actually ended on. When the user's command already carries `--settings`, fleetcom does not add a second one (they would fight); the pinned id, the exit scrape, and store correlation remain.

Two more channels need no injection. On a clean exit claude prints `Resume this session with:` / `claude --resume <uuid>`; fleetcom scrapes the *last* such hint from the task's final terminal text (viewport plus scrollback), once, after the exit latch and reader-thread EOF both close. And transcripts land in `<claude-home>/projects/<cwd-slug>/<uuid>.jsonl` (`<cwd-slug>` is the absolute working directory with `/` and `.` replaced by `-`), which save-time correlation can search.

### codex

codex has no launch-time id pinning, and its hook system is trust-gated — a config change the user must approve interactively, which an invisible injection cannot do. The `notify` config override is the one injection channel that works, so a codex spawn appends:

```
-c 'notify=["<root>/codex-notify.sh"]'
```

codex invokes the program after each turn with the notification JSON as its final argument; the script writes that argument verbatim over `$FLEETCOM_CAPTURE_FILE` (and exits 0 without writing when the variable is unset — a run outside fleetcom). Only payloads with `"type":"agent-turn-complete"` are parsed; the id is the `thread-id` field.

The override is skipped entirely — no flag, no capture env — when the user already routes notify anywhere: a `-c notify=`/`--config notify=` on the command line, or an active top-level `notify` assignment in `<codex-home>/config.toml`. The reason is precedence: the CLI override outranks the config file, so injecting would silently disable the user's own notifier, and breaking configured notifications to gain capture is the wrong trade. The config check is line-based on purpose — a `notify` key inside a TOML table matches too, which only errs toward not injecting. The exit scrape and store correlation still operate.

The scrape reads both exit-hint shapes, taking the last: `codex resume <uuid>`, and `codex resume, then select <name> (<uuid>)` — only the parenthesized id is trusted, never the name. Correlation searches rollout files under `<codex-home>/sessions/YYYY/MM/DD/rollout-<local-ts>-<uuid>.jsonl`: the day directories are named by local date, so the probe spans the UTC date ±2 days; the id is a v7 uuid whose embedded millisecond instant marks session start (the filename's own timestamp is local wall-clock, and file creation can lag — rollouts are written lazily); the rollout's first line, its `session_meta` record, must name the task's working directory.

## Id precedence

When a recipe is saved (and when `r` reruns a finished task), the id comes from the first channel that has one:

1. **Exit-hint scrape.** Authored by the tool *as it exits*, so it postdates every capture-file write — SessionStart hooks and per-turn notify all land mid-run.
2. **Capture file.** Outranks the spawn-time id because the hook rewrites it on every session move: resume, `/clear`, compact.
3. **Spawn-time id.** The `--session-id` fleetcom pinned, or the id the user's own flags already targeted.
4. **On-disk store correlation** (save-time only). The tool's session store is searched for a transcript created within ±30 s of the spawn, from the task's working directory. One match is required: several in-window candidates cannot be told apart, and resuming the *wrong* conversation is worse than resuming none.

## Recipe semantics

Save rewrites the command through the harness's `resume_command`; nothing else about the [recipe format](sessions.md#format) changes:

- The stored entry is a plain runnable string — `claude --resume '<id>'` works pasted into any shell, and fleetcom versions predating this feature load it untouched.
- claude: an existing `--resume` value is replaced in place; otherwise ` --resume '<id>'` is appended; a user-pinned `--session-id` is dropped (claude rejects it alongside `--resume`). codex: an existing uuid target is replaced; otherwise `resume '<id>'` slots in after the program, ahead of flags and prompt. Named targets (`codex resume my-thread`) and self-targeting forms (`codex resume --last`) are the user's choice and stand.
- Rerun (`r`) rewrites the stored command the same way, and the resuming command *becomes* the stored command: re-detection classifies it as resuming, so the respawn injects only the capture channel, never a second id.
- A stale id — the tool deleted or expired the session — fails inside the task's PTY, where the tool's own error is visible. The recipe is ordinary JSON; edit the id or strip the resume flag by hand.

## Degradation

Anything the feature cannot account for saves the plain command; the recipe still works, it just starts fresh.

| Situation | Behavior |
| -- | -- |
| Shell constructs in the command: <code>\| ; & < > $ ` ( ) \\</code>, newlines, a leading `VAR=` prefix, unterminated quotes | Not detected. Spawns and saves as the plain command; no instrumentation at all. |
| Non-conversation subcommands (claude: `mcp`, `doctor`, `config`, …; codex: `exec`, `login`, `apply`, …) | Not detected. |
| Capture assets cannot install | Spawns untouched. |
| codex: the user routes `notify` (command line or `config.toml`) | No injection; exit scrape and store correlation remain. |
| claude: the user passes `--settings` | No overlay hook; the pinned id, exit scrape, and store correlation remain. |
| No id captured by save time | Store correlation is tried; on failure the plain command is stored. |
| Store correlation is ambiguous (several in-window transcripts) or the store lacks creation times | `None` by design — the wrong resume is worse than none. |

## Security

Every id these channels produce is spliced into a shell command when the recipe loads. The gate is a strict shape check: exactly `8-4-4-4-12` lowercase hex, nothing else — free-text session names, paths, and anything longer or shorter yield `None` at the channel, and `resume_command` re-validates before splicing (defense in depth).

The gate exists because terminal output is untrusted input: the exit scrape reads bytes the child authored, and a hostile child can print anything, including a forged resume hint. Under the gate, the worst a forgery achieves is naming a different uuid — never shell syntax. Capture payloads and store filenames pass the same check.

## Adding a harness

One tool = one implementation of the `Harness` trait ([`src/harness/mod.rs`](../src/harness/mod.rs)), registered in `HARNESSES`. Eight methods, each with a single owner:

- `name` — registry identity (test routing assertions).
- `home_env_var` — the env var overriding the tool's home root (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`); resolved from the *task's* launch env so instrumentation and correlation inspect the store the child actually uses, not the daemon's.
- `detect` — classify a recipe command string: this tool or not, the id the user's flags already target, whether launch-time pinning is allowed. Refuse blocklisted subcommands and anything the tokenizer cannot fully account for; `None` makes the whole feature no-op.
- `instrument` — the spawn-time additions: an args suffix (every spliced path shell-quoted), env pairs, and the pinned id when one exists. This is where user-config guards live (codex's notify check).
- `parse_capture` — id out of a capture-file payload.
- `scrape_exit` — id out of final terminal text. Take the *last* hint; validate through the strict-uuid gate.
- `correlate_fs` — best-effort id from the on-disk store. Ambiguity must return `None`.
- `resume_command` — rewrite a command into its resuming form, preserving every byte you don't own.

Before writing any of it, verify the tool's actual behavior empirically — the contracts above encode observed behavior, not documentation:

- **Id stability under resume.** Does resuming keep the id or mint a new one? This decides whether the capture file must be rewritten mid-run and where the id ranks in the precedence chain.
- **A live capture channel.** A hook, notify program, or equivalent that fleetcom can inject *invisibly and additively* — codex's trust-gated hooks are the cautionary case.
- **Exit-hint shape.** What the tool prints on a clean exit, byte for byte, SGR included.
- **On-disk store layout.** Path scheme, which timestamp marks what (codex's filenames are local time; the v7 id is UTC), and whether the working directory is recorded.
- **Launch-time pinning.** Whether an id can be fixed at spawn, and which flag combinations reject it.

Test at three levels, following the existing patterns:

- **Corpus fixture:** record a real PTY session ending in the tool's exit hint ([`tests/corpus/README.md`](../tests/corpus/README.md)) and assert the scrape recovers the id from the emulator's retained text (`corpus_scrape_recovers_the_exit_hint_id` in each harness module).
- **Unit:** the supervisor tests drive spawn/save/rerun against stub scripts and scratch home dirs ([`src/supervisor.rs`](../src/supervisor.rs), test module).
- **Daemon:** [`tests/daemon_resume.rs`](../tests/daemon_resume.rs) runs the full wire story — stub CLIs heading the hello's `PATH`, a fully explicit hello env so every store resolves into scratch, spawn → save → load across a real socket and a daemon restart.

## Environment variables

| Variable | Read from | Meaning |
| -- | -- | -- |
| `FLEETCOM_RUNTIME_DIR` | client env (hello) | Capture-asset root, used verbatim. Unset: the platform runtime dir (macOS: `<cache>/fleetcom/run`) plus a per-daemon discriminator hashed from the sessions root — two daemons must not share a root, because the install sweep would delete each other's live capture files and task ids collide across daemons. |
| `FLEETCOM_CAPTURE_FILE` | — (internal) | Set by fleetcom on instrumented children; names the task's capture file. The injected hook and notify script write through it and do nothing when it is absent. |
| `CLAUDE_CONFIG_DIR` | client env (hello) | claude's home override; correlation reads `<home>/projects/…`. Default `~/.claude`. |
| `CODEX_HOME` | client env (hello) | codex's home override; the `config.toml` notify guard and rollout correlation read it. Default `~/.codex`. |
