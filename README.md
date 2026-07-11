# multi

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

## Installation

Unix only — it relies on PTYs and process-group signals (`killpg`).

### From source (recommended)

From the project root:

- `cargo install --path .` to install `multi` on your `PATH`, or
- `cargo build --release` and run `target/release/multi`.

## Usage

There are a few ways to invoke `multi`:

- `multi`
  - Connects to the daemon (autostarting it if needed) and opens the dashboard
- `multi <session>`
  - Loads a saved session at startup, then opens the dashboard
- `multi --foreground`
  - Runs everything in-process, without a daemon (jobs die when you quit)
- `multi --kill`
  - Kills the daemon and every job it owns

The daemon starts itself the first time you run `multi`; you never invoke
`multi --daemon` directly.

## Key Commands

### Dashboard

| Key | Command |
|--|--|
| ↑ ↓ / `k` `j` | move the selection |
| `Enter` | attach to the selected task |
| `Space` | peek at the selected task |
| `n` | new command in the current directory |
| `@` | new command in a directory you pick (with completion) |
| `s` | toggle grouping: by state / by directory |
| `m` | tag the task "in use" (pins it to the top) |
| `^X` | kill a running task, or remove a finished one |
| `w` | save the current tasks as a session |
| `o` | load a saved session |
| `q` | disconnect — leave the daemon and jobs running |
| `Q` | quit — kill every job and stop the daemon |

### Attached

| Key | Command |
|--|--|
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
client and leaves everything running; the next `multi` reattaches. `Q` (or
`multi --kill`) tears it all down. If the daemon dies, the client says so and
offers to reconnect rather than freezing on a stale view.

### Grouping and the `@` picker

Group the fleet by state (In use / Running / Completed) or by working directory.
`@` opens a live directory picker — the current dir first, recently used dirs
next, matching subdirectories below — so you can launch a command anywhere
without leaving the dashboard.

### Sessions

Save the current set of `{directory: [commands]}` as a named recipe and reload
it later (`w` / `o`, or `multi <name>`). Loading re-runs the commands; it does
not resurrect live processes — that is the daemon's job.

## Notes

`multi` mimics the multi-pane "fleet view" of an agentic coding session, but for
any shell command. It is built for supervising several concurrent, long-running
commands at once: build/test/watch loops, servers, and interactive agent CLIs
that sit idle awaiting input.

### When to use it

- You run several long-lived commands and want one place to watch, tag, and
  attach to them
- You want those jobs to survive closing your terminal, and to reattach later
- You launch the same commands often and want them saved as a session

### When to avoid it

- For interactive multiplexing of shells you drive by hand, use `tmux` — `multi`
  runs one command per pane, not a shell session
- It is not a full process manager: crash-resilient ownership (adopting jobs
  after a daemon *crash*, as opposed to a clean shutdown) is out of scope

Commands run through a non-interactive shell (`$SHELL -c`), so functions and
aliases defined in your `~/.zshrc` are not available — an opt-in interactive
mode for that is planned.
