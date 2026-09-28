# Codex terminal release captures

Recorded 2026-09-28 with the official arm64 macOS Codex 0.157.1 and 0.158.0
executables. `manifest.json` preserves source commits, executable/archive hashes,
capture fleetcom provenance, stream hashes, checkpoint offsets, and resize events.
The capture fleetcom tree was d138a752 plus the recorded dirty patch, not a clean
release checkout. These are raw terminal bytes, not constructed repaint fixtures.

The releases ran with isolated homes/workspaces, `--no-daemon`,
`TERM=xterm-256color`, and keyboard enhancement disabled. A loopback Responses
server supplied scripted replies and title responses. The unknown
`phase0-fixture-model` causes the warning footer. `SSH_TTY` forced OSC 52 output;
the desktop clipboard was never modified. Paths embedded in terminal output are
capture text, not runtime dependencies.

Each `transitions` file is one complete direct-Codex stream, including teardown.
Replay at 40 rows by 120 columns, resize at the recorded byte offsets to 20 by 60
and back, and inspect checkpoints before applying resizes at that same offset.
Assertions cover visible prompt/reply retention, `/new` clearing them, alternate
screen entry/exit, and the exact clipboard payload. Draining between checkpoints
ensures later repaints do not masquerade as repeated clipboard stores.

Each `fleetcom-load` file is a prefix of the outer fleetcom client's output,
ending after copy. It records an actual foreground fleetcom load of a saved Codex
thread. The attached checkpoint contains the previous reply; a later completed
turn contains both replies. Copy reaches the outer terminal as OSC 52. These
assertions verify replayed display and clipboard behavior; they do not execute
save/load. `tests/daemon_resume.rs` separately exercises that path with stubs.
The Phase 0 live-binary experiment verified preservation of the saved thread ID.

One stream per scenario avoids duplicate cumulative checkpoint blobs. Direct
session hashes identify the original captures; outer prefix hashes differ from
their full source sessions. The tests replay both whole segments and seven-byte
chunks without Codex, network access, or accounts. Synthetic clipboard size and
coalescing tests elsewhere retain their existing coverage.

Not verified here: Linux release execution, real accounts, native clipboard
backends, native right-click paste, mouse gestures, shared daemons, or child
process cleanup.
