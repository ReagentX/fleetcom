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
| `s` | Cycle grouping: by state / by directory / by custom group |
| `m` | Tag the selected task "in use" (toggles) |
| `g` | Assign the selected task to a group (opens the group picker) |
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

#### Shift+Enter, paste, and the wheel

Three inputs are richer than a keypress, and each is routed by state rather than forwarded blind:

- Shift+Enter and Alt+Enter are sent as `ESC CR`, which is distinct from plain Enter. Shift requires a terminal that reports modified keys; terminals that do not report it send plain `CR`.
- Paste travels as one message. Bracketed-paste-aware children receive paste markers with embedded terminators removed; other children receive line endings as `CR`. Fleetcom text fields strip control characters.
- Mouse-protocol children receive clicks, drags, releases, and wheel events in the negotiated encoding. Full-screen children without a mouse protocol use alternate scroll. For inline children without a mouse protocol, wheel-up enters Fleetcom's scrollback view. When Fleetcom captures the mouse (for mouse-protocol children, inline children, or scrollback), terminal selection requires the terminal's selection-override modifier. Otherwise, drag selects normally.

#### Scrollback

Tasks retain 2,000 lines of scrollback. While attached to an inline child, wheel-up over its output enters scrollback; `Shift+PageUp` also enters it (`Ctrl+PageUp` and `Alt+PageUp` work when Shift is intercepted). The status bar shows `[scroll ↑N]`. The wheel scrolls, `PageUp`/`PageDown` move by pages, `↑`/`↓` by lines, and `Home` jumps to the oldest row. `Esc`, `Enter`, `q`, `End`, or reaching the bottom returns to live output. Typing also returns to live and forwards the key. Detaching or switching tasks resets the view.

#### Destroy is Shift-gated

`X` kills a running task or removes a finished one. Removal also terminates remaining processes in the task's process group, escalating from `TERM` to `KILL` after two seconds. Lowercase `x` and Ctrl-X do nothing.

#### Rerun

`r` re-executes a *finished* task's command using the same command string and directory, under the environment of the client requesting the rerun (launch context always belongs to whoever asks for the launch). The task retains its ID, `◆` tag, group, and list position; its clock and screen reset. On a running task, `r` is a no-op because rerunning would first require a destructive kill. Rerun also works inside peek, which keeps the result visible while starting the next run.

#### Detach vs. quit

`q` (and `Ctrl-C`) disconnects the client and leaves the daemon and its jobs running; the next `fleetcom` reattaches. `Q` kills every job (`TERM` to each process group, `KILL` after a 2 s grace for any that ignore it) and stops the daemon. `Ctrl-C` is intercepted only in the dashboard; while attached it belongs to the child.

#### Grouping and tagging

`s` cycles three grouping modes: state, dir, custom. The header shows the strip `by state · dir · custom` with the active mode bold and the rest dim.

- By state: In use / Running / Completed.
- By dir: one section per working directory; the invocation directory first, the rest alphabetical.
- By custom group: one section per group name, sorted by name, with Unassigned last. Unassigned is the triage inbox — fresh spawns land there unless they inherit a group (below) — and the section exists only while an ungrouped task does.

Groups are daemon state on the task itself: an assignment survives client detach and rerun (`r`), and switching grouping modes never touches it. `g` reassigns the selected task through the [group picker](#the-g-group-picker).

`m` toggles the "in use" tag on the selected task; tagged tasks are marked `◆`, so the handful you're actively steering stay reachable as the list grows. In state mode they form the In use section at the top. In custom mode a tag floats the task to the top of its group rather than ejecting it into a global section: within a group, tagged tasks sort first, then running, then completed, and within each of those, tasks cluster by directory, then spawn order.

In custom mode — and only there — a new command inherits the selected task's group, through both `n` and the `@` picker. The spawn prompt shows the destination as `❯ dir ▸ group ▸ command`, each segment present only when it applies: the dir segment for a non-default directory, the group segment when a group will be inherited. State- and dir-mode spawns start unassigned.

## The `@` directory picker

`@` opens a bottom panel: a typed-path field, plus the directories that match it. There are three kinds of row, and `Enter` does the right thing for each:

- Current directory: run the command right here (`Enter`).
- Recent directories: ones you've launched in before; `Enter` runs there, `Tab`/`→` browses into them.
- Subdirectories of the current path: `Enter` or `Tab`/`→` descends into one.

Typing filters the rows; `Backspace` climbs the typed path; `↑`/`↓` move the highlight; `Esc` cancels. Completion updates on each input, permitting navigation and launch without leaving the dashboard.

## The `g` group picker

`g` on a selected task opens a bottom panel with the same structure as the `@` picker: a typed-name field plus the matching rows. Row 0 is always Unassigned, so the list is never empty; the fleet's existing group names follow, sorted, filtered by case-insensitive prefix as you type. The task's current group is marked `(current)`.

`Enter` acts on the highlighted row, and the hint line names the action:

- The Unassigned row: clear the task back to unassigned (`enter clear`).
- An existing group: assign it (`enter assign`).
- Typed text matching no existing group: create that group and assign it (`enter create`).

`↑`/`↓` move the highlight; `Esc` cancels without changing anything.

The daemon normalizes every group name it receives, whether from the picker or a [session](sessions.md) file: control characters are stripped, surrounding whitespace is trimmed, and the result is capped at 64 characters. A name that normalizes to nothing, or to the literal `Unassigned`, means "no group" — the reserved section name can never collide with a group of your own. Case is preserved and significant: `unassigned` is a legal group name.

## Peek

A centered box over the dashboard showing the selected task's live screen (the last screenful). `↑`/`↓` (or `k`/`j`) switch which task you're peeking at; `Enter` attaches to it; `r` reruns it if it has finished; `Space`, `Esc`, or `q` closes.

## Attached

The task owns the terminal, and its status bar reads `[attached] <command>    Ctrl-\ background`. `Ctrl-\` returns to the dashboard; every other key (control chords included) goes to the child.

## When the daemon drops

If the daemon connection is lost, the client clears the task list because it can no longer verify that state. `r` reconnects in daemon-backed mode; `q` quits. A `--foreground` core has no external process to reconnect to, so only quit is available.
