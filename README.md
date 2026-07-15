# fleetcom

A supervisor for concurrent shell commands.

Running several long-lived commands becomes cumbersome once they span terminal panes or need to survive a disconnect. `fleetcom` gives each command its own PTY and exposes every screen through one dashboard. A daemon owns the jobs, so closing the client does not stop them.

## What it does

- Runs each command in its own PTY and groups tasks by state, by working directory, or by custom named groups.
- Provides read-only previews and full interactive attachment.
- Keeps jobs running after the client disconnects.
- Saves and reloads task recipes with directories, commands, group assignments, and display names.
- Captures `claude`, `codex`, and `grok` conversation IDs and saves commands that resume them.
- Launches commands in other directories through the `@` picker.

## Documentation

The [`docs/`](docs/README.md) directory covers configuration, on-disk state, session files, commands, and a complete first run.

## Installation

Unix only: it relies on PTYs and process-group signals (`killpg`).

### Cargo (recommended)

For normal use, install the published crate from [crates.io](https://crates.io/crates/fleetcom):

```sh
cargo install fleetcom
```

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
| `s` | cycle grouping: by state / by directory / by custom group |
| `m` | tag the task "in use" (prioritizes it within the active grouping) |
| `g` | assign the selected task to a named group (picker: pick, create, or clear) |
| `R` | rename the selected task: a display name shown in place of the command (an empty prompt clears it) |
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

Every task runs in its own pseudo-terminal, emulated with `alacritty_terminal`. `fleetcom` answers cursor-position and device-attribute queries, renders `?2026` synchronized updates as whole frames, and reflows history after a resize. The dashboard preview, peek overlay, and attached view all read the same emulated screen grid. As a result, full-screen programs such as `vim` and `htop` retain one consistent terminal state across views. Backgrounding changes client focus without notifying the child.

### Input fidelity

Attached input follows the terminal modes reported by the child. Modified Enter becomes `ESC CR` when the terminal reports the modifier. Paste receives bracketed-paste markers only when the child enables them. Mouse events go to children that request a mouse protocol. Full-screen children receive alternate-scroll input only while DECSET 1007 is enabled; otherwise `fleetcom` suppresses wheel events. Each task retains 2,000 lines of scrollback. For inline children, wheel-up or `Shift+PageUp` enters history; paging keys navigate it, while `Esc` or ordinary input returns to live output. [`docs/commands.md`](docs/commands.md) documents the exact routing rules.

### Jobs outlive the UI

A per-user daemon owns the processes and their terminals. `q` disconnects the client but leaves the daemon and its jobs running; the next `fleetcom` invocation reattaches. Each launch uses the environment and working directory of the client that requested it. This means a job launched from a virtual environment sees that environment even when another client originally started the daemon.

`Q` and `fleetcom --kill` stop the daemon and send `SIGTERM` to each job's process group, followed by `SIGKILL` after a two-second grace period. Sending `SIGTERM`, `SIGINT`, or `SIGHUP` directly to the daemon uses the same shutdown path. `fleetcom --kill` reads the daemon PID from the lock file, so it also works while another client occupies the socket. If the connection drops, the client discards the task snapshot it can no longer verify and offers to reconnect.

Signals target each task's process group. `fleetcom` leaves an exited leader unreaped until final cleanup, preserving the process-group ID so background children remain signalable. A process that creates a new session or double-forks out of the group is outside `fleetcom`'s control.

### Grouping and launch targets

The dashboard groups tasks by state (In use / Running / Completed), working directory, or names assigned with `g`. Custom groups sort by name, with Unassigned last. `@` opens a live directory picker with the current directory first, recently used directories next, and matching subdirectories after them. This provides an explicit launch directory without leaving the dashboard.

### Task names

`R` gives the selected task a display name. The dashboard row and peek title show the name in place of the command; the attached status bar shows `name · command`. An empty prompt clears the name, and rerun preserves it. `fleetcom` removes control characters, trims surrounding whitespace, and limits names to 64 characters.

### Sessions

`w`, `o`, and `fleetcom <name>` save or load named recipes. A recipe retains each task's directory, command, group assignment, and display name. Loading starts new processes; it does not recover the processes that existed when the recipe was saved. Process continuity comes from the daemon, while sessions provide repeatable launches.

### Agent session resume

Relaunching a bare `claude`, `codex`, or `grok` command starts another conversation. `fleetcom` captures the ID and stores a resuming command (`claude --resume '<id>'`, `codex resume '<id>'`, or `grok --resume '<id>'`) when saving or rerunning (`r`) a task. If capture is unavailable (piped command, excluded subcommand), `fleetcom` keeps the original command. [`agent-resume.md`](src/harness/agent-resume.md) documents the capture and rewrite mechanics.

## Notes

`fleetcom` targets concurrent build, test, watch, server, and interactive-agent processes. It is deliberately narrower than a terminal multiplexer: each task is one command, not a persistent shell session.

### When to use it

- Several long-lived commands need one place for observation, tagging, and attachment.
- Jobs must survive a terminal closing and remain available for reattachment.
- The same command set is launched often enough to justify a saved session.

### When to avoid it

- For interactive multiplexing of persistent shells, use `tmux`; `fleetcom` runs one command per task.
- It is not a full process manager: the fleet's lifetime is bounded by the daemon's (see the first limitation below).

### Known limitations

- The fleet dies with the daemon. The daemon holds every PTY master, so daemon termination closes the terminals and the kernel sends `SIGHUP` to each task's process group. A clean shutdown (`Q`, `--kill`, or `SIGTERM`) sends `SIGTERM` first and escalates to `SIGKILL` after two seconds. A crash or direct `SIGKILL` skips that grace period. HUP-immune jobs (`nohup`, `trap '' HUP`) can survive, but the next daemon does not own or display them. A panic while serving one client only drops that connection; the daemon process remains the fleet's single point of failure.
- Commands run through the client's non-interactive shell (`$SHELL -c`, or `/bin/sh` when `SHELL` is unset), so functions and aliases defined in `~/.zshrc` are not available.
- The daemon serves one client at a time. A second `fleetcom` prints a waiting notice, then attaches when the active client disconnects (`q`).
