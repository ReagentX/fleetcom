# Commands

`fleetcom` has two control surfaces. Launch arguments select the operating mode; keys control the dashboard, pickers, and attached PTY. `q`, `Q`, and `Ctrl-C` mean different things on each surface.

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
| `R` | Rename the selected task: a display name shown in place of the command |
| `r` | Rerun a finished task; supported agent tasks use the captured resume command |
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

The `∙` glyph flips after ≈600 ms of quiet; the Idle *section* in the by-state sort uses a 10 s window. A task can therefore show `∙` while still filed under Running.

### Input and lifecycle mechanics

#### Peek vs. attach

`Space` opens a read-only overlay containing the selected task's live screen. `↑`/`↓` move between tasks without closing the overlay; `Space`, `Esc`, or `q` closes it. `Enter`, from either the dashboard or peek, attaches to the task and forwards input to its PTY. Full-screen programs such as `vim` and `htop` retain their terminal state and cursor.

#### Attach and background

While attached, `Ctrl-\` returns to the dashboard. Every other key, including `Ctrl-C`, `Ctrl-Z`, and `Ctrl-D`, is forwarded to the child. `Ctrl-\` refers to the physical chord; the input handler accepts both `Ctrl-\` and crossterm's `Ctrl-4` representation of that chord.

#### Shift+Enter, paste, and the wheel

Modified Enter, paste, and mouse input require state-dependent encoding:

- Shift+Enter and Alt+Enter are sent as `ESC CR`, which is distinct from plain Enter. Shift requires a terminal that reports modified keys; terminals that do not report it send plain `CR`.
- Paste travels as one message. Bracketed-paste-aware children receive paste markers with embedded terminators removed; other children receive line endings as `CR`. `fleetcom` text fields strip control characters.
- Mouse-protocol children receive clicks, drags, releases, and wheel events in the negotiated encoding. Full-screen children without a mouse protocol use alternate scroll. For inline children without a mouse protocol, wheel-up enters `fleetcom`'s scrollback view. When `fleetcom` captures the mouse (for mouse-protocol children, inline children, or scrollback), terminal selection requires the terminal's selection-override modifier. Otherwise, drag selects normally.

#### Scrollback

Tasks retain 2,000 lines of scrollback. While attached to an inline child, wheel-up over its output enters scrollback; `Shift+PageUp` also enters it (`Ctrl+PageUp` and `Alt+PageUp` work when Shift is intercepted). The status bar shows `[scroll ↑N]`. The wheel scrolls, `PageUp`/`PageDown` move by pages, `↑`/`↓` by lines, and `Home` jumps to the oldest row. `Esc`, `Enter`, `q`, `End`, or reaching the bottom returns to live output. Typing also returns to live and forwards the key. Detaching or switching tasks resets the view.

#### Destroy is Shift-gated

`X` kills a running task or removes a finished one. Removal also terminates remaining processes in the task's process group, escalating from `TERM` to `KILL` after two seconds. Lowercase `x` and Ctrl-X do nothing.

#### Rerun

`r` re-executes a *finished* task in the same directory, using the environment of the client that requested the rerun. Most tasks reuse their stored command. A supported `claude`, `codex`, or `grok` task instead uses its captured resume command when a valid conversation ID is available. The task retains its ID, `◆` tag, group, name, and spawn order; its clock and screen reset. Because lifecycle participates in sorting, the task can move to another section when it starts running again. A running task is left untouched because rerunning it would require a destructive kill first. The same key works inside peek, keeping the task visible while the replacement starts.

#### Detach vs. quit

From the dashboard, `q` or `Ctrl-C` disconnects the client and leaves the daemon and its jobs running; the next `fleetcom` reattaches. `Q` stops the daemon after sending `TERM` to each job's process group, then `KILL` after a 2 s grace. Processes that have moved to another group are outside this sweep. A `TERM`-ignoring member can also survive if its leader exits during shutdown, because checking group emptiness releases the process-group ID reservation before escalation (see [Shutdown is graceful-first](README.md#shutdown-is-graceful-first)). Outside attached mode, `Ctrl-C` disconnects the client; while attached, it belongs to the child. In prompts and pickers, `Esc` cancels without disconnecting.

#### Grouping and tagging

`s` cycles three grouping modes: state, dir, custom. The header shows the strip `by state · dir · custom` with the active mode bold and the rest dim.

- By state: In use / Running / Idle / Completed. A running task files under Idle after 10 s without output; Completed stays one section (`✓`/`✗` show exit status).
- By dir: one section per working directory; the invocation directory first, the rest alphabetical.
- By custom group: one section per group name, sorted by name, with Unassigned last. Fresh spawns remain unassigned unless they inherit a group, and the Unassigned section exists only while it has a member.

Groups belong to task state: an assignment survives client detach and rerun (`r`), and switching grouping modes does not modify it. `g` reassigns the selected task through the [group picker](#the-g-group-picker).

`m` toggles the "in use" tag and marks the task with `◆`. In state mode, tagged tasks form the In use section at the top. In custom mode, a tag moves the task to the top of its existing group rather than creating a global section. Within each group, the order is tagged, running, idle, completed; each bucket then sorts by directory and spawn order.

In custom mode only, a new command inherits the selected task's group, through both `n` and the `@` picker. The spawn prompt shows the destination as `❯ dir ▸ group ▸ command`, each segment present only when it applies: the dir segment for a non-default directory, the group segment when a group will be inherited. State- and dir-mode spawns start unassigned.

#### Renaming

`R` opens a rename prompt containing the selected task's current name. `Enter` saves; an empty field clears the name and restores the command as the display label; `Esc` cancels. Plain `r` remains rerun.

A named task shows its name in place of the command in the dashboard row and the peek title. The attached status bar shows `name · command`.

The daemon removes control characters, trims surrounding whitespace, and limits display names to 64 characters. An empty result clears the name. Unlike groups, `Unassigned` is a legal display name; only the [group picker](#the-g-group-picker) reserves that label.

## The `@` directory picker

`@` opens a bottom panel containing a path field and its matching directories. `Enter` depends on the selected row type:

- Current directory: run the command in that directory (`Enter`).
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

The daemon normalizes every group name received from the picker or a [session](sessions.md) file. It removes control characters, trims surrounding whitespace, and caps the result at 64 characters. An empty result or the exact name `Unassigned` means no group, preventing a user-defined name from colliding with the reserved section. Comparison remains case-sensitive, so `unassigned` is a valid group name.

## Peek

A centered box over the dashboard showing the selected task's live screen (the last screenful). `↑`/`↓` (or `k`/`j`) switch which task you're peeking at; `Enter` attaches to it; `r` reruns it if it has finished; `Space`, `Esc`, or `q` closes.

## Attached

The task owns the terminal, and its status bar reads `[attached] <command>    Ctrl-\ background`, or `[attached] <name> · <command>` for a named task. `Ctrl-\` returns to the dashboard; every other key (control chords included) goes to the child.

## Connection loss

If the daemon connection closes, the client clears the task list because it can no longer verify the snapshot. In daemon-backed mode, `r` reconnects and `q` quits. A `--foreground` core has no external process to reconnect to, so only quit remains available.
