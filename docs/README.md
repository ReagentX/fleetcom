# fleetcom Documentation

How to install, configure, and drive `fleetcom`, and where it keeps its files.

## Index

- [Commands](commands.md) — every key and launch flag, with the mechanics behind them
- [Sessions](sessions.md) — the `{directory: [commands]}` recipe format and where it lives
- [Directory & Environment Configuration](#directory--environment-configuration) — the socket, the lock, and the session paths
- [Sample Usage Session](#sample-usage-session) — a first run, start to finish
- [Notes & Caveats](#notes--caveats) — the sharp edges

## Advanced Installation

`fleetcom` is Unix-only — it relies on PTYs and process-group signals (`killpg`).

`cargo install fleetcom` is the normal path once it's published. To build from a clone:

- `cargo test` — confirm the suite passes
- `cargo build --release` — compile to `target/release/fleetcom`
- `cargo install --path .` — put `fleetcom` on your `PATH`

## Directory & Environment Configuration

`fleetcom` writes two kinds of state, in two different places: **runtime** state — the daemon's socket and lock, ephemeral — and **config** state — your saved sessions, durable.

### Runtime directory (socket + lock)

Holds `default.sock` (the client↔daemon socket, mode `0600`) and `daemon.lock` (the single-instance `flock`). The directory is created `0700` and validated: if it already exists it must be a real directory this user owns — a symlink or a directory planted by someone else is rejected, so a shared `/tmp` can't be used to hijack the socket.

Resolved in this order:

| Order | Condition | Path |
| -- | -- | -- |
| 1 | `FLEETCOM_RUNTIME_DIR` is set | `$FLEETCOM_RUNTIME_DIR` (verbatim) |
| 2 | `$XDG_RUNTIME_DIR` is set and non-empty (Linux) | `$XDG_RUNTIME_DIR/fleetcom` |
| 3 | otherwise | `$TMPDIR/fleetcom-$uid` |

On macOS `$TMPDIR` is already per-user; the `$uid` suffix on the fallback is what keeps users apart on a shared `/tmp` (an XDG-less Linux box).

### Config directory (sessions)

Holds saved sessions under a `sessions/` subdirectory — one `<name>.json` per session. See [Sessions](sessions.md) for the format.

| Order | Condition | Path |
| -- | -- | -- |
| 1 | `FLEETCOM_CONFIG_DIR` is set | `$FLEETCOM_CONFIG_DIR/sessions` |
| 2 | Linux | `${XDG_CONFIG_HOME:-~/.config}/fleetcom/sessions` |
| 2 | macOS | `~/Library/Application Support/fleetcom/sessions` |

The platform default is [`dirs::config_dir()`](https://docs.rs/dirs/latest/dirs/fn.config_dir.html) joined with `fleetcom`. The directory is created on the first save.

## Sample Usage Session

A first run, from an empty dashboard to a saved, backgrounded fleet. The frames below are illustrative — schematic of the real layout, not pixel captures.

Start it. The first `fleetcom` autostarts the daemon and opens an empty dashboard:

```text
  fleetcom   0 running · 0 idle · 0 done      by state

  ❯ n run · @ dir · s sort · w save · o load
  ↑↓ select · enter attach · space peek · n/@ new · s sort · m tag · X kill · q detach · Q quit
```

Press `n`, type a command, `Enter`. It runs in its own PTY and shows up under **Running**. Add a second the same way:

```text
  fleetcom   2 running · 0 idle · 0 done      by state

  Running
  ✻  cargo watch -x test      test result: ok. 42 passed       9s
  ✻  npm run dev              VITE v5.0  ready in 312 ms         4s

  ❯ n run · @ dir · s sort · w save · o load
  ↑↓ select · enter attach · space peek · n/@ new · s sort · m tag · X kill · q detach · Q quit
```

Each row is `glyph · tag · command · latest output · age`. `Space` peeks — a read-only box of the selected task's live screen, without leaving the dashboard:

```text
  ┌─ cargo watch -x test ───────────────────────────────┐
  │ running 3 tests                                      │
  │ test result: ok. 42 passed; 0 failed                 │
  │                                                      │
  └ space/esc close · enter attach ─────────────────────┘
```

`Enter` attaches — the task takes the whole terminal and your keystrokes go to it. The status bar shows the one reserved key:

```text
  [attached] npm run dev                       Ctrl-\ background
```

`Ctrl-\` backgrounds it and returns to the dashboard. `m` tags the selected task "in use" — it gets a `◆` and pins to the top:

```text
  fleetcom   2 running · 0 idle · 0 done      by state

  In use
  ✻ ◆cargo watch -x test      test result: ok. 42 passed      1m

  Running
  ✻  npm run dev              VITE v5.0  ready in 312 ms        1m
```

`w`, a name, `Enter` saves the fleet as a [session](sessions.md). Now `q` disconnects — the daemon and both jobs keep running without you. Run `fleetcom` again and you reattach to exactly this dashboard. `Q` (or `fleetcom --kill`) group-kills the jobs and stops the daemon.

## Notes & Caveats

- **Commands run through a non-interactive shell** (`$SHELL -c`), so functions and aliases from your `~/.zshrc` aren't available. An opt-in interactive mode is planned.
- **The daemon captures the environment of the client that _first_ starts it** and runs every job under that environment. A second terminal with a different `PATH` or virtualenv attaches to the same daemon, and its commands resolve against the first terminal's environment, not its own.
- **The daemon serves one client at a time.** A second `fleetcom` connects but waits until the first disconnects (`q`).
- **`Q` signals each job's _process group_.** A job that re-backgrounds itself past its own shell's exit (`cmd &`, then the shell exits) leaves that group and survives — kill it by hand. This is deliberate: once the shell is reaped its PID can be recycled, so signalling the old group could hit an unrelated process.
- **`--foreground` is ephemeral.** It runs the core in-process with no daemon, so the jobs die when you quit and there is nothing to reattach to.
- **Crash-resilient ownership is out of scope.** Adopting jobs after a daemon _crash_ (as opposed to a clean shutdown) is not supported.
