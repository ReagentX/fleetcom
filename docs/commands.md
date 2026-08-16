# Commands

`fleetcom` has two control surfaces. Launch arguments select the operating mode; keys control the dashboard, pickers, and attached PTY. `q`, `Q`, and `Ctrl-C` mean different things on each surface.

## Invocation

| Invocation | Effect |
| -- | -- |
| `fleetcom` | Connect to the daemon (autostarting it if needed) and open the dashboard |
| `fleetcom <session>` | Load a saved [session](sessions.md) at startup, then open the dashboard |
| `fleetcom --foreground` | Run the core in-process, no daemon; tasks die when you quit |
| `fleetcom --scrollback <lines>` | Set per-task scrollback depth (default 2,000, max 100,000); see [Scrollback](#scrollback) |
| `fleetcom --kill` | Stop the daemon and kill every task it owns; works even while another client is attached (it signals the daemon rather than queueing behind the socket) |
| `fleetcom --help` / `-h` | Print usage and exit |
| `fleetcom --version` / `-V` | Print the version and exit |

`--daemon` is internal. An ordinary invocation starts it when necessary. The first non-`-` argument is treated as the session name; a second is rejected.

## Dashboard

| Key | Command |
| -- | -- |
| `↑` `↓` / `k` `j` | Move the selection |
| `Tab` / `Shift-Tab` | Jump the selection to the next / previous section |
| `Enter` | Attach to the selected task |
| `Space` | Peek at the selected task |
| `n` | New command in the invocation directory |
| `@` | New command in a directory you pick |
| `/` | Jump the selection to a task by name, command, or group (opens the [find palette](#the--find-palette)) |
| `s` | Cycle grouping: by state / by directory / by custom group |
| `m` | Tag the selected task "in use" (toggles) |
| `M` | Select the next tagged task in dashboard order, wrapping at the end |
| `g` | Assign the selected task to a group (opens the group picker) |
| `R` | Rename the selected task: a display name shown in place of the command |
| `r` | Rerun a finished task; supported agent tasks use the captured resume command |
| `X` | Kill a running task (`TERM`, then `KILL` after 2 s), or remove a finished one |
| `w` | Save the current tasks as a session |
| `o` | Load a saved session or a recovery snapshot (opens the [session picker](#the-o-session-picker)) |
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

The `∙` glyph and the Idle section both apply after 10 seconds without output.

### Input and task lifecycle

#### Peek vs. attach

`Space` opens a read-only overlay containing the selected task's live screen. `↑`/`↓` move between tasks without closing the overlay; `Space`, `Esc`, or `q` closes it. `Enter`, from either the dashboard or peek, attaches to the task and forwards input to its PTY. Full-screen programs such as `vim` and `htop` retain their terminal state and cursor.

#### Attach and background

Attached mode gives the child control of terminal input. `Ctrl-\` returns to the dashboard; other supported keys, including `Ctrl-C`, `Ctrl-Z`, and `Ctrl-D`, are forwarded to the child.

Cursor-key encoding follows the child's live cursor-key mode. In application-cursor mode, unmodified cursor keys use `SS3` (`ESC O A` for Up); modified cursor keys use `CSI`. Function keys, modified navigation such as `Alt+Left`, and standard `Ctrl` combinations are also supported.

`Ctrl-\` names the physical chord. Crossterm may report the same chord as `Ctrl-4`, so `fleetcom` accepts both representations.

#### Modified keys, paste, and mouse input

These inputs cannot all be forwarded byte-for-byte. Their encoding and destination depend on the terminal and the attached child's modes.

Shift+Enter and Alt+Enter send `ESC CR` rather than plain `CR`. If the terminal does not report modified keys, `fleetcom` cannot distinguish Shift+Enter from Enter and forwards plain `CR`.

Paste travels as one message. For bracketed-paste-aware children, `fleetcom` adds paste markers and removes embedded terminators. Otherwise, it converts line endings to `CR`. Its own text fields strip control characters.

Mouse routing follows the child's reported modes. A mouse-aware child receives clicks, drags, releases, and wheel events in the negotiated encoding. For a full-screen child without mouse reporting, the terminal's alternate-scroll mode handles the wheel when enabled.

Selection depends on who owns mouse input. When `fleetcom` has capture, a left drag selects child-screen text on inline screens, in full-screen programs that disable alternate scroll, and in scrollback. While scrollback is visible, `fleetcom` keeps capture and never forwards mouse events to the child. Release copies the highlighted span to the system clipboard via OSC 52 and shows a `copied N chars` status-bar notice. Trailing padding is trimmed from each selected row, and concealed (SGR 8) cells copy as the blanks shown on screen.

Terminal-native selection remains available under capture through the terminal's override modifier, typically Shift. This selects across the terminal's entire view, including `fleetcom`'s chrome. On the dashboard, in peek, and in full-screen programs using alternate scroll, the terminal owns drag selection directly.

Attached children do not receive kitty keyboard-protocol or application-keypad sequences. Keypad digits send their normal characters.

#### Scrollback

Each task retains 2,000 lines of scrollback by default. `--scrollback <lines>` or `FLEETCOM_SCROLLBACK` overrides the depth, with the flag taking precedence. Values are capped at 100,000 lines; `0` disables scrollback, and an unparsable environment value falls back to the default instead of failing startup.

The supervisor reads this setting when it starts. It therefore applies to a `--foreground` run or a daemon started by the current invocation. An existing daemon keeps its configured depth until `fleetcom --kill`.

While attached to an inline child, wheel-up over its output enters scrollback. `Shift+PageUp` also enters it; `Ctrl+PageUp` and `Alt+PageUp` provide alternatives when the terminal intercepts Shift. The status bar shows the current offset as `[scroll ↑N]`.

Once open, the wheel scrolls by three lines. `PageUp` and `PageDown` move by pages, `↑` and `↓` move by one line, and `Home` jumps to the oldest retained row. A left drag selects the displayed history; release copies it using the same trimming and concealment rules as live-screen selection. Scrolling cancels an active drag.

`Esc`, `Enter`, `q`, `End`, or reaching the bottom returns to live output. Typing returns to live output and forwards the key to the child; `Ctrl-\` backgrounds the task as usual. Leaving scrollback cancels an active drag, while detaching or switching tasks resets the view.

#### Destroying tasks

Destroy is Shift-gated: only uppercase `X` acts. It kills a running task or removes a finished one. Removal also terminates remaining processes in the task's process group, escalating from `TERM` to `KILL` after two seconds. Lowercase `x` and `Ctrl-X` do nothing.

#### Rerunning tasks

`r` acts only on a finished task. A running task remains untouched because rerunning it would first require a destructive kill.

The replacement starts in the same directory using the requesting client's environment. Most tasks reuse their stored command. A supported `claude`, `codex`, `grok`, or `omp` task instead uses its captured resume command when a valid conversation ID is available.

Rerunning preserves the task's ID, `◆` tag, group, name, and spawn order; its clock and screen reset. Since lifecycle affects sorting, the task may move to another section when it starts. The same key works inside peek, which remains open while the replacement starts.

#### Disconnecting and quitting

Input meaning depends on the active surface. From the dashboard, `q` or `Ctrl-C` disconnects the client. A daemon and its tasks continue running, so the next `fleetcom` invocation reconnects. Under `--foreground`, the in-process core exits with the client and its tasks die. While attached, `Ctrl-C` belongs to the child. In prompts and pickers, `Esc` cancels without disconnecting.

Uppercase `Q` stops the daemon and terminates each task's process group. Shutdown sends `TERM` first, then `KILL` after a two-second grace period. Processes that have moved into another group are outside this sweep.

A `TERM`-ignoring member can also survive when its leader exits during shutdown. The group-emptiness check then releases the process-group ID reservation before escalation; [Shutdown is graceful-first](README.md#shutdown-is-graceful-first) explains the tradeoff.

### Task organization

#### Grouping and tagging

`s` cycles three grouping modes: state, dir, custom. The header shows the strip `by state · dir · custom` with the active mode bold and the rest dim.

- By state: In use / Running / Idle / Completed. A running task files under Idle after 10 s without output; Completed stays one section (`✓`/`✗` show exit status).
- By dir: one section per working directory; the invocation directory first, then the remaining labels sorted without regard to case.
- By custom group: one section per group name, sorted without regard to case, with Unassigned last. Fresh spawns remain unassigned unless they inherit a group, and the Unassigned section exists only while it has a member.

Dashboard section labels and within-section directory tiebreaks use the same case-insensitive order. Names that differ only by case sort next to each other in a deterministic order. Group identity remains case-sensitive, so `API` and `api` stay separate sections.

Groups belong to task state: an assignment survives client detach and rerun (`r`), and switching grouping modes does not modify it. `g` reassigns the selected task through the [group picker](#the-g-group-picker).

`m` toggles the "in use" tag and marks the task with `◆`. In state mode, tagged tasks form the In use section at the top. In custom mode, a tag moves the task to the top of its existing group rather than creating a global section. Within a dir or custom section, tasks sort as tagged, live, then completed; each class then sorts by directory and spawn order. Idle state does not affect row order in these modes, so a quiet task keeps its position and shows `∙`. State mode instead moves quiet tasks from Running to Idle.

`M` cycles the selection through tagged tasks in dashboard order. It wraps after the last tagged task. With no tagged tasks, the selection does not move; with one, the selection moves to that task and stays there.

In custom mode only, a new command inherits the selected task's group, through both `n` and the `@` picker. The spawn prompt shows the destination as `❯ dir ▸ group ▸ command`, each segment present only when it applies: the dir segment for a non-default directory, the group segment when a group will be inherited. State- and dir-mode spawns start unassigned.

#### Renaming

`R` opens a rename prompt containing the selected task's current name. `Enter` saves; an empty field clears the name and restores the command as the display label; `Esc` cancels. Plain `r` remains rerun.

A named task shows its name in place of the command in the dashboard row and the peek title. The attached status bar shows `name · command`.

The daemon removes control characters, trims surrounding whitespace, and limits display names to 64 characters. An empty result clears the name. Unlike groups, `Unassigned` is a legal display name; only the [group picker](#the-g-group-picker) reserves that label.

## The `@` directory picker

`@` opens a bottom panel containing a path field and its matching directories. `Enter` depends on the selected row type:

- Resolved path: run the command in that directory (`Enter`). Row 0 is always this row, so the list is never empty.
- Current task directories: `Enter` runs there; `Tab`/`→` browses into them. These rows precede subdirectories.
- Subdirectories of the resolved path: `Enter` or `Tab`/`→` descends into one.

Typing filters both lists under different rules. A subdirectory matches the fragment as a case-insensitive prefix. A current task directory matches a case-insensitive substring of its final path component: `log` finds `~/Documents/Code/Rust/Logria`, while `crab` finds both `crabapple` and `crabstep`. Parent components do not participate, so `doc` does not match every directory under `~/Documents/`.

Once the field contains `/`, current task directory rows are omitted; the picker shows the resolved path and its matching subdirectories. Without `/`, a current task directory that is also a matching subdirectory appears once, with the current task row behavior.

`Backspace` deletes one character and the matches re-filter; `↑`/`↓` move the highlight; `Esc` cancels. Completion updates on each input, permitting navigation and launch without leaving the dashboard. `←`/`→` move the caret within the typed path (`→` descends only when the caret is at the end), and `Ctrl-A`/`Ctrl-E` (or `Home`/`End`) jump to either end; the same caret keys work in every `fleetcom` text field.

## The `/` find palette

`/` opens a bottom panel listing tasks that match the query. Each row reads `<glyph> <label> · <section>`: the status glyph, display name (or command when unnamed), and current section. Empty input lists the whole fleet. Results follow dashboard order. With no tasks, `/` does nothing.

Matching checks case-insensitive substrings in the name, command, and group. For example, `eep` finds `sleep 5`. A display name adds a searchable field without replacing the command, so a task named `api tests` can still match `cargo`.

The working directory is not a match field.

`Enter` moves the dashboard selection to the highlighted task and closes the panel; it does not attach. Press `Enter` again from the dashboard to attach, or `Space` to peek. With no matches, `Enter` leaves the panel open. `↑`/`↓` move the highlight. `Esc` closes the panel without changing the selection.

## The `g` group picker

`g` on a selected task opens a bottom panel with a typed-name field and matching rows. Row 0 is always Unassigned, so the list is never empty. Existing group names follow in case-insensitive order and are filtered by case-insensitive prefix. The task's current group is marked `(current)`.

`Enter` acts on the highlighted row, and the hint line names the action:

- The Unassigned row: clear the task back to unassigned (`enter clear`).
- An existing group: assign it (`enter assign`).
- Typed text matching no existing group: create that group and assign it (`enter create`).

`↑`/`↓` move the highlight; `Esc` cancels without changing anything.

The daemon normalizes every group name received from the picker or a [session](sessions.md) file. It removes control characters, trims surrounding whitespace, and caps the result at 64 characters. An empty result or the exact name `Unassigned` means no group, preventing a user-defined name from colliding with the reserved section. Comparison remains case-sensitive, so `unassigned` is a valid group name.

## The `o` session picker

`o` opens a bottom panel listing the saved [sessions](sessions.md), sorted by name ignoring case: `↑`/`↓` move the highlight, `Enter` loads, `Esc` cancels. While [recovery snapshots](sessions.md#recovery) exist, the hint adds `tab recovery (N)` and `Tab` (or `Shift-Tab`) flips the panel to them; `Tab` again returns to the saved list. Each list keeps its own highlight. With no snapshots, `Tab` does nothing and the hint omits it.

A recovery row reads `<age> ago · <tasks> task(s) · <label>`: the file's age, its command count, and its stored label (normally `autosaved <timestamp>`). `Enter` loads the highlighted snapshot; the status line confirms the load and suggests saving it. Press `w` to save the recovered fleet as a named session.

## The `?` controls overlay

The dashboard's two hint rows cover common actions: `↑↓ select · enter attach · space peek · ? controls` and `❯ n run · @ dir · / find · s sort`. `?` opens an expanded reference for dashboard actions and the attached-mode background chord.

The overlay groups bindings by purpose in a centered box. It does not scroll. `?`, `Esc`, or `q` returns to the dashboard. Other keys do nothing in the overlay; `Ctrl-C` still disconnects.

`?` is Shift-`/`. `fleetcom` accepts both event forms for this binding: `?`, or `/` with Shift. An unmodified `/` still opens the [find palette](#the--find-palette).

The box uses a two-column layout. When height is limited, group headers drop first; if the entries still do not fit, the overlay clips the tail and reports `+N more` on the bottom border. Narrow terminals clip each row to the box width.

## Peek

A centered box over the dashboard showing the selected task's live screen (the last screenful). `↑`/`↓` (or `k`/`j`) switch which task you're peeking at; `Enter` attaches to it; `r` reruns it if it has finished; `Space`, `Esc`, or `q` closes.

The footer's `preview:` segment names the source of the row's dashboard preview: `floor` (the last non-blank row of the live screen), `marker` (a full-screen program with no usable title), `title` (the child's window title; a full-screen program's title renders verbatim, while on the primary screen only a title the task's agent adapter recognizes renders — a refusal falls to `floor`), or `anchor/<rule>` (a recognized agent status, tagged with the matcher that produced it). Most rules name a screen matcher, such as `claude:spinner` or `codex:approval-menu`. The `claude:registry-approval` and `claude:registry-waiting` rules instead come from Claude's on-disk session status.

## Attached

The task owns the terminal, and its status bar reads `[attached] <command>    Ctrl-\ background`, or `[attached] <name> · <command>` for a named task. `Ctrl-\` returns to the dashboard. Other supported input normally goes to the child; [scrollback](#scrollback) reserves its navigation keys.

## Connection loss

If the daemon connection closes, the client clears the task list because it can no longer verify the snapshot. In daemon-backed mode, `r` reconnects and `q` quits. A `--foreground` core has no external process to reconnect to, so only quit remains available.
