# `fleetcom`

Use `fleetcom` to supervise your local development fleet: multiplexers, application servers, REPLs, builds, tests, and AI agent sessions.

## Key Features

### Fleet view

Monitor every task's state and live output from one dashboard: test results, dev servers, and which agent is waiting on you.

![`fleetcom` fleet view](docs/img/home.png)

### Quick peek

Press `Space` for a quick peek at a task’s live screen without attaching to it.

![`fleetcom` quick peek](docs/img/quickpeek.png)

### Attach and interact

Press `Enter` to take control of a task, then `Ctrl-\` to return to the dashboard without interrupting it.

![`fleetcom` attach](docs/img/attach.png)

### Custom groups

Organize related tasks into named groups across working directories.

![`fleetcom` custom-group view](docs/img/groups.png)

### Resume agent sessions

Start `claude`, `codex`, `grok`, or `omp` normally. Rerun the task or reload a saved session to resume the conversation.

## Operational model

Managing several long-lived commands across terminal panes is pesky, especially when you need to reconnect later. With `fleetcom`:

- Run each command in its own PTY and group tasks by state, working directory, or named group.
- Keep tasks running under a daemon across client disconnects.
- Save and reload task recipes: directories, commands, group assignments, and display names.
- Rerun a completed task in place, keeping its identity, group, and name.
- Save and rerun `claude`, `codex`, `grok`, and `omp` tasks with captured conversation IDs.
- Recover the current task set from automatic snapshots.

## Documentation

See [`docs/`](docs/README.md) for configuration, on-disk state, session files, resuming supported agent sessions, commands, and a complete first run.

## Installation

Unix only: PTYs and process-group signals (`killpg`) are required.

### Cargo (recommended)

For normal use, install the published crate from [crates.io](https://crates.io/crates/fleetcom):

```sh
cargo install fleetcom
```

See [Source installation](docs/README.md#installation-from-source) to build from a repository clone.

## Usage

Connect to the daemon, autostarting it when necessary, and open the dashboard by invoking:

```sh
fleetcom
```

See the [invocation reference](docs/commands.md#invocation) for sessions, foreground mode, scrollback, and daemon shutdown.

## Key Commands

### Dashboard

Use the two dashboard hints for common actions; press `?` for the expanded key reference:

```text
  ❯ n run · @ dir · / find · s sort
  ↑↓ select · enter attach · space peek · ? controls
```

### Attached

- Press `Ctrl-\` to background the task and return to the dashboard.
- Other supported input is forwarded to the task's PTY.

See [`docs/commands.md`](docs/commands.md#dashboard) for every key and launch flag, including the routing mechanics.

## How it works

`fleetcom` runs each task in a separate pseudo-terminal, emulated with `alacritty_terminal`. The dashboard preview, peek overlay, and attached view all read the same emulated screen grid. This preserves terminal state as you move between views, including for full-screen programs such as `vim` and `htop`. See [`docs/how-it-works.md`](docs/how-it-works.md) for terminal emulation, input routing, and activity grouping.

## Scope and tradeoffs

Use `fleetcom` for concurrent build, test, watch, server, and interactive-agent processes. Each task is one command rather than a persistent shell session.

### When to use `fleetcom`

- You need one place to observe, tag, and attach to several long-lived commands.
- You need to reattach to jobs after closing a terminal.
- The same command set is launched often enough to justify a saved session.
- Captured agent conversations (`claude`, `codex`, `grok`, `omp`) should resume on rerun.

### When to avoid `fleetcom`

- You primarily need persistent interactive shell workspaces; use `tmux` or `zellij` directly. You can supervise a multiplexer with `fleetcom`, but cannot use it as a persistent shell workspace.
- You need a full process manager: the fleet’s lifetime is bounded by the daemon’s.

### Operational limits

- The fleet's lifetime is bounded by the daemon's: the daemon process is the single point of failure.
- Commands are executed through the client's non-interactive shell (`$SHELL -c`, or `/bin/sh` when `SHELL` is unset), so functions and aliases defined in `~/.zshrc` are not available.
- Only one client can be connected to the daemon at a time.

See [Operational constraints](docs/README.md#operational-constraints) for shutdown and signal mechanics.
