# `fleetcom`

`fleetcom` supervises your local development fleet: multiplexers, application servers, REPLs, builds, tests, and AI agent sessions.

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

### Resume agent sessions

Start `claude`, `codex`, or `grok` normally. When you rerun the task or reload a saved session, `fleetcom` resumes the same conversation automatically.

![`fleetcom` group view](docs/img/groups.png)

## Operational model

Running several long-lived commands is pesky once they span terminal panes or need to survive a disconnect. `fleetcom`:

- Runs each command in its own PTY and groups tasks by state, working directory, or named group.
- Delegates tasks to a daemon, so a disconnecting client stops nothing.
- Saves and reloads task recipes: directories, commands, group assignments, and display names.
- Reruns a completed task in place, keeping its identity, group, and name.
- Preserves `claude`, `codex`, and `grok` conversations, so saved or rerun tasks resume instead of starting fresh.
- Automatically snapshots the current task set for recovery.

## Documentation

The [`docs/`](docs/README.md) directory covers configuration, on-disk state, session files, resuming supported agent sessions, commands, and a complete first run.

## Installation

Unix only: it relies on PTYs and process-group signals (`killpg`).

### Cargo (recommended)

For normal use, install the published crate from [crates.io](https://crates.io/crates/fleetcom):

```sh
cargo install fleetcom
```

[`docs/commands.md`](docs/README.md#installation-from-source) covers advanced installation.

## Usage

Connect to the daemon, autostarting it when necessary, and open the dashboard by invoking:

```sh
fleetcom
```

[`docs/commands.md`](docs/commands.md#invocation) covers advanced usage.

## Key Commands

### Dashboard

The dashboard shows two short key hints; `?` opens an expanded key reference:

```text
  ❯ n run · @ dir · / find · s sort
  ↑↓ select · enter attach · space peek · ? controls
```

### Attached

- `Ctrl-\` backgrounds the task and returns to the dashboard.
- Every other key reaches the task's PTY.

[`docs/commands.md`](docs/commands.md#dashboard) covers every key and launch flag, including the routing mechanics.

## How it works

Every task runs in its own pseudo-terminal, emulated with `alacritty_terminal`. The dashboard preview, peek overlay, and attached view all read the same emulated screen grid, so full-screen programs such as `vim` and `htop` retain one consistent terminal state across views. [`docs/how-it-works.md`](docs/how-it-works.md) documents the terminal emulation, input routing, and activity grouping.

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
