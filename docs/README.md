# fleetcom Documentation

`fleetcom` splits durable session recipes from ephemeral daemon state. This document covers those paths, installation from source, and a complete first run.

## Index

- [Commands](commands.md): every key and launch flag, with the mechanics behind them
- [Sessions](sessions.md): the `{directory: [commands]}` recipe format and where it lives
- [Directory & Environment Configuration](#directory--environment-configuration): the socket, the lock, and the session paths
- [Sample Usage Session](#sample-usage-session): a first run, start to finish
- [Notes & Caveats](#notes--caveats): the sharp edges

## Installation from source

`fleetcom` is Unix-only: it relies on PTYs and process-group signals (`killpg`).

From a repository clone:

- `cargo test`: confirm the suite passes
- `cargo build --release`: compile to `target/release/fleetcom`
- `cargo install --path .`: put `fleetcom` on `PATH`

## Directory & Environment Configuration

`fleetcom` writes two kinds of state in separate locations. Runtime state
contains the ephemeral daemon socket and lock. Config state contains durable
session recipes.

### Runtime directory (socket + lock)

The runtime directory holds `default.sock`, the mode-`0600` client↔daemon socket, and `daemon.lock`, the single-instance `flock`. The daemon writes its PID into the lock file, which is how `--kill` finds it. The directory is created with mode `0700`; an existing path must be a real directory owned by the current user. Symlinks and directories owned by another user are rejected.

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

The following walkthrough moves from an empty dashboard to a saved fleet. The frames are layout sketches rather than terminal captures.

Start it. The first `fleetcom` autostarts the daemon and opens an empty dashboard:

```text
  fleetcom   0 running · 0 idle · 0 done      by state

  ❯ n run · @ dir · s sort · w save · o load
  ↑↓ select · enter attach · space peek · n/@ new · s sort · m tag · r rerun · X kill · q detach · Q quit
```

Press `n`, enter a command, and press `Enter`. The command runs in its own PTY and appears under Running. Repeat the process for a second command:

```text
  fleetcom   2 running · 0 idle · 0 done      by state

  Running
  ✻  cargo watch -x test      test result: ok. 42 passed       9s
  ✻  npm run dev              VITE v5.0  ready in 312 ms         4s

  ❯ n run · @ dir · s sort · w save · o load
  ↑↓ select · enter attach · space peek · n/@ new · s sort · m tag · r rerun · X kill · q detach · Q quit
```

Each row is `glyph · tag · command · latest output · age`. `Space` peeks: a read-only box of the selected task's live screen, without leaving the dashboard:

```text
  ┌─ cargo watch -x test ───────────────────────────────┐
  │ running 3 tests                                      │
  │ test result: ok. 42 passed; 0 failed                 │
  │                                                      │
  └ space/esc close · enter attach ─────────────────────┘
```

`Enter` attaches to the task. Keystrokes then go to its PTY, except for the reserved background chord shown in the status bar:

```text
  [attached] npm run dev                       Ctrl-\ background
```

`Ctrl-\` returns to the dashboard. `m` tags the selected task "in use," adding `◆` and moving it to the first section:

```text
  fleetcom   2 running · 0 idle · 0 done      by state

  In use
  ✻ ◆cargo watch -x test      test result: ok. 42 passed      1m

  Running
  ✻  npm run dev              VITE v5.0  ready in 312 ms        1m
```

`w`, a name, and `Enter` save the fleet as a [session](sessions.md). `q` then disconnects while the daemon and both jobs continue running. A subsequent `fleetcom` invocation reconstructs the dashboard from the daemon's current task state. `Q` or `fleetcom --kill` stops the jobs (`TERM`, then `KILL` after a two-second grace period) and exits the daemon.

## Notes & Caveats

- The fleet dies with the daemon. The daemon holds every task's PTY master, so daemon death of any kind closes them and the kernel hangs up each task's controlling terminal: `SIGHUP` to its process group. A clean shutdown (below) is gentler, but a crash or `SIGKILL` delivers the bare HUP with no grace. Only HUP-immune jobs (`nohup`, `trap '' HUP`) survive that: unowned and invisible to the next daemon, which starts empty. A panic while *serving a client* is contained: the connection drops, the fleet keeps running.
- Commands run through a non-interactive shell (`$SHELL -c`), so functions and aliases from `~/.zshrc` are unavailable.
- Each launch uses the launching client's environment and working directory, sent once per connection during the hello handshake. Connect from a venv terminal and your spawns, reruns, and session loads all see that venv, whichever client originally autostarted the daemon. Environment is never written to disk; session files store only directories and commands.
- Client and daemon versions must match. The handshake carries a protocol version, and the daemon refuses a mismatched client with instructions (`fleetcom --kill`, retry) instead of serving it with silently wrong semantics. The common case is a `cargo install` while an old daemon is still running.
- The daemon serves one client at a time. A second `fleetcom` connects but waits until the first disconnects (`q`).
- Kills are graceful-first, and the group stays reachable. `X`, `Q`, `--kill`, and daemon signals all send `SIGTERM` to the job's *process group* and escalate to `SIGKILL` only after a 2-second grace, so a `TERM` handler gets its chance to flush and exit cleanly. The exited leader is deliberately held unreaped (a zombie) until the task is removed, which keeps the group id reserved: kills and removals reach background children the job left in its group (`cmd &` never leaves the group in a non-interactive shell). A job that `setsid`s or double-forks out of the group escapes the sweep; kill that one by hand.
- `--foreground` is ephemeral. It runs the core in-process with no daemon, so the jobs die when you quit and there is nothing to reattach to.
- Signalling the daemon is a clean shutdown. `SIGTERM`/`SIGINT`/`SIGHUP` to the daemon group-kill every job, remove the socket, and exit. This is the same teardown as `Q` or `fleetcom --kill`.
