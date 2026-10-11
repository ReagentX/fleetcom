# Codex 0.158.0 preview captures

Captured 2026-09-28 from the official macOS arm64 release executable in a
40-row × 120-column PTY with `TERM=xterm-256color`. Each file is the complete,
unmodified direct Codex PTY stream from startup through teardown. Tests replay
prefixes at the byte checkpoints below. There are no resize events in these
three scenarios. The `fullscreen` scenarios use Codex's alternate screen; they
are not captures of an outer fleetcom client. The 0.157.1 pair of each
scenario, captured the same day, was deleted on 2026-10-10.

The Responses endpoint was a loopback server with scripted replies and harmless
tool calls. Each run used an isolated home and workspace, `--no-daemon`, and
`CODEX_TUI_DISABLE_KEYBOARD_ENHANCEMENT=1`. This is real CLI rendering with a
mock provider, not a hosted-model or account test. The unknown fixture model
produces a warning beside the inline model footer. Working-state tests assert
`live_preview` separately so they do not preserve the existing missing-label
behavior. Approval tests also verify preview resolution.

The inline scenario submits a prompt, queues `Queued phase zero message` with
Tab, retrieves it with Shift-Left (`ESC [ 1 ; 2 D`), clears the draft with
Ctrl-U, and waits for completion. The approval scenario declines a `printf`
command. The stdin scenario approves a shell command that reads one line, then
the mock requests `fixture\n` as terminal input and a second approval menu is
displayed. Tests check the rendered rows and adapter results against the
reviewed capture screens, not generated expectations.

## Producer provenance

| Version | Source commit | Official macOS arm64 tarball SHA-256 |
| --- | --- | --- |
| 0.158.0 | `064c6b8c737f5b41d171fdda80bd9ef10ad06eb3` | `341c4a08f9ce1935b3007376dc2a3d50a0a89112930e9a474ae61367218f6e8a` |

Imported from the matching scenario's `session.bin` in the Phase 0 evidence
bundle `fleetcom-phase0.STTTSl`; stream hashes match that bundle's manifests
and fixture index. No home directories, request dumps, or pilot runs are
included. All paths and IDs visible in the streams belong to isolated capture
workspaces. Playback requires no Codex executable, network, or account.

## Streams and checkpoints

Offsets are exclusive byte ends measured from startup. Dimensions remain
40×120 at every checkpoint.

| Stream | Tested checkpoints | Bytes | SHA-256 |
| --- | --- | ---: | --- |
| `0.158.0-inline.bin` | working 11311; queued 14478; Shift-Left 16558; completed 17253 | 17516 | `891a15ed16251b0af90ca3cf06311d401fef8e7a0bf93a3fea240f3a2f74f8ac` |
| `0.158.0-fullscreen-approval.bin` | approval 12967 | 14592 | `357b3c8c5b4d6b34b422d08f745d92700b4969635d8e29282d1cb76b48b84d8e` |
| `0.158.0-fullscreen-stdin.bin` | command approval 13496; stdin approval 17417 | 19416 | `cb8c3662a0121b8dd1fd73ee3d15de3ae9208bdd72f054b1c71c58460d27ab47` |

These captures do not establish Linux CLI rendering, real-account behavior,
native mouse/clipboard behavior, or shared-daemon compatibility. Tests replay
the recorded bytes portably; they do not drive a live CLI.
