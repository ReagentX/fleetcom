# fleetcom Documentation

The daemon supervises live processes; session files store repeatable launch recipes. Reconnecting returns you to the daemon's running tasks, while loading a session starts new processes from its recipe. Use this guide for storage paths, lifecycle details, and a complete first run.

## Index

- [Commands](commands.md): every key and launch flag, including the routing mechanics
- [How it works](how-it-works.md): the PTY emulation, input routing, and activity grouping
- [Sessions](sessions.md): the task recipe format and storage location
- [Agent session resume](agent-resume.md): capturing and resuming supported `claude`, `codex`, `grok`, and `omp` sessions
- [Storage paths](#storage-paths): runtime and session paths
- [First-run walkthrough](#first-run-walkthrough): a first run, start to finish
- [Security](#security): the trust boundary, on-disk state, and what is not protected
- [Operational constraints](#operational-constraints): process and protocol boundaries

## Installation from source

`fleetcom` is Unix-only: PTYs and process-group signals (`killpg`) are required.

From a repository clone:

- `cargo test`: run the test suite
- `cargo build --release`: compile to `target/release/fleetcom`
- `cargo install --path .`: put `fleetcom` on `PATH`

## Storage paths

The daemon socket, lock, and log are stored under the runtime path; durable session recipes are stored under the configuration path. These paths are resolved independently.

### Runtime directory (socket + lock + log)

The runtime directory contains three files: `default.sock` carries client↔daemon traffic, `daemon.lock` holds the single-instance `flock`, and `daemon.log` records an autostarted daemon's stderr. The daemon writes its PID to the lock file so `--kill` can signal it without waiting for the socket. See [Security](#security) for the required permissions and ownership checks.

Resolved in this order:

| Order | Condition | Path |
| -- | -- | -- |
| 1 | `FLEETCOM_RUNTIME_DIR` is set | `$FLEETCOM_RUNTIME_DIR` (verbatim) |
| 2 | `$XDG_RUNTIME_DIR` is set and non-empty | `$XDG_RUNTIME_DIR/fleetcom` |
| 3 | otherwise | `$TMPDIR/fleetcom-$uid` |

`$XDG_RUNTIME_DIR` is honored on every supported platform. On macOS, `$TMPDIR` is already per-user. With the `$uid` suffix, users are also separated when the fallback path is beneath a shared `/tmp`.

### Config directory (sessions)

Saved sessions are stored under a `sessions/` subdirectory: one sanitized-name `.json` file per session. See [Sessions](sessions.md) for the format.

| Order | Condition | Path |
| -- | -- | -- |
| 1 | `FLEETCOM_CONFIG_DIR` is set | `$FLEETCOM_CONFIG_DIR/sessions` |
| 2 | Linux | `${XDG_CONFIG_HOME:-~/.config}/fleetcom/sessions` |
| 2 | macOS | `~/Library/Application Support/fleetcom/sessions` |

The platform default is [`dirs::config_dir()`](https://docs.rs/dirs/latest/dirs/fn.config_dir.html) joined with `fleetcom`. Missing session directories are created on the first save.

## First-run walkthrough

Follow these steps from an empty dashboard to a saved fleet. The frames below are layout illustrations, not captured terminal output.

Run `fleetcom` to start the daemon and open an empty dashboard:

```text
  fleetcom   0 running · 0 idle · 0 done      by state · dir · custom

  ❯ n run · @ dir · / find · s sort
  ↑↓ select · enter attach · space peek · ? controls
```

Use the hints for common dashboard actions; press `?` for the expanded key reference. Press `n`, enter a command, and press `Enter`. The command is started in its own PTY and listed under Running. Repeat the process for a second command:

```text
  fleetcom   2 running · 0 idle · 0 done      by state · dir · custom

  Running
  ✻  cargo watch -x test      test result: ok. 42 passed         9s
  ✻  npm run dev              VITE v5.0  ready in 312 ms         4s

  ❯ n run · @ dir · / find · s sort
  ↑↓ select · enter attach · space peek · ? controls
```

Each row is `glyph · tag · command · latest output · age`. Age is measured from launch while running, last output once idle, or exit once completed. Press `Space` to peek at the selected task's live screen in a read-only box over the dashboard:

```text
  ┌─ cargo watch -x test ───────────────────────────────┐
  │ running 3 tests                                     │
  │ test result: ok. 42 passed; 0 failed                │
  │                                                     │
  └ space/esc close · enter attach · preview: floor ────┘
```

Press `Enter` to attach to the task. Keystrokes are then forwarded to its PTY, except for the reserved background chord shown in the status bar:

```text
  [attached] npm run dev                       Ctrl-\ background
```

Press `Ctrl-\` to return to the dashboard. Press `m` to tag the selected task "in use," mark it `◆`, and place it in the first section:

```text
  fleetcom   2 running · 0 idle · 0 done      by state · dir · custom

  In use
  ✻ ◆cargo watch -x test      test result: ok. 42 passed        1m

  Running
  ✻  npm run dev              VITE v5.0  ready in 312 ms        1m
```

Press `s` to cycle through state, directory, and custom grouping. The active mode is bold in the header. In custom mode, press `g` to assign the selected task to a named group. Named sections are sorted without regard to case, so `API` and `api` are adjacent. They remain separate because group identity is case-sensitive. Unassigned is listed last when at least one task has no group:

```text
  fleetcom   2 running · 0 idle · 0 done      by state · dir · custom

  api
  ✻ ◆cargo watch -x test      test result: ok. 42 passed        2m

  Unassigned
  ✻  npm run dev              VITE v5.0  ready in 312 ms        2m
```

In custom mode, a new command is assigned the selected task's group, shown in the spawn prompt (`❯ api ▸ cargo run`). Group assignments are stored with task state and preserved across detach and rerun. See the [command reference](commands.md#the-g-group-picker) for picker mechanics.

Press `R` to rename the selected task, starting with its current name. Press `Enter` to save, or `Esc` to cancel. Leave the field empty to display the command again. After naming a task, its name is displayed in dashboard rows and peek titles:

```text
  fleetcom   2 running · 0 idle · 0 done      by state · dir · custom

  api
  ✻ ◆api tests                test result: ok. 42 passed        3m

  Unassigned
  ✻  npm run dev              VITE v5.0  ready in 312 ms        3m
```

In the attached status bar, both are displayed: `[attached] api tests · cargo watch -x test`. Names are stored in daemon state, preserved across detach and rerun, and included in saved [sessions](sessions.md).

Press `w`, type a name, and press `Enter` to save the fleet as a [session](sessions.md). Press `q` to disconnect while the daemon and both tasks continue running. Run `fleetcom` again to view the daemon's current task state. Press `Q` or run `fleetcom --kill` to stop the tasks (`TERM`, then `KILL` after a two-second grace period) and exit the daemon.

## Security

`fleetcom` runs entirely as your user, without raising or dropping privileges. Filesystem permissions control access: the socket is mode `0600` inside a mode-`0700` directory, and the daemon performs no peer authentication. Any process running as your user can therefore connect, spawn commands, and read task output. That is the trust boundary.

### On-disk state

| Path | Mode | Contents |
| -- | -- | -- |
| [runtime directory](#runtime-directory-socket--lock--log) | `0700` | the socket, lock, daemon log, and any capture roots resolved beneath it |
| `<runtime>/default.sock` | `0600` | the client↔daemon socket |
| `<runtime>/daemon.lock` | `0666 & ~umask` when new; otherwise unchanged | the owning daemon's PID, trustworthy only while its `flock` is held |
| `<runtime>/daemon.log` | `0666 & ~umask` when new; otherwise unchanged | stderr from the autostarted daemon |
| [session directory](#config-directory-sessions) | `0700` | saved recipes |
| `<sessions>/<name>.json` | `0600` | directories, commands, groups, display names |
| `<sessions>/recovery/` | `0700` | [automatic snapshots](sessions.md#recovery) |
| `<sessions>/recovery/<snapshot>.json` | `0600` | one automatic session recipe |
| `<capture-root>/<pid>-<nonce>/` | `0700` | [agent hook, notifier, and extension-module assets plus per-run capture payloads](agent-resume.md#capture-state-and-isolation) |

Saves are atomic: a mode-`0600` temporary file is written in the destination directory, synced, then renamed over the target. No partial or world-readable recipe is exposed. New session and recovery directories are created with mode `0700`; group and other permissions are removed from the destination directory on each save.

### The runtime directory must be trustworthy

The runtime directory is validated before its contents are used. The path must be a real directory owned by the current user; symlinks and directories owned by another user are rejected. Group or other write access is fatal because another user could already have planted entries. Any remaining group or other permissions are removed in place.

### What is not protected

Full command lines are persisted in recipes, including any embedded secrets. A token passed as an argument is written to its session file and to every recovery snapshot that captures the task.

The client environment is not persisted. Each client's environment and working directory are sent during the connection handshake and retained in daemon memory. Only directories, commands, group assignments, and display names are stored in session and recovery files.

### Captured IDs in shell commands

To resume an agent conversation, `fleetcom` inserts its captured ID into a command run through `$SHELL -c`. Because the ID becomes shell input, validation accepts only lowercase hexadecimal in the `8-4-4-4-12` UUID shape. The same check applies to capture payloads, live session records, and final command construction. `fleetcom` does not read session IDs from terminal output and instruments only a bare program word or its canonical resume form. See [Agent session resume](agent-resume.md#validation-boundary) for both boundaries.

### Copying text through the terminal

When you copy a selection or forward an attached task's clipboard store, the text is sent to the host terminal as an OSC 52 escape sequence, including through intermediaries such as SSH connections and terminal multiplexers.

## Operational constraints

### Task lifetime after daemon termination

Each PTY master is held by the daemon. On daemon termination, the terminals are closed and `SIGHUP` is sent by the kernel to every task's process group. On clean shutdown, `SIGTERM` is sent before `SIGKILL`; after a crash or direct `SIGKILL`, no grace period is available. HUP-immune processes (`nohup`, `trap '' HUP`) can be left running, but are neither supervised nor displayed by the next daemon. After a panic while serving one client, only that connection is dropped.

### Commands run through the client's non-interactive shell

`fleetcom` invokes `$SHELL -c`, falling back to `/bin/sh` when `SHELL` is unset. Because the shell is non-interactive, functions and aliases from `~/.zshrc` are unavailable.

### Environment and directory

Each client's environment and working directory are sent once during the connection handshake. This launch context is used for subsequent spawns, reruns, and session loads from that client. See [Security](#security) for persisted state.

### Scrollback depth is fixed per supervisor

Per-task scrollback depth is resolved once, at supervisor startup:

| Order | Condition | Depth |
| -- | -- | -- |
| 1 | `--scrollback <lines>` was passed | that value, clamped to 100,000 |
| 2 | `FLEETCOM_SCROLLBACK` parses as a whole number | that value, clamped to 100,000 |
| 3 | otherwise | 2,000 |

Set `0` to disable scrollback. An unparseable `FLEETCOM_SCROLLBACK` is treated as 2,000 without failing daemon startup. Depth is resolved at daemon startup from the inherited environment. To change it, stop the current daemon with `fleetcom --kill` first. With `--foreground`, depth is resolved in-process for each invocation.

### Client and daemon protocol versions must match

Mismatched versions are rejected during the handshake. Stop an incompatible daemon with `fleetcom --kill`, which also terminates every running task, then start a new client.

### The daemon serves one client at a time

When you run a second `fleetcom`, a waiting notice is printed. You can attach once the active client disconnects (`q`). Press `Ctrl-C` while waiting to abort without affecting the daemon.

### Shutdown is graceful-first

On `X`, `Q`, `--kill`, or a daemon shutdown signal, `SIGTERM` is sent to each task's *process group*, then `SIGKILL` after two seconds. Exited leaders remain unreaped through escalation, reserving the process-group IDs so background children remain signalable. On full shutdown (`Q`/`--kill`), one shared grace period is required even when all listed tasks have finished or exited on `TERM`; with no tasks left to clean up, shutdown is immediate. Original escalation timers are retained for tasks already terminating. A child created by `cmd &` in a non-interactive shell normally remains in its parent's group. A process that calls `setsid` or otherwise leaves the group is outside the sweep and must be terminated separately.

### `--foreground` is ephemeral

Run the core in-process with no daemon. Tasks are terminated when you quit; you cannot reattach later.

### Signalling the daemon is a clean shutdown

Send `SIGTERM`, `SIGINT`, or `SIGHUP` to the daemon to group-kill every task, remove the socket, and exit. This is the same teardown as `Q` or `fleetcom --kill`.
