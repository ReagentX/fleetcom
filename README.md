# `fleetcom`

`fleetcom` supervises your local development fleet: multiplexers, application servers, REPLs, builds, tests, and AI agent sessions.

Running several long-lived commands is pesky once they span terminal panes or need to survive a disconnect. `fleetcom` gives each command its own PTY and exposes every screen through one dashboard. A daemon owns the jobs, so closing the client does not stop them.

## Key Features

### Fleet view

See every task’s state and latest output from one dashboard.

![`fleetcom` fleet view](docs/img/home.png)

### Quick peek

Press `Space` to quick peek a task's live screen without attaching to it.

![`fleetcom` quick peek](docs/img/quickpeek.png)

### Attach and interact

Press `Enter` to take control of a task, then `^\` to return to the fleet without interrupting it.

![`fleetcom` attach](docs/img/attach.png)

## Operational model

- Runs each command in its own PTY and groups tasks by state, by working directory, or by custom named groups.
- Keeps jobs running after the client disconnects.
- Saves and reloads task recipes with directories, commands, group assignments, and display names.
- Preserves supported `claude`, `codex`, and `grok` conversations so saved or rerun tasks can resume them.
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

| Invocation | Behavior |
| -- | -- |
| `fleetcom` | Connect to the daemon, autostarting it when necessary, and open the dashboard |
| `fleetcom <session>` | Load a saved session, then open the dashboard |
| `fleetcom --foreground` | Run in-process without a daemon; jobs stop when the client quits |
| `fleetcom --kill` | Stop the daemon and every job it owns |
| `fleetcom --help` / `--version` | Print usage or version information and exit |

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
| `q` | disconnect; leave the daemon and jobs running |
| `Q` | quit; kill the jobs and stop the daemon |

### Attached

| Key | Command |
| -- | -- |
| `Ctrl-\` | background the task and return to the dashboard |
| anything else | forwarded to the task's PTY |

[`docs/commands.md`](docs/commands.md) covers every key and launch flag, including the routing mechanics.

## How it works

Every task runs in its own pseudo-terminal, emulated with `alacritty_terminal`. The dashboard preview, peek overlay, and attached view all read the same emulated screen grid, so full-screen programs such as `vim` and `htop` retain one consistent terminal state across views. [`docs/how-it-works.md`](docs/how-it-works.md) documents the terminal emulation, input routing, and activity windows.

## Scope and tradeoffs

`fleetcom` targets concurrent build, test, watch, server, and interactive-agent processes. Each task is one command rather than a persistent shell session.

### When to use `fleetcom`

- Several long-lived commands need one place for observation, tagging, and attachment.
- Jobs must survive a terminal closing and remain available for reattachment.
- The same command set is launched often enough to justify a saved session.
- Agent sessions (`claude`, `codex`, `grok`) must resume their conversations on rerun rather than start new ones.

### When to avoid `fleetcom`

- You primarily need persistent interactive shell workspaces; use `tmux` or `zellij` directly. `fleetcom` can supervise a multiplexer, but it does not replace one.
- You need a full process manager: the fleet’s lifetime is bounded by the daemon’s.

### Operational limits

- The fleet dies with the daemon: the daemon process is the fleet's single point of failure.
- Commands run through the client's non-interactive shell (`$SHELL -c`, or `/bin/sh` when `SHELL` is unset), so functions and aliases defined in `~/.zshrc` are not available.
- The daemon serves one client at a time.

[Operational constraints](docs/README.md#operational-constraints) documents the shutdown and signal mechanics.
