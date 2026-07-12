# fleetcom 

A fleet-view supervisor for concurrent shell commands.

Supervising several long-running commands usually means juggling terminal panes and reconstructing state after a disconnect. `fleetcom` runs each command in its own PTY and exposes the resulting screens through one dashboard. A daemon owns the jobs, so closing the client does not stop them.

## What it does

- Runs each command in its own PTY and groups tasks by state or working directory.
- Provides read-only previews and full interactive attachment.
- Keeps jobs running after the client disconnects.
- Saves and reloads `{directory: [commands]}` recipes.
- Launches commands in other directories through the `@` picker.

## Documentation

Configuration, on-disk layout, the session format, every command, and a first-run walkthrough live in [`docs/`](docs/README.md).

## Installation

Unix only: it relies on PTYs and process-group signals (`killpg`).

### From source

From the project root:

- `cargo install --path .` to install `fleetcom` on `PATH`, or
- `cargo build --release` and run `target/release/fleetcom`.

## Usage

`fleetcom` exposes four operating modes:

- `fleetcom`
  - Connects to the daemon (autostarting it if needed) and opens the dashboard
- `fleetcom <session>`
  - Loads a saved session at startup, then opens the dashboard
- `fleetcom --foreground`
  - Runs everything in-process, without a daemon (jobs die when you quit)
- `fleetcom --kill`
  - Kills the daemon and its running jobs
- `fleetcom --help` / `--version`
  - Print usage / the version and exit

The first ordinary invocation starts the daemon when necessary. `--daemon` is an internal mode.

## Key Commands

### Dashboard

| Key | Command |
| -- | -- |
| ↑ ↓ / `k` `j` | move the selection |
| `Enter` | attach to the selected task |
| `Space` | peek at the selected task |
| `n` | new command in the current directory |
| `@` | new command in a directory you pick (with completion) |
| `s` | toggle grouping: by state / by directory |
| `m` | tag the task "in use" (pins it to the top) |
| `X` | kill a running task (`TERM`, then `KILL` after 2 s), or remove a finished one (Shift-gated) |
| `w` | save the current tasks as a session |
| `o` | load a saved session |
| `q` | disconnect; leave the daemon and jobs running |
| `Q` | quit; kill the jobs and stop the daemon |

### Attached

| Key | Command |
| -- | -- |
| `Ctrl-\` | background the task and return to the dashboard |
| anything else | forwarded to the task's PTY |

## Features

### One PTY per command

Every task runs in its own pseudo-terminal, emulated with `vt100`. The same screen grid powers the dashboard preview, the peek overlay, and full attached rendering. A mid-run `vim` or `htop` therefore renders from the same terminal state as any other task. Backgrounding changes client focus; it does not notify the child.

### Jobs outlive the UI

A per-user daemon owns the processes and their terminals. `q` disconnects the client and leaves everything running; the next `fleetcom` reattaches. `Q` and `fleetcom --kill` stop the daemon and terminate each job's process group with `SIGTERM`, escalating to `SIGKILL` after a two-second grace period. `SIGTERM`, `SIGINT`, and `SIGHUP` sent directly to the daemon use the same shutdown path. Because `fleetcom --kill` signals the daemon through its lock-file PID, it also works while another client occupies the socket. If the connection drops, the client discards its stale view and offers to reconnect.

Teardown caveat: signals go to each job's *process group*. A job that re-backgrounds itself past its own shell's exit (`cmd &`, then the shell exits) leaves that group and survives. Kill it by hand. This is deliberate: once the shell is reaped its PID can be recycled, so signalling the old group could hit an unrelated process.

### Grouping and the `@` picker

Group the fleet by state (In use / Running / Completed) or by working directory. `@` opens a live directory picker: the current dir first, recently used dirs next, matching subdirectories below. Launch a command anywhere without leaving the dashboard.

### Sessions

Save the current set of `{directory: [commands]}` as a named recipe and reload it later (`w` / `o`, or `fleetcom <name>`). Loading re-runs the commands; it does not resurrect live processes. Process continuity and session replay are separate mechanisms.

## Notes

`fleetcom` is intended for concurrent build, test, watch, server, and interactive-agent processes. It is narrower than a terminal multiplexer: each task is one command rather than a persistent shell session.

### When to use it

- Several long-lived commands need one place for observation, tagging, and attachment.
- Jobs must survive a terminal closing and remain available for reattachment.
- The same command set is launched often enough to justify a saved session.

### When to avoid it

- For interactive multiplexing of persistent shells, use `tmux`. `fleetcom` runs one command per pane, not a shell session.
- It is not a full process manager: crash-resilient ownership (adopting jobs
  after a daemon *crash*, as opposed to a clean shutdown) is out of scope

### Known limitations

- Commands run through a non-interactive shell (`$SHELL -c`), so functions and aliases defined in `~/.zshrc` are not available.
- The daemon captures the environment of the client that **first** starts it and runs every job under that environment. A second terminal with a different `PATH` or virtualenv attaches to the same daemon, and its commands resolve against the first terminal's environment, not its own.
- The daemon serves **one client at a time**; a second `fleetcom` connects but waits until the first disconnects (`q`).
- The daemon can only clean up when it gets the chance: `SIGKILL` (or a crash) skips its shutdown path, and the jobs keep running, unowned. The next `fleetcom` starts an empty daemon that knows nothing about them.
