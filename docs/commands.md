# Commands

Choose the operating mode with launch arguments. Use keys to control the dashboard, pickers, and attached PTY. `q`, `Q`, and `Ctrl-C` mean different things on each surface.

## Invocation

| Invocation | Effect |
| -- | -- |
| `fleetcom` | Connect to the daemon (autostarting it if needed) and open the dashboard |
| `fleetcom <session>` | Load a saved [session](sessions.md) at startup, then open the dashboard |
| `fleetcom --foreground` | Run the core in-process, no daemon; tasks are terminated when you quit |
| `fleetcom --scrollback <lines>` | Set per-task scrollback depth (default 2,000, max 100,000); see [Scrollback](#scrollback) |
| `fleetcom --kill` | Stop the daemon and kill every task it owns; available even while another client is attached (shutdown is requested by signal, without waiting for the socket) |
| `fleetcom --help` / `-h` | Print usage and exit |
| `fleetcom --version` / `-V` | Print the version and exit |

`--daemon` is internal. The daemon is started automatically when needed. The first non-`-` argument is treated as the session name; a second is rejected.

## Dashboard

| Key | Command |
| -- | -- |
| `↑` `↓` / `k` `j` | Move the selection |
| `Tab` / `Shift-Tab` | Jump the selection to the next / previous section |
| `Enter` | Attach to the selected task |
| `Space` | Peek at the selected task |
| `n` | New command in the invocation directory |
| `@` | New command in a directory you pick |
| `/` | Jump the selection to a task by name, command, or group (through the [find palette](#the--find-palette)) |
| `s` | Cycle grouping: by state / by directory / by custom group |
| `m` | Tag the selected task "in use" (toggles) |
| `M` | Select the next tagged task in dashboard order, wrapping at the end |
| `g` | Assign the selected task to a group (through the group picker) |
| `R` | Rename the selected task: a display name shown in place of the command |
| `r` | Rerun a finished task; use the captured resume command for supported agent tasks |
| `X` | Kill a running task (`TERM`, then `KILL` after 2 s), or remove a finished one |
| `w` | Save the current tasks as a session |
| `o` | Load a saved session or a recovery snapshot (through the [session picker](#the-o-session-picker)) |
| `?` | Open the [controls overlay](#the--controls-overlay) |
| `q` (or `Ctrl-C`) | Disconnect from the daemon; under `--foreground`, quit and stop the tasks |
| `Q` | Quit; kill the tasks and stop the daemon |

![`fleetcom` controls overlay](img/controls.png)

### Status glyphs

| Glyph | Meaning |
| -- | -- |
| `✻` | Running: active, producing output |
| `∙` | Idle: running, but quiet |
| `✓` | Completed, exit 0 |
| `✗` | Completed, non-zero exit |
| `◆` | Tagged "in use" |

After 10 seconds without output, a task is marked `∙` and grouped under Idle.

### Input and task lifecycle

#### Peek vs. attach

Press `Space` to peek at the selected task's live screen in a read-only overlay. Use `↑`/`↓` to move between tasks; press `Space`, `Esc`, or `q` to close. Press `Enter` from the dashboard or peek to attach and send input to the task's PTY. Terminal state and cursor position are preserved for full-screen programs such as `vim` and `htop`.

#### Attach and background

While attached, send terminal input to the child. Press `Ctrl-\` to return to the dashboard; other supported keys, including `Ctrl-C`, `Ctrl-Z`, and `Ctrl-D`, are forwarded to the child.

Cursor keys are encoded according to the child's live cursor-key mode. In application-cursor mode, unmodified cursor keys are encoded with `SS3` (`ESC O A` for Up); modified cursor keys are encoded with `CSI`. Function keys, modified navigation such as `Alt+Left`, and standard `Ctrl` combinations are also supported.

`Ctrl-\` is the physical chord. The same chord may be reported by Crossterm as `Ctrl-4`; both representations are accepted.

#### Modified keys, paste, and mouse input

These inputs cannot all be forwarded byte-for-byte. Their encoding and destination depend on the terminal and the attached child's modes.

Press Shift+Enter or Alt+Enter to send `ESC CR` rather than plain `CR`. Without terminal support for reporting modified keys, Shift+Enter cannot be distinguished from Enter and is forwarded as plain `CR`.

Pasted text is sent as one message. For bracketed-paste-aware children, paste markers are added and embedded terminators removed. Otherwise, line endings are converted to `CR`. Control characters are stripped before insertion into `fleetcom` text fields.

Mouse events are routed according to the child's reported modes. Clicks, drags, releases, and wheel events are forwarded in the negotiated encoding when mouse reporting is enabled. For a full-screen child without mouse reporting, use the wheel through alternate-scroll mode when enabled.

Selection depends on who owns mouse input. With `fleetcom` mouse capture, drag with the left button to select child-screen text on inline screens, in full-screen programs that disable alternate scroll, and in scrollback. While scrollback is visible, mouse capture is retained and mouse events are never forwarded to the child. Release to copy the highlighted span to the system clipboard via OSC 52. The status-bar notice is `copied N chars`. Trailing padding is trimmed from each selected row, and concealed (SGR 8) cells are copied as the blanks shown on screen.

Terminal-native selection remains available under capture through the terminal's override modifier, typically Shift. Use this modifier to select across the terminal's entire view, including `fleetcom`'s chrome. On the dashboard, in peek, and in full-screen programs using alternate scroll, drag selection is handled directly by the terminal.

Attached children do not receive kitty keyboard-protocol or application-keypad sequences. Keypad digits are sent as their normal characters.

#### Scrollback

By default, 2,000 lines of scrollback are retained per task. Set `--scrollback <lines>` or `FLEETCOM_SCROLLBACK` to override the depth; prefer the flag when both are set. Values are capped at 100,000 lines. Set `0` to disable scrollback. An unparsable environment value is treated as the default without failing startup.

The depth is configured at supervisor startup: for a `--foreground` run or a daemon started by the current invocation. To change the depth of an existing daemon, stop it with `fleetcom --kill` first.

While attached to an inline child, scroll up over its output or press `Shift+PageUp` to enter scrollback. Use `Ctrl+PageUp` or `Alt+PageUp` if Shift is intercepted by the terminal. Read the current offset in the status bar as `[scroll ↑N]`.

In scrollback, use the wheel to scroll by three lines, `PageUp`/`PageDown` by pages, `↑`/`↓` by one line, or `Home` to reach the oldest retained row. Drag with the left button to select displayed history; release to copy it using the same trimming and concealment rules as live-screen selection. An active drag is canceled on scroll.

Press `Esc`, `Enter`, `q`, or `End`, or scroll to the bottom, to return to live output. Type to return to live output and send the key to the child; press `Ctrl-\` to background the task as usual. An active drag is canceled on leaving scrollback. The view is reset on detach or task switch.

#### Destroying tasks

Press uppercase `X` to kill a running task or remove a finished one. On removal, remaining processes in the task's process group are also terminated: `TERM`, then `KILL` after two seconds. Lowercase `x` and `Ctrl-X` are unbound.

#### Rerunning tasks

Press `r` to rerun a finished task. Running tasks cannot be rerun without first being killed and are left untouched.

`fleetcom` starts the replacement in the same directory, using the requesting client's environment and the stored command. For a supported `claude`, `codex`, `grok`, or `omp` task with a valid captured conversation ID, it uses the resume command instead.

On rerun, the task's ID, `◆` tag, group, name, and spawn order are preserved; its clock and screen are reset. Under lifecycle sorting, the restarted task may be listed in another section. You can also rerun inside peek without closing the overlay.

#### Disconnecting and quitting

Input meaning depends on the active surface. From the dashboard, press `q` or `Ctrl-C` to disconnect. A daemon and its tasks continue running, so you can reconnect with `fleetcom`. Under `--foreground`, the in-process core exits with the client and its tasks are terminated. While attached, `Ctrl-C` belongs to the child. In prompts and pickers, press `Esc` to cancel without disconnecting.

Press uppercase `Q` to stop the daemon and terminate each task's process group: `TERM` first, then `KILL` after one shared two-second grace period. Exited leaders remain unreaped until escalation so `TERM`-ignoring descendants in their groups still receive `KILL`. For a nonempty fleet, shutdown is delayed by the grace period even when all listed tasks have finished or exited on `TERM`; with no tasks left to clean up, shutdown is immediate. Original escalation timers are retained for tasks already terminating. Processes that have moved into another group or session are outside this sweep.

### Task organization

#### Grouping and tagging

Press `s` to cycle through state, dir, and custom grouping. In the header strip `by state · dir · custom`, the active mode is bold and the rest dim.

- By state: In use / Running / Idle / Completed. After 10 s without output, a running task is grouped under Idle. Completed tasks are grouped in one section, with exit status marked `✓`/`✗`.
- By dir: one section per working directory; the invocation directory first, then the remaining labels sorted without regard to case.
- By custom group: one section per group name, sorted without regard to case, with Unassigned last. Fresh spawns remain unassigned unless they inherit a group, and the Unassigned section exists only while it has a member.

Dashboard section labels and within-section directory tiebreaks use the same case-insensitive order. Names that differ only by case are sorted next to each other in a deterministic order. Group identity remains case-sensitive, so `API` and `api` stay separate sections.

Groups belong to task state: an assignment survives client detach and rerun (`r`), and switching grouping modes does not modify it. Press `g` to reassign the selected task through the [group picker](#the-g-group-picker).

Press `m` to toggle the "in use" tag, marked `◆`. In state mode, tagged tasks form the In use section at the top. In custom mode, tagged tasks are placed at the top of their existing groups rather than in a global section. Within a dir or custom section, tasks are sorted as tagged, live, then completed; each class is then sorted by directory and spawn order. Idle state does not affect row order in these modes, so quiet tasks are kept in place and marked `∙`. In state mode, quiet tasks are moved from Running to Idle.

Press `M` to select the next tagged task in dashboard order, wrapping after the last. With no tagged tasks, the selection is unchanged; with one, that task is selected.

In custom mode only, a new command is assigned the selected task's group, through both `n` and the `@` picker. The destination is shown in the spawn prompt as `❯ dir ▸ group ▸ command`, each segment present only when it applies: the dir segment for a non-default directory, the group segment when a group will be inherited. New tasks are unassigned in state and dir modes.

#### Renaming

Press `R` to rename the selected task, starting with its current name. Press `Enter` to save, or `Esc` to cancel. Leave the field empty to clear the name and display the command again. Use lowercase `r` to rerun.

For a named task, the name is displayed in place of the command in the dashboard row and peek title. In the attached status bar, both are displayed as `name · command`.

The daemon removes control characters and surrounding whitespace from display names, then limits them to 64 characters. An empty result means no name. Unlike groups, `Unassigned` is a legal display name; the label is reserved only in the [group picker](#the-g-group-picker).

## The `@` directory picker

Press `@` to open the directory picker: a bottom panel with a path field and matching directories. Use `Enter` according to the selected row type:

- Resolved path: run the command in that directory (`Enter`). Row 0 is always this row, so the list is never empty.
- Current task directories: press `Enter` to run there, or `Tab`/`→` to browse into them. These rows precede subdirectories.
- Subdirectories of the resolved path: press `Enter` or `Tab`/`→` to descend into one.

Typing filters both lists under different rules. A subdirectory matches the fragment as a case-insensitive prefix. A current task directory matches a case-insensitive substring of its final path component: type `log` to find `~/Documents/Code/Rust/Logria`, or `crab` to find both `crabapple` and `crabstep`. Parent components do not participate, so `doc` does not match every directory under `~/Documents/`.

Once the field contains `/`, current task directory rows are omitted; only the resolved path and matching subdirectories are shown. Without `/`, a current task directory that is also a matching subdirectory appears once, with the current task row behavior.

Press `Backspace` to delete one character, `↑`/`↓` to move the highlight, or `Esc` to cancel. Matches are refreshed on each input. Use `←`/`→` to move the caret within the typed path (`→` to descend only at the end), and `Ctrl-A`/`Ctrl-E` (or `Home`/`End`) to jump to either end. Use the same caret keys in every `fleetcom` text field.

## The `/` find palette

Press `/` to find tasks in a bottom panel. Each row is formatted as `<glyph> <label> · <section>`: the status glyph, display name (or command when unnamed), and current section. Leave the query empty to list the whole fleet in dashboard order. The palette is unavailable with no tasks.

Matching checks case-insensitive substrings in the name, command, and group. For example, type `eep` to find `sleep 5`. A display name adds a searchable field without replacing the command, so a task named `api tests` can still match `cargo`.

The working directory is not a match field.

Press `Enter` to select the highlighted task on the dashboard and close the panel without attaching. Press `Enter` again from the dashboard to attach, or `Space` to peek. With no matches, the panel is left open on `Enter`. Use `↑`/`↓` to move the highlight, or `Esc` to close without changing the selection.

## The `g` group picker

Select a task and press `g` to open a bottom panel with a typed-name field and matching rows. Row 0 is always Unassigned, so the list is never empty. Existing group names follow in case-insensitive order and are filtered by case-insensitive prefix. The task's current group is marked `(current)`.

Press `Enter` to apply the action listed in the hint for the highlighted row:

- The Unassigned row: clear the task back to unassigned (`enter clear`).
- An existing group: assign it (`enter assign`).
- Typed text matching no existing group: create that group and assign it (`enter create`).

Use `↑`/`↓` to move the highlight; press `Esc` to cancel without changing anything.

The daemon normalizes group names from the picker or a [session](sessions.md) file: it removes control characters and surrounding whitespace, then caps the result at 64 characters. An empty result or the exact name `Unassigned` means no group, preventing a user-defined name from colliding with the reserved section. Comparison remains case-sensitive, so `unassigned` is a valid group name.

## The `o` session picker

Press `o` to list saved [sessions](sessions.md), sorted by name ignoring case. Use `↑`/`↓` to move the highlight, `Enter` to load, or `Esc` to cancel. When [recovery snapshots](sessions.md#recovery) are available, press `Tab` (or `Shift-Tab`) to switch to them, then `Tab` again to return to saved sessions. A separate highlight is retained for each list. With no snapshots, `Tab` is unbound and omitted from the hint.

A recovery row is formatted as `<age> ago · <tasks> task(s) · <label>`: the file's age, its command count, and its stored label (normally `autosaved <timestamp>`). Press `Enter` to load the highlighted snapshot. A load confirmation and reminder to save are displayed in the status line. Press `w` to save the recovered fleet as a named session.

## The `?` controls overlay

Use the two dashboard hints for common actions: `↑↓ select · enter attach · space peek · ? controls` and `❯ n run · @ dir · / find · s sort`. Press `?` for the expanded reference, including dashboard actions and the attached-mode background chord.

Bindings are grouped by purpose in a centered, non-scrolling box. Press `?`, `Esc`, or `q` to return to the dashboard, or `Ctrl-C` to disconnect. Other keys are ignored.

`?` is Shift-`/`. Both event forms are accepted for this binding: `?`, or `/` with Shift. Press unmodified `/` for the [find palette](#the--find-palette).

Bindings are arranged in two columns. At limited height, group headers are omitted first, then excess entries are clipped and counted as `+N more` on the bottom border. At narrow widths, each row is clipped to the box width.

## Peek

Peek shows the selected task's live screen (the last screenful) in a centered box over the dashboard. Use `↑`/`↓` (or `k`/`j`) to switch tasks, `Enter` to attach, `r` to rerun a finished task, or `Space`, `Esc`, or `q` to close.

Read the source of the row's dashboard preview in the footer's `preview:` segment: `floor` (the last non-blank row of the live screen), `marker` (a full-screen program with no usable title), `title` (the child's window title), or `anchor/<rule>` (a recognized agent status, tagged with the matcher that produced it). On the alternate screen, any current title is rendered; recognized title formats are normalized first. On the primary screen, only a retained title recognized by the task's agent adapter is rendered. Otherwise, `floor` is used. Most rules name a screen matcher, such as `claude:spinner` or `codex:approval-menu`. The `claude:registry-approval` and `claude:registry-waiting` rules instead come from Claude's on-disk session status.

## Attached

Interact with the task in the terminal. The status bar is formatted as `[attached] <command>    Ctrl-\ background`, or `[attached] <name> · <command>` for a named task. Press `Ctrl-\` to return to the dashboard. Other supported input normally goes to the child; navigation keys are reserved in [scrollback](#scrollback).

## Connection loss

On connection loss, the task list is cleared because the snapshot can no longer be verified. In daemon-backed mode, press `r` to reconnect or `q` to quit. A `--foreground` core has no external process to reconnect to, so only quit remains available.
