# Commands

`fleetcom` has two control surfaces: launch arguments select the operating mode, while keys control the dashboard and its overlays.

## Invocation

| Invocation | Effect |
| -- | -- |
| `fleetcom` | Connect to the daemon (autostarting it if needed) and open the dashboard |
| `fleetcom <session>` | Load a saved [session](sessions.md) at startup, then open the dashboard |
| `fleetcom --foreground` | Run the core in-process, no daemon; jobs die when you quit |
| `fleetcom --kill` | Stop the daemon and kill every job it owns; works even while another client is attached (it signals the daemon rather than queueing behind the socket) |
| `fleetcom --help` / `-h` | Print usage and exit |
| `fleetcom --version` / `-V` | Print the version and exit |

`--daemon` is internal. An ordinary invocation starts it when necessary. The first non-`-` argument is treated as the session name; a second is rejected.

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

`Space` opens a read-only overlay containing the selected task's live screen. `↑`/`↓` move between tasks without closing the overlay; `Space`, `Esc`, or `q` closes it. `Enter`, from either the dashboard or peek, attaches to the task and forwards input to its PTY. Full-screen programs such as `vim` and `htop` retain their terminal state and cursor.

#### Attach and background

While attached, `Ctrl-\` returns to the dashboard. Every other key, including `Ctrl-C`, `Ctrl-Z`, and `Ctrl-D`, is forwarded to the child. `Ctrl-\` refers to the physical chord; the input handler accepts both `Ctrl-\` and the `Ctrl-4` representation produced by crossterm's legacy decoder.

#### Destroy is Shift-gated

`X` (capital) kills the selected task if it's running, or removes it from the list if it's finished. Removal also sweeps anything the job left in its process group (a `cmd &` child, for instance): `TERM` at removal, `KILL` after the 2 s grace, behind the already-gone row. Lowercase `x` is a deliberate no-op: the same guard as `Q` vs. `q`. It is *not* `Ctrl-X`: the terminal sends byte `0x18` for both `Ctrl+x` and `Ctrl+Shift+X`, with no shift bit, so a Ctrl chord can't carry the distinction. Only an unmodified capital reliably means "yes, destroy this."

#### Rerun

`r` re-executes a *finished* task's command using the same command string and directory, under the environment of the client requesting the rerun (launch context always belongs to whoever asks for the launch). The task retains its ID, `◆` tag, and list position; its clock and screen reset. On a running task, `r` is a no-op because rerunning would first require a destructive kill. Rerun also works inside peek, which keeps the result visible while starting the next run.

#### Detach vs. quit

`q` (and `Ctrl-C`) disconnects the client and leaves the daemon and its jobs running; the next `fleetcom` reattaches. `Q` kills every job (`TERM` to each process group, `KILL` after a 2 s grace for any that ignore it) and stops the daemon. `Ctrl-C` is intercepted only in the dashboard; while attached it belongs to the child.

#### Grouping and tagging

`s` toggles between grouping by state (In use / Running / Completed) and by working directory. `m` toggles the "in use" tag on the selected task; tagged tasks are marked `◆` and pinned to the top, so the handful you're actively steering stay reachable as the list grows.

## The `@` directory picker

`@` opens a bottom panel: a typed-path field, plus the directories that match it. There are three kinds of row, and `Enter` does the right thing for each:

- Current directory: run the command right here (`Enter`).
- Recent directories: ones you've launched in before; `Enter` runs there, `Tab`/`→` browses into them.
- Subdirectories of the current path: `Enter` or `Tab`/`→` descends into one.

Typing filters the rows; `Backspace` climbs the typed path; `↑`/`↓` move the highlight; `Esc` cancels. Completion updates on each input, permitting navigation and launch without leaving the dashboard.

## Peek

A centered box over the dashboard showing the selected task's live screen (the last screenful). `↑`/`↓` (or `k`/`j`) switch which task you're peeking at; `Enter` attaches to it; `r` reruns it if it has finished; `Space`, `Esc`, or `q` closes.

## Attached

The task owns the terminal, and its status bar reads `[attached] <command>    Ctrl-\ background`. `Ctrl-\` returns to the dashboard; every other key (control chords included) goes to the child.

## When the daemon drops

If the daemon connection is lost, the client clears the task list because it can no longer verify that state. `r` reconnects in daemon-backed mode; `q` quits. A `--foreground` core has no external process to reconnect to, so only quit is available.
