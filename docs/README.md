# fleetcom Documentation

Fleetcom keeps durable session recipes separate from ephemeral daemon state. This guide documents both storage paths, the build workflow, and a complete first run.

## Index

- [Commands](commands.md): every key and launch flag, including the routing mechanics
- [Sessions](sessions.md): the task recipe format and where it lives
- [Agent session resume](agent-resume.md): how `claude` and `codex` tasks are captured and saved as resuming commands
- [Directory & Environment Configuration](#directory--environment-configuration): the socket, the lock, and the session paths
- [Sample Usage Session](#sample-usage-session): a first run, start to finish
- [Notes & Caveats](#notes--caveats): process and protocol boundaries

## Installation from source

`fleetcom` is Unix-only: it relies on PTYs and process-group signals (`killpg`).

From a repository clone:

- `cargo test`: confirm the suite passes
- `cargo build --release`: compile to `target/release/fleetcom`
- `cargo install --path .`: put `fleetcom` on `PATH`

## Directory & Environment Configuration

Fleetcom separates runtime state from configuration. Runtime state contains the daemon socket and lock; configuration contains durable session recipes.

### Runtime directory (socket + lock)

The runtime directory holds `default.sock`, the mode-`0600` client↔daemon socket, and `daemon.lock`, the single-instance `flock`. The daemon records its PID in the lock file; `--kill` uses that PID rather than waiting for the socket. Fleetcom creates the directory with mode `0700`. An existing path must be a real directory owned by the current user, so symlinks and directories owned by another user are rejected.

Resolved in this order:

| Order | Condition | Path |
| -- | -- | -- |
| 1 | `FLEETCOM_RUNTIME_DIR` is set | `$FLEETCOM_RUNTIME_DIR` (verbatim) |
| 2 | `$XDG_RUNTIME_DIR` is set and non-empty (Linux) | `$XDG_RUNTIME_DIR/fleetcom` |
| 3 | otherwise | `$TMPDIR/fleetcom-$uid` |

On macOS, `$TMPDIR` is already per-user. The `$uid` suffix also separates users when the fallback resolves beneath a shared `/tmp`.

### Config directory (sessions)

Holds saved sessions under a `sessions/` subdirectory: one `<name>.json` per session. See [Sessions](sessions.md) for the format.

| Order | Condition | Path |
| -- | -- | -- |
| 1 | `FLEETCOM_CONFIG_DIR` is set | `$FLEETCOM_CONFIG_DIR/sessions` |
| 2 | Linux | `${XDG_CONFIG_HOME:-~/.config}/fleetcom/sessions` |
| 2 | macOS | `~/Library/Application Support/fleetcom/sessions` |

The platform default is [`dirs::config_dir()`](https://docs.rs/dirs/latest/dirs/fn.config_dir.html) joined with `fleetcom`. The directory is created on the first save.

## Sample Usage Session

The following walkthrough moves from an empty dashboard to a saved fleet. The frames show layout, not captured terminal output.

Run `fleetcom`. The first invocation starts the daemon and opens an empty dashboard:

```text
  fleetcom   0 running · 0 idle · 0 done      by state · dir · custom

  ❯ n run · @ dir · s sort · w save · o load
  ↑↓ select · enter attach · space peek · n/@ new · s sort · m tag · g group · R rename · r rerun · X kill · q detach · Q quit
```

Press `n`, enter a command, and press `Enter`. The command runs in its own PTY and appears under Running. Repeat the process for a second command:

```text
  fleetcom   2 running · 0 idle · 0 done      by state · dir · custom

  Running
  ✻  cargo watch -x test      test result: ok. 42 passed         9s
  ✻  npm run dev              VITE v5.0  ready in 312 ms         4s

  ❯ n run · @ dir · s sort · w save · o load
  ↑↓ select · enter attach · space peek · n/@ new · s sort · m tag · g group · R rename · r rerun · X kill · q detach · Q quit
```

Each row is `glyph · tag · command · latest output · age`. `Space` peeks: a read-only box of the selected task's live screen, without leaving the dashboard:

```text
  ┌─ cargo watch -x test ───────────────────────────────┐
  │ running 3 tests                                     │
  │ test result: ok. 42 passed; 0 failed                │
  │                                                     │
  └ space/esc close · enter attach ─────────────────────┘
```

`Enter` attaches to the task. Keystrokes then go to its PTY, except for the reserved background chord shown in the status bar:

```text
  [attached] npm run dev                       Ctrl-\ background
```

`Ctrl-\` returns to the dashboard. `m` tags the selected task "in use," adding `◆` and moving it to the first section:

```text
  fleetcom   2 running · 0 idle · 0 done      by state · dir · custom

  In use
  ✻ ◆cargo watch -x test      test result: ok. 42 passed        1m

  Running
  ✻  npm run dev              VITE v5.0  ready in 312 ms        1m
```

`s` cycles through state, directory, and custom grouping. The header renders the active mode in bold. In custom mode, `g` assigns the selected task to a named group. Named sections sort alphabetically; Unassigned appears last when at least one task has no group:

```text
  fleetcom   2 running · 0 idle · 0 done      by state · dir · custom

  api
  ✻ ◆cargo watch -x test      test result: ok. 42 passed        2m

  Unassigned
  ✻  npm run dev              VITE v5.0  ready in 312 ms        2m
```

In custom mode, a new command inherits the selected task's group. The spawn prompt makes that destination explicit (`❯ api ▸ cargo run`). Group assignments belong to task state, so detach and rerun preserve them. The [command reference](commands.md#the-g-group-picker) documents the picker mechanics.

`R` renames the selected task. The prompt opens with the current name; `Enter` saves, an empty field restores the command as the display label, and `Esc` cancels. Rows and peek titles then show the name in place of the command:

```text
  fleetcom   2 running · 0 idle · 0 done      by state · dir · custom

  api
  ✻ ◆api tests                test result: ok. 42 passed        3m

  Unassigned
  ✻  npm run dev              VITE v5.0  ready in 312 ms        3m
```

The attached status bar shows both: `[attached] api tests · cargo watch -x test`. Names are daemon state, survive detach and rerun, and persist in saved [sessions](sessions.md).

`w`, a name, and `Enter` save the fleet as a [session](sessions.md). `q` then disconnects while the daemon and both jobs continue running. A subsequent `fleetcom` invocation reconstructs the dashboard from the daemon's current task state. `Q` or `fleetcom --kill` stops the jobs (`TERM`, then `KILL` after a two-second grace period) and exits the daemon.

## Notes & Caveats

- The fleet dies with the daemon. Because the daemon holds each PTY master, daemon termination closes the terminals and the kernel sends `SIGHUP` to every task's process group. A clean shutdown sends `SIGTERM` before `SIGKILL`; a crash or direct `SIGKILL` provides no grace period. HUP-immune jobs (`nohup`, `trap '' HUP`) can survive, but the next daemon neither owns nor displays them. A panic while serving one client only drops that connection.
- Commands run through the client's non-interactive shell (`$SHELL -c`, or `/bin/sh` when `SHELL` is unset), so functions and aliases from `~/.zshrc` are unavailable.
- Each launch uses the launching client's environment and working directory, sent once per connection during the hello handshake. Connect from a venv terminal and your spawns, reruns, and session loads all see that venv, whichever client originally autostarted the daemon. Environment is never written to disk; session files store only directories, commands, group assignments, and display names.
- Client and daemon protocol versions must match. The daemon rejects a mismatch during the handshake. Stop an incompatible daemon with `fleetcom --kill`, which also terminates every running job, then start a new client.
- The daemon serves one client at a time. A second `fleetcom` prints a waiting notice, then attaches when the active client disconnects (`q`). `Ctrl-C` while waiting aborts without touching the daemon.
- Shutdown is graceful-first. `X`, `Q`, `--kill`, and daemon signals send `SIGTERM` to the job's *process group* and escalate to `SIGKILL` after two seconds. Fleetcom holds an exited leader unreaped until task removal, which reserves the process-group ID and keeps background children signalable. A child created by `cmd &` in a non-interactive shell remains in that group. A process that calls `setsid` or double-forks out of the group escapes this sweep and must be terminated separately.
- `--foreground` is ephemeral. It runs the core in-process with no daemon, so the jobs die when you quit and there is nothing to reattach to.
- Signalling the daemon is a clean shutdown. `SIGTERM`/`SIGINT`/`SIGHUP` to the daemon group-kill every job, remove the socket, and exit. This is the same teardown as `Q` or `fleetcom --kill`.
