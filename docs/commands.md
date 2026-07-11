# Commands

`fleetcom` has two command surfaces: **keys** inside the dashboard and its overlays, and **flags** at launch.

## Invocation

| Invocation | Effect |
| -- | -- |
| `fleetcom` | Connect to the daemon (autostarting it if needed) and open the dashboard |
| `fleetcom <session>` | Load a saved [session](sessions.md) at startup, then open the dashboard |
| `fleetcom --foreground` | Run the core in-process, no daemon; jobs die when you quit |
| `fleetcom --kill` | Stop the daemon and kill every job it owns; works even while another client is attached (it signals the daemon rather than queueing behind the socket) |
| `fleetcom --help` / `-h` | Print usage and exit |
| `fleetcom --version` / `-V` | Print the version and exit |

`--daemon` exists but is internal: the first `fleetcom` autostarts it for you, and you never invoke it by hand. The first non-`-` argument is taken as the session name (one at most).

## Dashboard

| Key | Command |
| -- | -- |
| `↑` `↓` / `k` `j` | Move the selection |
| `Enter` | Attach to the selected task |
| `Space` | Peek at the selected task |
| `n` | New command in the invocation directory |
| `@` | New command in a directory you pick |
| `s` | Toggle grouping: by state / by directory |
| `m` | Tag the selected task "in use" (toggles) |
| `r` | Rerun a finished task: same command, same directory, same row |
| `X` | Kill a running task (`TERM`, then `KILL` after 2 s), or remove a finished one |
| `w` | Save the current tasks as a session |
| `o` | Load a saved session |
| `q` (or `Ctrl-C`) | Disconnect; leave the daemon and jobs running |
| `Q` | Quit; kill the jobs and stop the daemon |

### Status glyphs

| Glyph | Meaning |
| -- | -- |
| `✻` | Running: active, producing output |
| `∙` | Idle: running, but quiet |
| `✓` | Completed, exit 0 |
| `✗` | Completed, non-zero exit |
| `◆` | Tagged "in use" |

### Mechanics

#### Peek vs. attach

`Space` opens a read-only overlay: the selected task's live screen, framed in a box and updating as it runs. `↑`/`↓` move between tasks *without leaving peek*, so you can flip through the fleet; `Space`, `Esc`, or `q` closes it. `Enter` (from the dashboard or from peek) *attaches*: the task takes the whole terminal and your keystrokes go to it, cursor and all, so a live `vim` or `htop` behaves exactly as it would on its own.

#### Attach and background

While attached, one key is reserved: `Ctrl-\` backgrounds the task and returns you to the dashboard. Everything else (including `Ctrl-C`, `Ctrl-Z`, `Ctrl-D`) is forwarded verbatim to the child, so the program never learns it lost the foreground. (`Ctrl-\` is the physical chord; it works whether the terminal reports it as `Ctrl-\` or, under crossterm's legacy decoder, `Ctrl-4`.)

#### Destroy is Shift-gated

`X` (capital) kills the selected task if it's running, or removes it from the list if it's finished. Lowercase `x` is a deliberate no-op: the same guard as `Q` vs. `q`. It is *not* `Ctrl-X`: the terminal sends byte `0x18` for both `Ctrl+x` and `Ctrl+Shift+X`, with no shift bit, so a Ctrl chord can't carry the distinction. Only an unmodified capital reliably means "yes, destroy this."

#### Rerun

`r` re-executes a *finished* task's command — the same command string, in the same directory, under the same daemon-captured environment as every spawn — in the same row: the task keeps its id, its `◆` tag, and its list position; only the clock and the screen reset. On a running task `r` is a no-op: a rerun that had to kill first would be destructive, and destroy is `X`'s Shift-gated job. It also works from inside peek, so you can read a result and rerun it without closing the overlay.

#### Detach vs. quit

`q` (and `Ctrl-C`) disconnects the client and leaves the daemon and its jobs running; the next `fleetcom` reattaches. `Q` kills every job (`TERM` to each process group, `KILL` after a 2 s grace for any that ignore it) and stops the daemon. `Ctrl-C` is intercepted only in the dashboard; while attached it belongs to the child.

#### Grouping and tagging

`s` toggles between grouping by state (In use / Running / Completed) and by working directory. `m` toggles the "in use" tag on the selected task; tagged tasks are marked `◆` and pinned to the top, so the handful you're actively steering stay reachable as the list grows.

## The `@` directory picker

`@` opens a bottom panel: a typed-path field, plus the directories that match it. There are three kinds of row, and `Enter` does the right thing for each:

- Current directory: run the command right here (`Enter`).
- Recent directories: ones you've launched in before; `Enter` runs there, `Tab`/`→` browses into them.
- Subdirectories of the current path: `Enter` or `Tab`/`→` descends into one.

Type to filter; `Backspace` climbs back up the typed path; `↑`/`↓` move the highlight; `Esc` cancels. It's live completion, so you can walk anywhere in the tree and launch there without leaving the dashboard.

## Peek

A centered box over the dashboard showing the selected task's live screen (the last screenful). `↑`/`↓` (or `k`/`j`) switch which task you're peeking at; `Enter` attaches to it; `r` reruns it if it has finished; `Space`, `Esc`, or `q` closes.

## Attached

The task owns the terminal, and its status bar reads `[attached] <command>    Ctrl-\ background`. `Ctrl-\` returns to the dashboard; every other key (control chords included) goes to the child.

## When the daemon drops

If the connection to the daemon is lost, the screen clears to a banner. The old task list would be a lie, since those jobs died with the daemon. `r` reconnects (when daemon-backed); `q` quits. A `--foreground` core has nothing to reconnect to, so it only offers quit.
