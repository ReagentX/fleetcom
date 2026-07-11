# Phase 2 — Persistence & Reattach

Design doc / resume anchor. Written before starting implementation; decisions
below are settled. **Milestones 1–3 landed**; **M4/M5 done to a focused scope** —
send-on-change `Screen` (no idle attach churn), reconnect-on-drop, and a
foreground indicator. Full `ScreenDiff` coalescing and scrollback retention were
**deferred to v3+** as premature: on a localhost single client the receiver
already coalesces screens to its render rate, and retained scrollback does
nothing until a paging UI (also v3+). Phase 2's core is complete; what remains is
v3+ (below).

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
- **Honest limitation (corrected in M5):** on a **clean** shutdown the daemon's
  `Task::drop` group-kills every job, so quit/`--kill`/client-Shutdown kill all.
  But on a **crash / SIGKILL** `Drop` never runs, and the jobs are `setsid`'d into
  their own sessions, so they are **orphaned and survive** (reparented to init) —
  the *opposite* of "task death", and the earlier claim here was wrong. A
  reconnecting client autostarts a fresh daemon that does **not** adopt those
  orphans; they run headless until manually killed. Crash-resilient ownership
  (kill-on-crash via pdeathsig, or adopt-on-reconnect) is v3+.

## Hard parts (where the risk is)

1. **Framing + partial reads** — the main new code; `read_exact` + length prefix.
2. **Resize semantics** — attached pane size = client terminal; daemon resizes on
   `Attach`/`Resize`, keeps last size after detach.
3. **Backpressure** — firehose task vs. slow client; the diff model coalesces
   (send current screen state on a tick, not every byte).
4. **Autostart races / stale sockets** — handle deterministically.

## Milestones (each independently shippable & harness-tested)

1. **Define the seam in-process.** ✅ **Done.** `protocol.rs` (`Command`/`Event`/
   `TaskView`/`ScreenView`), `supervisor.rs` (the task owner — `apply`/`tick`/
   `drain`), `path.rs` (shared path helpers); `app.rs`/`ui.rs` are now a client
   over a `Vec<TaskView>` mirror, holding no `Task`. Zero IPC. 16 unit tests + 9
   PTY harnesses green (incl. idle-silence, fast group-kill quit, SIGTERM
   restore) — no behavior change.
2. **Loopback transport.** ✅ **Done.** `transport.rs`: a `Transport` trait the
   client's loop speaks (`send`/`poll`/`shutdown`); `ThreadTransport` runs the
   `Supervisor` on its own thread behind a pair of mpsc channels (commands out,
   events back), `LocalTransport` keeps the unit tests synchronous. Still one
   process; the message set now genuinely crosses a thread boundary. Same 16
   unit tests + 9 harnesses green (idle still 0 B/2 s, fast group-kill quit,
   SIGTERM restore). The socket is a third `Transport` impl — the client is
   already blind to which it holds.
3. **Real daemon + socket.** *Commit 1 (3a) done:* `frame.rs` (`[u32 len][u8
   kind][payload]`) + `Command`/`Event` serialization (jzon control, raw `Screen`
   tail) + `daemon.rs` (`multi --daemon` owns the one `Supervisor`, persists
   across reconnects) + `SocketTransport` (a third `Transport` impl) + autostart.
   `ThreadTransport` lives on as `multi --foreground`. *Commit 2 (3b) done:* the
   disconnect/quit split — `ExitIntent {Disconnect, Quit}` threads through
   `Transport::shutdown`; `q`/Ctrl-C/signals disconnect (close the socket, daemon
   survives), `Q` and `multi --kill` quit (Shutdown → daemon kills all + exits).
   In-process cores kill all on either intent, so the `--foreground` UI harnesses
   are unchanged. 14 harnesses green (9 UI on `--foreground` + 5 daemon:
   Q-quit, attach streaming, crash-survival, q-disconnect+reattach, `--kill`).
4. **Live reattach.** *Focused scope done:* send-on-change `Screen` — the daemon
   skips re-emitting an unchanged watched screen, killing the ~20/s idle-attach
   churn (`Supervisor::last_screen`). *Deferred to v3+:* full `ScreenDiff`
   coalescing (needs a client-side vt100 parser to reconstruct) and scrollback
   retention (pointless without a paging UI).
5. **New-model UX.** ✅ **Done.** Disconnect/quit bindings (m3b); a foreground
   indicator in the header; and reconnect-on-drop — `Transport::connected` goes
   false on socket EOF, the client shows a "daemon connection lost" banner, and
   `r` autostarts a fresh daemon instead of freezing on a stale mirror.

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
