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
| `X` | kill a running task (`TERM`, then `KILL` after 2 s), or remove a finished one (Shift-gated); removal sweeps any background processes the job left in its group, with the same `TERM`-then-`KILL` grace |
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

A per-user daemon owns the processes and their terminals. `q` disconnects the client and leaves everything running; the next `fleetcom` reattaches. Each launch runs under the environment and working directory of the client that requested it: connect from a venv terminal and your jobs see that venv, whichever client started the daemon. `Q` and `fleetcom --kill` stop the daemon and terminate each job's process group with `SIGTERM`, escalating to `SIGKILL` after a two-second grace period. `SIGTERM`, `SIGINT`, and `SIGHUP` sent directly to the daemon use the same shutdown path. Because `fleetcom --kill` signals the daemon through its lock-file PID, it also works while another client occupies the socket. If the connection drops, the client discards its stale view and offers to reconnect.

Teardown caveat: signals go to each job's *process group*, and the group stays signalable for the task's whole life: the exited leader is held unreaped until after the final sweep, which keeps its group id reserved. A `cmd &` child never leaves the group (a non-interactive shell's `&` creates no new process group), so kills and removals reach it too. What *does* escape is a job that `setsid`s or double-forks itself out of the group: that one is on its own; kill it by hand.

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
- It is not a full process manager: the fleet's lifetime is bounded by the daemon's (see the first limitation below).

### Known limitations

- The fleet dies with the daemon. The daemon holds every task's PTY master, so daemon death of any kind closes them and the kernel hangs up each task's terminal: SIGHUP to its process group. A clean shutdown (`Q`, `--kill`, SIGTERM) delivers TERM first with a KILL after a two-second grace; a crash or SIGKILL skips that and the jobs get the bare HUP. Only HUP-immune jobs (`nohup`, `trap '' HUP`) survive a daemon crash: unowned and invisible to the next daemon, which starts empty. A panic while serving a client is contained (the connection drops, the fleet keeps running), but the daemon process itself is the fleet's single point of failure.
- Commands run through the client's non-interactive shell (`$SHELL -c`, or `/bin/sh` when `SHELL` is unset), so functions and aliases defined in `~/.zshrc` are not available.
- The daemon serves one client at a time. A second `fleetcom` prints a waiting notice, then attaches when the active client disconnects (`q`).
