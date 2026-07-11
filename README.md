# fleetcom

A fleet-view supervisor for arbitrary shell commands.

Run any number of commands, each in its own PTY, and watch them from one live
dashboard — grouped by status, ready to peek at, attach to, or leave running in
the background. A daemon owns the jobs, so they outlive the UI: disconnect from
one terminal and reattach from another.

## tl;dr

- Run each command in its own PTY; a live dashboard groups them by status
- Peek at any task, attach to drive it, background it again with a keystroke
- A background daemon keeps jobs running after you disconnect — reattach any time
- Save and reload `{directory: [commands]}` sessions
- Group by state or working directory; pick a directory to launch in with `@`

## Documentation

Deeper reference — configuration and on-disk layout, the session format, every
command, and a first-run walkthrough — lives in [`docs/`](docs/README.md).

## Installation

Unix only — it relies on PTYs and process-group signals (`killpg`).

### From source (recommended)

From the project root:

- `cargo install --path .` to install `fleetcom` on your `PATH`, or
- `cargo build --release` and run `target/release/fleetcom`.

## Usage

There are a few ways to invoke `fleetcom`:

- `fleetcom`
  - Connects to the daemon (autostarting it if needed) and opens the dashboard
- `fleetcom <session>`
  - Loads a saved session at startup, then opens the dashboard
- `fleetcom --foreground`
  - Runs everything in-process, without a daemon (jobs die when you quit)
- `fleetcom --kill`
  - Kills the daemon and its running jobs

The daemon starts itself the first time you run `fleetcom`; you never invoke
`fleetcom --daemon` directly.

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
| `X` | kill a running task, or remove a finished one (Shift-gated) |
| `w` | save the current tasks as a session |
| `o` | load a saved session |
| `q` | disconnect — leave the daemon and jobs running |
| `Q` | quit — kill the jobs and stop the daemon |

### Attached

| Key | Command |
| -- | -- |
| `Ctrl-\` | background the task and return to the dashboard |
| anything else | forwarded to the task's PTY |

## Features

### One PTY per command

Every task runs in its own pseudo-terminal, emulated with `vt100`. The same
screen grid powers the dashboard preview, the peek overlay, and full attached
rendering — so a mid-run `vim` or `htop` shows its real, live screen, and
backgrounding an attached task never tells the child it lost the foreground.

### Jobs outlive the UI

A per-user daemon owns the processes and their terminals. `q` disconnects the
client and leaves everything running; the next `fleetcom` reattaches. `Q` (or
`fleetcom --kill`) group-kills the running jobs and stops the daemon. If the daemon
dies, the client says so and offers to reconnect rather than freezing on a stale
view.

Teardown caveat: `Q` signals each job's *process group*. A job that
re-backgrounds itself past its own shell's exit (`cmd &`, then the shell exits)
leaves that group and survives — kill it by hand. This is deliberate: once the
shell is reaped its PID can be recycled, so signalling the old group could hit
an unrelated process.

### Grouping and the `@` picker

Group the fleet by state (In use / Running / Completed) or by working directory.
`@` opens a live directory picker — the current dir first, recently used dirs
next, matching subdirectories below — so you can launch a command anywhere
without leaving the dashboard.

### Sessions

Save the current set of `{directory: [commands]}` as a named recipe and reload
it later (`w` / `o`, or `fleetcom <name>`). Loading re-runs the commands; it does
not resurrect live processes — that is the daemon's job.

## Notes

`fleetcom` mimics the multi-pane "fleet view" of an agentic coding session, but for
any shell command. It is built for supervising several concurrent, long-running
commands at once: build/test/watch loops, servers, and interactive agent CLIs
that sit idle awaiting input.

### When to use it

- You run several long-lived commands and want one place to watch, tag, and
  attach to them
- You want those jobs to survive closing your terminal, and to reattach later
- You launch the same commands often and want them saved as a session

### When to avoid it

- For interactive multiplexing of shells you drive by hand, use `tmux` — `fleetcom`
  runs one command per pane, not a shell session
- It is not a full process manager: crash-resilient ownership (adopting jobs
  after a daemon *crash*, as opposed to a clean shutdown) is out of scope

### Known limitations

- Commands run through a non-interactive shell (`$SHELL -c`), so functions and
  aliases defined in your `~/.zshrc` are not available — an opt-in interactive
  mode for that is planned.
- The daemon captures the environment of the client that **first** starts it and
  runs every job under that environment. A second terminal with a different
  `PATH` or virtualenv attaches to the same daemon, and its commands resolve
  against the first terminal's environment, not its own.
- The daemon serves **one client at a time**; a second `fleetcom` connects but
  waits until the first disconnects (`q`).
