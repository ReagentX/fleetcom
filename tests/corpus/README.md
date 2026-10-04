# PTY capture corpus

Use synthetic escape sequences to isolate parser rules, and captured PTY output
to test state transitions from real terminal programs. Raw output is stored in
this corpus for byte-for-byte replay through the emulator, with display and
parser assertions checked against captured behavior.

## Capture method

The raw captures listed below start in a 40×120 PTY with
`TERM=xterm-256color`. Feed their bytes to the emulator verbatim. The
`preview_*.bin` fixtures are constructed repaints, described separately below.
Versioned capture directories record their own geometry, checkpoints, and
resize events; they also distinguish direct child output from an outer
fleetcom client's output.

## Fixtures

| Fixture | Scenario | Coverage |
| --- | --- | --- |
| `claude_resume.bin` | `claude --session-id`: one prompt, reply, `/exit` | alternate-screen teardown and final primary-screen display |
| `codex_resume.bin` | `codex resume` | top-anchored DECSTBM scroll regions (`CSI 1;N r`), reverse index, inline-TUI history insertion, and SGR-styled exit output |
| `grok_resume.bin` | `grok --session-id`: one prompt, reply, `/exit` | final primary-screen display |
| `tmux_split.bin` | `tmux` session with two splits and one command per pane | scroll regions, pane borders, full redraws |
| `vim_session.bin` | `vim -u NONE`: insert, navigate, `:set number`, `:q!` | alternate screen, cursor addressing, line editing |
| `less_altscreen.bin` | `less` over `/usr/share/dict/words`: page, `G`, `g`, `q` | alternate-screen entry and exit, full-screen paging |
| `top_live.bin` | live `top` session for approximately four seconds, then `q` | rapid full-screen redraws, HPA/VPA addressing |
| `shell_colors.bin` | `ls --color`, `git log --color`, and 16-color, 256-color, and truecolor SGR | SGR runs, color-depth coverage |
| `build_log.bin` | `cargo check` and `cargo clippy` with `CARGO_TERM_COLOR=always` | bulk scrolling output, styled diagnostics |
| `wide_emoji.bin` | `printf` output containing VS16 emoji, CJK, and combining marks | wide and zero-width characters; VS16 remains a zero-width attachment on a single-cell base |
| `dec_scrollregion.bin` | `printf` output with DEC line drawing (`ESC ( 0`) and `CSI 5;20r` | DEC charset translation; the non-top-anchored region does not scroll, and the later full-screen scroll retains one row |
| `topregion_scroll.bin` | `printf` output with `CSI 1;20r`, 34 newlines through the bottom margin, and an isolation line below the region | top-anchored-region retention pinned at 35: 34 region scrolls plus the row preserved by `ESC[2J` |

## Preview fixtures

Use the `preview_*.bin` fixtures to verify summary-adapter extraction,
normalization, and fallback behavior. Each fixture is a constructed repaint
stream: optional alternate-screen entry, clear, home, then sanitized screen rows
joined with CRLF. Claude and Grok use the alternate screen; Codex and omp are
inline. Identifying and user-configured text is replaced with
alignment-preserving synthetic values. Geometry is 40×120 unless noted.

Geometry is 40×80 for `preview_codex_reasoning.bin` and 40×36 for
`preview_codex_queued.bin`. Both are bottom-anchored on a 40-row screen to
reproduce the inline layout.

Indentation in the Codex hint-row and approval fixtures is approximate; match
trimmed heads and column-0 structure in their tests. In the Claude waiting
fixture, the welcome box is omitted and agent-roster rows are included below the
input box. In the Claude task-list fixtures, generic phase names are used in a
task-list layout and the welcome box is omitted. In the Claude workflow-wait
fixture, generic wording is used, the welcome box is omitted, and a long blank
gap is included above the input box with a roster below it. In the omp fixtures,
the local model path and working directory in the status line are replaced with
same-length synthetic values. A streamed intent phrase is used in status rows
rather than omp's default `Working…`. In `preview_omp_idle_titled.bin`, the
`preview_omp_idle.bin` rows are reused verbatim and preceded by an `ESC]0;π >
fix the parser BEL` title announce.

| Fixture | Scenario | Coverage |
| --- | --- | --- |
| `preview_claude_working.bin` | claude spinner with the tmux focus-events hint row | `claude:spinner` extraction through an indented hint row |
| `preview_claude_working_tool.bin` | claude spinner over an indented tool-attachment row | `claude:spinner`; the attachment row is not the action row |
| `preview_claude_action.bin` | claude `⏺ Running 1 shell command…` above the spinner | `claude:action-row` preferred over the rotating verb |
| `preview_claude_approval.bin` | claude file-write approval dialog, input box replaced | `awaiting approval` synthesized by `claude:approval-menu` |
| `preview_claude_idle.bin` | claude idle with the `Try "…"` placeholder | fall-through to the marker; the placeholder excluded from anchors |
| `preview_claude_done.bin` | claude after a finished turn (`✻ Crunched for 4s` in the body) | fall-through; body completion rows are out of the pinned window |
| `preview_claude_body_menu.bin` | approval-menu text quoted in the body while the spinner runs | the pinned spinner preferred; body menu text is ignored |
| `preview_claude_body_menu_idle.bin` | approval-menu text touching the chrome window, input box intact | extraction aborted at a foreign column-0 row; marker fallback |
| `preview_claude_tasklist.bin` | claude spinner above a six-row task-list block, agent roster below the box | `claude:spinner` through a bounded indented block |
| `preview_claude_body_above_tasklist.bin` | spinner-shaped body row above `⏺` prose and the task list | negative: status structure rejected on column-0 prose |
| `preview_codex_working.bin` | codex `• Working (7s • esc to interrupt) · 1 background terminal running · /ps to view · /stop to close` | `codex:working` normalization: affordances stripped, slow suffix kept |
| `preview_codex_working_over_ran.bin` | codex working with a `• Ran` row higher in the same turn | `codex:working` preferred at the pin; stale row excluded |
| `preview_codex_scrollback.bin` | codex finished turn, `• Ran` from the prior turn in scrollback | scan stopped at the reply bullet; floor-tier fallback |
| `preview_codex_ran.bin` | codex transient completion row | `codex:ran` extraction through the `└` attachment row |
| `preview_codex_hint_row.bin` | codex working with `tab to queue message` below the composer, no status line | `codex:working` through the composer pin; no model prefix without one |
| `preview_codex_approval.bin` | codex approval modal: composer and status line replaced by a numbered menu | `awaiting approval` synthesized by `codex:approval-menu` |
| `preview_codex_reasoning.bin` | codex status row with a reasoning phrase over an `• Explored` group and reply bullet; composer carrying text; no status line | header kept verbatim by `codex:working`; preceding `•`-headed rows excluded; no model prefix without a status line |
| `preview_codex_queued.bin` | codex `• Working` row separated from the composer by a `• Queued follow-up inputs` block; `model-with-reasoning · current-dir` status line below | `codex:working` extracted across queued-message heads; model label from the first status-line item |
| `preview_codex_body_menu.bin` | modal-shaped menu quoted in the body, live composer below | negative: modal match suppressed with composer present; floor-tier fallback |
| `preview_claude_waiting.bin` | claude waiting on a backgrounded subagent, `⏺` prose and agent roster around the box | ellipsis-less row extracted verbatim by `claude:waiting`; no model label mid-session |
| `preview_claude_workflow_wait.bin` | claude waiting on a dynamic workflow, with 19 blank rows before the input box and a workflow roster below it | blank rows skipped during `claude:waiting` matching; the roster is excluded |
| `preview_grok_working.bin` | grok braille spinner with elapsed/throughput ticker | `grok:spinner` cut at the label's `…`; border label read |
| `preview_grok_worked.bin` | grok `Worked for 8.7s` completion row above the box | `grok:worked` kept verbatim |
| `preview_grok_still_running.bin` | grok `◎ 1 subagent still running` above the idle box | `grok:still-running`; border label read |
| `preview_grok_subagent_scrollback.bin` | grok idle with `Subagent running:` in the body, no `◎` row | fall-through; body-shaped text is not status |
| `preview_grok_idle.bin` | grok idle session | fall-through to the marker |
| `preview_grok_splash.bin` | grok launch splash with resume hint above the box | fall-through; distinct views excluded from anchors |
| `preview_omp_working.bin` | omp status row carrying the model's streamed intent phrase above the input box | `omp:spinner`; padding, spinner frame, and interrupt hint stripped |
| `preview_omp_approval.bin` | omp approval selector, input box replaced, tool-call preview box and a live status row still above it | `awaiting approval` synthesized by `omp:approval-menu` with status animation still active |
| `preview_omp_idle.bin` | omp idle with the welcome box and tip above the input box | fall-through to the floor tier; no marker for an inline UI |
| `preview_omp_idle_titled.bin` | `preview_omp_idle.bin` after a `π > fix the parser` title announce | idle prefix stripped and `fix the parser` rendered in the primary-screen title tier |
| `preview_omp_body_hint.bin` | status-shaped row quoted in the transcript, prose between it and an idle input box | negative: the pin is the row above the box, not a substring search |
| `preview_trunc_claude.bin` | synthetic 40×80: spinner row truncated inside its parenthetical | `Hashing…` still extracted by head match |
| `preview_trunc_codex.bin` | synthetic 40×80: working row truncated inside the `/ps` hint | head still matched; key-hint suffix omitted |
| `preview_trunc_grok.bin` | synthetic 40×80: spinner label truncated with the CLI's ellipsis | CLI's own `…` kept verbatim |
| `preview_wrap_grok.bin` | synthetic 40×30: ellipsis wrapped onto the next row | structure rejected; marker fallback |

## Codex 0.157.1 and 0.158.0 captures

The versioned directories contain raw PTY streams from the official macOS arm64
release executables, recorded on 2026-09-28. Codex ran with isolated homes,
`--no-daemon`, and a loopback Responses server supplying scripted replies:

- [`codex_0158_preview`](codex_0158_preview/README.md) covers working status,
  queued input, Shift-Left draft retrieval, completed-turn rejection by the
  working matcher, ordinary command approval, and the additional terminal-input
  approval in 0.158.0. The existing approval matcher recognizes both menus.
- [`codex_0158_terminal`](codex_0158_terminal/README.md) covers direct Codex
  output across resize, `/new`, alternate-screen teardown, and OSC 52 copy.
  Separate outer fleetcom captures show resumed history and the exact clipboard
  payload forwarded to the terminal.

Each directory documents provenance and replay boundaries. The terminal
manifest records the capture fleetcom tree: `d138a752` plus the recorded dirty
patch, not a clean release checkout. Tests replay the bytes without Codex,
network access, or accounts. Replaying a saved-thread capture checks its display
and clipboard output; it does not execute save/load. See
[agent session resume](../../docs/agent-resume.md#codex) for the live validation.

These recordings do not establish Linux release behavior, real-account
behavior, native clipboard or mouse behavior, or shared-daemon compatibility.
Replay tests are portable; the live capture platform was macOS arm64.

## Validation coverage

Verify display and parser behavior against these fixtures:

- `tmux_split`, `vim_session`, `less_altscreen`, `top_live`, `shell_colors`,
  and `build_log` for displayed state: every plain-text row, the cursor, and
  selected styled cells.
- `codex_resume`, `wide_emoji`, `dec_scrollregion`, and `topregion_scroll` for
  parser semantics: scrollback retention, intensity stacking, charset
  translation, and VS16 width.
- Every raw capture listed above for comparison of the emulator's final styled
  cells, text, cursor, and alternate-screen state with the terminal backend,
  and for a serialize-and-replay round trip.

See `src/terminal/golden.rs` for absolute display and parser expectations and
terminal-backend comparisons, `src/terminal/ansi.rs` for the round trip, and
`src/harness/summary_tests.rs` for preview-fixture expectations.
