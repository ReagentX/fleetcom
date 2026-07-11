# Phase 2 — Persistence & Reattach

Design doc / resume anchor. Written before starting implementation; decisions
below are settled. Next action: **milestone 1**.

## Status (as of writing)

Phases 1 and 1.5 are done and committed on `master`:

- `c36c662` — Phase 1: fleet-view PTY supervisor (dashboard, peek, attach,
  grouping, `@` dir picker, tag, group-kill teardown, signal handling).
- `a86bc66` — Sessions: save/load a `{dir: [commands]}` recipe (jzon).
- `f3fb685` — resolve: lexical `.`/`..` collapse.

14 unit tests + a suite of PTY-driven Python harnesses (in the session
scratchpad, not committed), all green. Deps: crossterm, dirs, jzon, nix,
portable-pty, signal-hook, vt100. Unix-only (`killpg`).

### Current architecture phase 2 builds on
- `src/task.rs` — `Task`: one PTY (portable-pty) + child + reader thread →
  `Arc<Mutex<vt100::Parser>>`. `terminate()` group-kills (non-blocking, gated on
  `finished.is_none()`), `Drop` guard. **Standalone; moves into the daemon whole.**
- `src/app.rs` — `App`: UI state (modes, `selected_id`, `focused_id`, pickers,
  `status`, grouping) + the single-threaded event loop. Selection **and** focus
  are already by task **id**, not index.
- `src/ui.rs` — hand-rolled crossterm, atomic repaint-on-change (zero bytes when
  unchanged). Renders from `App`.
- `src/session.rs` — recipe file format; independent of who executes it.
- `src/format.rs` — rel-time + truncation helpers.

## The inversion that defines phase 2

Today `q` quits **and kills every job**. Phase 2 splits that into two explicit,
user-chosen actions:

- **Disconnect** (detach) — exit the client; daemon + jobs keep running.
- **Quit** (kill) — group-kill all tasks, stop the daemon, exit.

For jobs to outlive the UI, something other than the UI must own them → a
**daemon** owns the processes; the TUI becomes a **client**.

## Settled decisions

- **Single client at a time** for v1 (no tmux-style mirroring yet).
- **Manual kill only — no idle timeout.** The user explicitly chooses disconnect
  vs. quit. Rationale: an idle timeout would reap persistent-but-quiet processes
  — notably long-running **AI-agent CLI apps** (e.g. an agent sitting idle
  awaiting input but which must stay alive). Daemon persists until explicitly
  killed.
- **One daemon per user** (flat task set) for v1. *(v3: allow arbitrary named
  daemons + a minimal session-browser UI. Keep the word "session" for the recipe
  files; use a different word for live daemon groupings.)*

## Topology

```
   ┌── multi (client / TUI) ──┐        ┌───── multi --daemon ─────┐
   │ App: selection, modes,   │  UDS   │ owns PTY masters + child │
   │ rendering (ui.rs)        │◄──────►│ processes + vt100 parsers│
   │ holds TaskView snapshots │ socket │ thread-per-task readers  │
   └──────────────────────────┘        │ lifecycle, group-kill    │
                                        └──────────────────────────┘
```

- **Daemon owns the authoritative `vt100::Parser` per task.** It must — it's fed
  by the live PTY and answers "current screen" on reattach even with no client
  attached. This is exactly what a recipe file cannot do, and why the daemon
  earns its place.
- **Client is thin**: renders `TaskView`s (id, command, cwd, status, preview,
  tag) and blits screen data for the attached pane. No PTYs/children client-side.

## What moves, what stays (not a rewrite)

- `task.rs` → **daemon**, near-verbatim (already a standalone unit).
- `app.rs` / `ui.rs` → **client**, with one substitution: `Vec<Task>` becomes
  `Vec<TaskView>` synced from the daemon. Rendering barely changes.
- **Id-based selection/focus already done** → the client references tasks by
  stable id, which is exactly what survives crossing a socket.
- `session.rs` unchanged → load/save become daemon commands (daemon
  spawns/enumerates).

## Wire protocol — Unix domain socket, stdlib

- `std::os::unix::net::{UnixListener, UnixStream}` — **no dependency**. Socket at
  `$XDG_RUNTIME_DIR/multi/default.sock` (fallback `$TMPDIR/multi-$UID/…`),
  per-user.
- **Framing:** `[u32 len][u8 kind][payload]`, `read_exact` for partial reads.
  `kind` splits **control** frames (jzon — low-frequency, debuggable, reuses the
  existing dep) from **pane-data** frames (raw bytes — high-frequency screen
  diffs, no base64 tax).
- **Client→daemon:** `List`, `Spawn{cmd,cwd}`, `Kill{id}`, `Tag{id,on}`,
  `Resize{rows,cols}`, `Attach{id}`, `Detach`, `Input{id,bytes}`,
  `LoadSession{name}`, `SaveSession{name}`, `ShutdownDaemon`.
- **Daemon→client:** `Tasks(snapshot)`, `TaskDelta{id,…}`, `ScreenFull{id,bytes}`,
  `ScreenDiff{id,bytes}`, `Exited{id,code}`, `Error`.

## Reattach & live streaming (the payoff)

- On connect: daemon sends full `Tasks` snapshot → client paints the dashboard
  instantly, no re-run.
- On `Attach{id}`: daemon sends `ScreenFull` (`vt100::Screen::contents_formatted`)
  then streams `ScreenDiff` (`Screen::contents_diff(prev)`) as output arrives. A
  mid-run `vim`/`htop` shows its **current live screen**. (This is the deferred
  `contents_diff` work becoming load-bearing; it doubles as the attached-mode
  anti-flicker upgrade.)
- Daemon retains **scrollback** (bump `vt100` scrollback from 0). Scrollback
  paging in the client is a later feature.

## Lifecycle

- **Autostart:** client `connect()`s; on `ENOENT`/`ECONNREFUSED` it spawns
  `multi --daemon` detached (`setsid`, stdio → log), removes any stale socket,
  polls for the socket (~1s), connects. (tmux's model.)
- **Shutdown:** manual only. `q`/detach leaves it running. Explicit `ShutdownDaemon`
  (a UI key + `multi --kill`) group-kills all and exits. **No idle-exit.**
- **Honest limitation:** the daemon is the tasks' parent → **daemon death = task
  death**. No init-reparenting/systemd handoff; surviving a daemon crash is out of
  scope. Document, don't pretend.

## Hard parts (where the risk is)

1. **Framing + partial reads** — the main new code; `read_exact` + length prefix.
2. **Resize semantics** — attached pane size = client terminal; daemon resizes on
   `Attach`/`Resize`, keeps last size after detach.
3. **Backpressure** — firehose task vs. slow client; the diff model coalesces
   (send current screen state on a tick, not every byte).
4. **Autostart races / stale sockets** — handle deterministically.

## Milestones (each independently shippable & harness-tested)

1. **Define the seam in-process.** Introduce `TaskView` + `Command`/`Event`
   enums; make the *current* single-process UI drive the task-set through them.
   Zero IPC. Proves the boundary before splitting. No behavior change; existing
   harnesses stay green.
2. **Loopback transport.** Route client↔core through those types over an
   in-process channel. Still one process; proves the message set.
3. **Real daemon + socket.** Split into `multi` / `multi --daemon`; framing +
   autostart + attach/detach. `q` becomes disconnect; add explicit quit/kill.
4. **Live reattach.** `ScreenFull`/`ScreenDiff` streaming; scrollback retention.
5. **New-model UX.** Disconnect vs. quit bindings, "daemon status",
   reconnect-on-drop.

Doing the seam in-process **first** (1–2) lands the scary IPC work (3) on a
proven boundary instead of a big-bang rewrite — the architecturally-correct order.

## Deps

Nothing new: stdlib `UnixListener`/`UnixStream` for transport, `jzon` (already in)
for control frames, raw bytes for pane data. Still Unix-only.

## Deferred to v3+

- Multiple simultaneous clients (mirroring).
- Arbitrary named daemons + a minimal session-browser UI.
- Scrollback paging UI; deep search.
- Crash-resilient task ownership (reparent to init/launchd).
