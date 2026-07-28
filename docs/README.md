# fleetcom Documentation

`fleetcom` has two kinds of state with different lifetimes: the daemon owns live processes, while session files store repeatable launch recipes. Confusing those boundaries makes shutdown, reconnect, and session behavior difficult to reason about. This guide documents the paths, the lifecycle, and a complete first run.

## Index

- [Commands](commands.md): every key and launch flag, including the routing mechanics
- [How it works](how-it-works.md): the PTY emulation, input routing, and activity grouping
- [Sessions](sessions.md): the task recipe format and where it lives
- [Agent session resume](agent-resume.md): how `fleetcom` captures and resumes supported `claude`, `codex`, and `grok` sessions
- [Storage paths](#storage-paths): the socket, the lock, and the session paths
- [First-run walkthrough](#first-run-walkthrough): a first run, start to finish
- [Security](#security): the trust boundary, on-disk state, and what is not protected
- [Operational constraints](#operational-constraints): process and protocol boundaries

## Installation from source

`fleetcom` is Unix-only: it relies on PTYs and process-group signals (`killpg`).

From a repository clone:

- `cargo test`: confirm the suite passes
- `cargo build --release`: compile to `target/release/fleetcom`
- `cargo install --path .`: put `fleetcom` on `PATH`

## Storage paths

Runtime state contains the daemon socket and lock. Configuration contains durable session recipes. The paths resolve independently.

### Runtime directory (socket + lock)

The runtime directory holds `default.sock`, the client↔daemon socket; `daemon.lock`, the single-instance `flock`; and `daemon.log`, the stderr of an autostarted daemon. The daemon records its PID in the lock file; `--kill` uses that PID rather than waiting for the socket. [Security](#security) documents the permissions and the ownership checks this directory must satisfy.

Resolved in this order:

| Order | Condition | Path |
| -- | -- | -- |
| 1 | `FLEETCOM_RUNTIME_DIR` is set | `$FLEETCOM_RUNTIME_DIR` (verbatim) |
| 2 | `$XDG_RUNTIME_DIR` is set and non-empty | `$XDG_RUNTIME_DIR/fleetcom` |
| 3 | otherwise | `$TMPDIR/fleetcom-$uid` |

`$XDG_RUNTIME_DIR` is honored on every supported platform. On macOS, `$TMPDIR` is already per-user. The `$uid` suffix also separates users when the fallback resolves beneath a shared `/tmp`.

### Config directory (sessions)

Holds saved sessions under a `sessions/` subdirectory: one sanitized-name `.json` file per session. See [Sessions](sessions.md) for the format.

| Order | Condition | Path |
| -- | -- | -- |
| 1 | `FLEETCOM_CONFIG_DIR` is set | `$FLEETCOM_CONFIG_DIR/sessions` |
| 2 | Linux | `${XDG_CONFIG_HOME:-~/.config}/fleetcom/sessions` |
| 2 | macOS | `~/Library/Application Support/fleetcom/sessions` |

The platform default is [`dirs::config_dir()`](https://docs.rs/dirs/latest/dirs/fn.config_dir.html) joined with `fleetcom`. The first save creates any missing session directories.

## First-run walkthrough

The following walkthrough moves from an empty dashboard to a saved fleet. The frames show layout, not captured terminal output.

Run `fleetcom`. The first invocation starts the daemon and opens an empty dashboard:

```text
  fleetcom   0 running · 0 idle · 0 done      by state · dir · custom

  ❯ n run · @ dir · s sort · w save · o load
  ↑↓ select · enter attach · space peek · m tag · g group · R rename · r rerun · X kill · q detach · Q quit
```

Press `n`, enter a command, and press `Enter`. The command runs in its own PTY and appears under Running. Repeat the process for a second command:

```text
  fleetcom   2 running · 0 idle · 0 done      by state · dir · custom

  Running
  ✻  cargo watch -x test      test result: ok. 42 passed         9s
  ✻  npm run dev              VITE v5.0  ready in 312 ms         4s

  ❯ n run · @ dir · s sort · w save · o load
  ↑↓ select · enter attach · space peek · m tag · g group · R rename · r rerun · X kill · q detach · Q quit
```

Each row is `glyph · tag · command · latest output · age`. The age counts from the task's last meaningful edge: launch while running, last output once idle, exit once completed. `Space` peeks: a read-only box of the selected task's live screen, without leaving the dashboard:

```text
  ┌─ cargo watch -x test ───────────────────────────────┐
  │ running 3 tests                                     │
  │ test result: ok. 42 passed; 0 failed                │
  │                                                     │
  └ space/esc close · enter attach · preview: floor ────┘
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

`w`, a name, and `Enter` save the fleet as a [session](sessions.md). `q` then disconnects while the daemon and both tasks continue running. A subsequent `fleetcom` invocation reconstructs the dashboard from the daemon's current task state. `Q` or `fleetcom --kill` stops the tasks (`TERM`, then `KILL` after a two-second grace period) and exits the daemon.

## Security

`fleetcom` runs entirely as your user. It neither raises nor drops privileges. Access control comes from filesystem permissions rather than authentication: the socket is mode `0600` inside a mode-`0700` directory, and the daemon performs no peer check. Any process running as your user can therefore connect, spawn commands, and read task output. That is the trust boundary.

### What lands on disk

| Path | Mode | Contents |
| -- | -- | -- |
| [runtime directory](#runtime-directory-socket--lock) | `0700` | the socket, lock, daemon log, and any capture roots resolved beneath it |
| `<runtime>/default.sock` | `0600` | the client↔daemon socket |
| `<runtime>/daemon.lock` | `0666 & ~umask` when new; otherwise unchanged | the owning daemon's PID, trustworthy only while its `flock` is held |
| `<runtime>/daemon.log` | `0666 & ~umask` when new; otherwise unchanged | stderr from the autostarted daemon |
| [session directory](#config-directory-sessions) | `0700` | saved recipes |
| `<sessions>/<name>.json` | `0600` | directories, commands, groups, display names |
| `<sessions>/recovery/` | `0700` | [automatic snapshots](sessions.md#recovery) |
| `<sessions>/recovery/<snapshot>.json` | `0600` | one automatic session recipe |
| `<capture-root>/<pid>-<nonce>/` | `0700` | [agent hook and notifier assets plus per-run capture payloads](agent-resume.md#capture-state-and-isolation) |

Saves are atomic: `fleetcom` writes a mode-`0600` temporary file in the destination directory, syncs it, then renames it over the target. This does not expose a partial or world-readable recipe. New session and recovery directories use mode `0700`; each save also removes group and other permissions from the destination directory.

### The runtime directory must be trustworthy

`fleetcom` validates the runtime directory before trusting its contents. The path must be a real directory owned by the current user; symlinks and directories owned by another user are rejected. Group or other write access is fatal because another user could already have planted entries. Any remaining group or other permissions are removed in place.

### What is not protected

Recipes persist full command lines, which can embed secrets. A token passed as an argument is written to its session file and to every recovery snapshot that captures the task.

`fleetcom` does not persist the client environment. Each client sends its environment and working directory during the connection handshake, and the daemon retains that launch context in memory. Session and recovery files store only directories, commands, group assignments, and display names.

### Captured IDs cross a shell boundary

Agent resume writes a captured conversation ID into a command run through `$SHELL -c`, so validation is a security boundary. Accepted IDs contain only lowercase hexadecimal in the `8-4-4-4-12` UUID shape. Hook payloads, terminal scrapes, filesystem correlation, and the command builder all apply that check. Instrumentation applies only to a bare program word or its canonical resume form, never arbitrary shell text. [Agent session resume](agent-resume.md#validation-boundary) documents both boundaries.

### Copied text leaves through the terminal

When `fleetcom` copies a selection or forwards an attached task's clipboard store, it sends the text to the host terminal as an OSC 52 escape sequence. The sequence also crosses intermediaries such as SSH connections and terminal multiplexers.

## Operational constraints

### The fleet dies with the daemon

Because the daemon holds each PTY master, daemon termination closes the terminals and the kernel sends `SIGHUP` to every task's process group. A clean shutdown sends `SIGTERM` before `SIGKILL`; a crash or direct `SIGKILL` provides no grace period. HUP-immune processes (`nohup`, `trap '' HUP`) can survive, but the next daemon neither owns nor displays them. A panic while serving one client only drops that connection.

### Commands run through the client's non-interactive shell

(`$SHELL -c`, or `/bin/sh` when `SHELL` is unset), so functions and aliases from `~/.zshrc` are unavailable.

### Environment and directory

Each launch uses the launching client's environment and working directory, sent once per connection during the hello handshake. Connect from a venv terminal and your spawns, reruns, and session loads all see that venv, whichever client originally autostarted the daemon. [Security](#security) covers what that context does and does not persist.

### Scrollback depth is fixed per supervisor

Each task's terminal keeps a scrollback history whose depth is resolved once, when the owning supervisor starts:

| Order | Condition | Depth |
| -- | -- | -- |
| 1 | `--scrollback <lines>` was passed | that value, clamped to 100,000 |
| 2 | `FLEETCOM_SCROLLBACK` parses as a whole number | that value, clamped to 100,000 |
| 3 | otherwise | 2,000 |

`0` disables scrollback. An unparseable `FLEETCOM_SCROLLBACK` falls back to 2,000 rather than failing daemon startup. The daemon resolves the depth at startup from its inherited environment, so a changed value reaches only the tasks of a new daemon; stop the current one with `fleetcom --kill` first. `--foreground` resolves the depth in-process for each invocation.

### Client and daemon protocol versions must match

The daemon rejects a mismatch during the handshake. Stop an incompatible daemon with `fleetcom --kill`, which also terminates every running task, then start a new client.

### The daemon serves one client at a time

A second `fleetcom` prints a waiting notice, then attaches when the active client disconnects (`q`). `Ctrl-C` while waiting aborts without touching the daemon.

### Shutdown is graceful-first

`X`, `Q`, `--kill`, and daemon shutdown signals send `SIGTERM` to each task's *process group*, then escalate to `SIGKILL` after two seconds. Removing one task (`X`) keeps an exited leader unreaped through escalation, reserving the process-group ID so background children remain signalable. During full shutdown (`Q`/`--kill`), checking whether a group is empty reaps its exited leader. A `TERM`-ignoring member that outlives the leader can then become unsafe to signal by group ID and survive daemon shutdown. A child created by `cmd &` in a non-interactive shell normally remains in its parent's group. A process that calls `setsid` or otherwise leaves the group is outside the sweep and must be terminated separately.

### `--foreground` is ephemeral

It runs the core in-process with no daemon, so the tasks die when you quit and there is nothing to reattach to.

### Signalling the daemon is a clean shutdown

`SIGTERM`/`SIGINT`/`SIGHUP` to the daemon group-kill every task, remove the socket, and exit. This is the same teardown as `Q` or `fleetcom --kill`.
