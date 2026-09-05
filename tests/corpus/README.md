# PTY capture corpus

Synthetic escape sequences isolate parser rules, but they do not reproduce the
state transitions emitted by real terminal programs. This corpus keeps their
raw PTY output so the emulator tests can replay those transitions byte for
byte. The fixtures provide the evidence for display and parser
assertions that would otherwise depend on synthetic approximations.

## Capture method

Each fixture is raw output from a 40×120 PTY with `TERM=xterm-256color`. Tests
feed the bytes to the emulator verbatim.

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

The `preview_*.bin` fixtures pin summary-adapter extraction, normalization,
and fallback behavior. Each fixture is a constructed repaint stream: optional
alternate-screen entry, clear, home, then sanitized screen rows joined with
CRLF. Claude and Grok use the alternate screen; Codex and omp are inline.
Identifying and user-configured text is replaced with alignment-preserving
synthetic values. Geometry is 40×120 unless noted.

`preview_codex_reasoning.bin` uses 40×80 geometry, and
`preview_codex_queued.bin` uses 40×36. Both are bottom-anchored on a 40-row
screen to reproduce the inline layout.

The Codex hint-row and approval fixtures use approximate indentation, so their
tests match trimmed heads and column-0 structure. The Claude waiting fixture
omits the welcome box and includes agent-roster rows below the input box. The
Claude task-list fixtures use generic phase names in a task-list layout; both
omit the welcome box. The Claude workflow-wait fixture uses generic wording,
omits the welcome box, and includes a long blank gap above the input box and a
roster below it. The omp fixtures replace the local model path and the working
directory in the status line with same-length synthetic values, and their
status rows carry a streamed intent phrase rather than omp's default
`Working…`. `preview_omp_idle_titled.bin` reuses the
`preview_omp_idle.bin` rows verbatim and prepends an
`ESC]0;π > fix the parser BEL` title announce.

| Fixture | Scenario | Coverage |
| --- | --- | --- |
| `preview_claude_working.bin` | claude spinner with the tmux focus-events hint row | `claude:spinner` extraction through an indented hint row |
| `preview_claude_working_tool.bin` | claude spinner over an indented tool-attachment row | `claude:spinner`; the attachment row is not the action row |
| `preview_claude_action.bin` | claude `⏺ Running 1 shell command…` above the spinner | `claude:action-row` preferred over the rotating verb |
| `preview_claude_approval.bin` | claude file-write approval dialog, input box replaced | `claude:approval-menu` synthesizes `awaiting approval` |
| `preview_claude_idle.bin` | claude idle with the `Try "…"` placeholder | fall-through to the marker; the placeholder never anchors |
| `preview_claude_done.bin` | claude after a finished turn (`✻ Crunched for 4s` in the body) | fall-through; body completion rows are out of the pinned window |
| `preview_claude_body_menu.bin` | approval-menu text quoted in the body while the spinner runs | the pinned spinner wins; body menu text is ignored |
| `preview_claude_body_menu_idle.bin` | approval-menu text touching the chrome window, input box intact | a foreign column-0 row aborts extraction and resolves to the marker |
| `preview_claude_tasklist.bin` | claude spinner above a six-row task-list block, agent roster below the box | `claude:spinner` through a bounded indented block |
| `preview_claude_body_above_tasklist.bin` | spinner-shaped body row above `⏺` prose and the task list | negative: column-0 prose invalidates the status structure |
| `preview_codex_working.bin` | codex `• Working (7s • esc to interrupt) · 1 background terminal running · /ps to view · /stop to close` | `codex:working` normalization: affordances stripped, slow suffix kept |
| `preview_codex_working_over_ran.bin` | codex working with a `• Ran` row higher in the same turn | `codex:working` wins at the pin; the stale row never surfaces |
| `preview_codex_scrollback.bin` | codex finished turn, `• Ran` from the prior turn in scrollback | the scan stops at the reply bullet and resolves to the floor tier |
| `preview_codex_ran.bin` | codex transient completion row | `codex:ran` extraction through the `└` attachment row |
| `preview_codex_hint_row.bin` | codex working with `tab to queue message` below the composer, no status line | `codex:working` through the composer pin; no model prefix without one |
| `preview_codex_approval.bin` | codex approval modal: composer and status line replaced by a numbered menu | `codex:approval-menu` synthesizes `awaiting approval` |
| `preview_codex_reasoning.bin` | codex status row with a reasoning phrase over an `• Explored` group and reply bullet; composer carrying text; no status line | `codex:working` keeps the header verbatim; the `•`-headed rows above it do not surface, and no status line means no model prefix |
| `preview_codex_queued.bin` | codex `• Working` row separated from the composer by a `• Queued follow-up inputs` block; `model-with-reasoning · current-dir` status line below | `codex:working` survives the queued-message heads; the first status-line item supplies the model label |
| `preview_codex_body_menu.bin` | modal-shaped menu quoted in the body, live composer below | negative: the composer's presence suppresses the modal match; floor tier reports |
| `preview_claude_waiting.bin` | claude waiting on a backgrounded subagent, `⏺` prose and agent roster around the box | `claude:waiting` extracts the ellipsis-less row verbatim; no model label mid-session |
| `preview_claude_workflow_wait.bin` | claude waiting on a dynamic workflow, with 19 blank rows before the input box and a workflow roster below it | `claude:waiting` matches across the blank rows; the roster is excluded |
| `preview_grok_working.bin` | grok braille spinner with elapsed/throughput ticker | `grok:spinner` cut at the label's `…`; border label read |
| `preview_grok_worked.bin` | grok `Worked for 8.7s` completion row above the box | `grok:worked` kept verbatim |
| `preview_grok_still_running.bin` | grok `◎ 1 subagent still running` above the idle box | `grok:still-running`; border label read |
| `preview_grok_subagent_scrollback.bin` | grok idle with `Subagent running:` in the body, no `◎` row | fall-through; body-shaped text is not status |
| `preview_grok_idle.bin` | grok idle session | fall-through to the marker |
| `preview_grok_splash.bin` | grok launch splash with resume hint above the box | fall-through; distinct views never anchor |
| `preview_omp_working.bin` | omp status row carrying the model's streamed intent phrase above the input box | `omp:spinner`; padding, spinner frame, and interrupt hint stripped |
| `preview_omp_approval.bin` | omp approval selector, input box replaced, tool-call preview box and a live status row still above it | `omp:approval-menu` synthesizes `awaiting approval` while the status row keeps animating |
| `preview_omp_idle.bin` | omp idle with the welcome box and tip above the input box | fall-through to the floor tier; an inline UI reaches no marker |
| `preview_omp_idle_titled.bin` | `preview_omp_idle.bin` after a `π > fix the parser` title announce | the primary-screen title tier strips the idle prefix and renders `fix the parser` |
| `preview_omp_body_hint.bin` | status-shaped row quoted in the transcript, prose between it and an idle input box | negative: the pin is the row above the box, not a substring search |
| `preview_trunc_claude.bin` | synthetic 40×80: spinner row truncated inside its parenthetical | head match still extracts `Hashing…` |
| `preview_trunc_codex.bin` | synthetic 40×80: working row truncated inside the `/ps` hint | the head still matches and the key-hint suffix is omitted |
| `preview_trunc_grok.bin` | synthetic 40×80: spinner label truncated with the CLI's ellipsis | extraction keeps the CLI's own `…` verbatim |
| `preview_wrap_grok.bin` | synthetic 40×30: the status row wraps its ellipsis onto the next row | the structure check fails and resolves to the marker |

## What the fixtures prove

The fixtures provide evidence for display and parser behavior:

- `tmux_split`, `vim_session`, `less_altscreen`, `top_live`, `shell_colors`,
  and `build_log` pin displayed state: every plain-text row, the cursor, and
  selected styled cells.
- `codex_resume`, `wide_emoji`, `dec_scrollregion`, and `topregion_scroll` pin
  parser semantics: scrollback retention, intensity stacking, charset
  translation, and VS16 width.
- `claude_resume`, `codex_resume`, and `grok_resume` compare the emulator's
  final display, cursor, and alternate-screen state with the terminal backend.

`src/terminal/golden.rs` contains the absolute display and parser expectations
and the terminal-backend comparisons. `src/harness/summary_tests.rs` contains
the preview-fixture expectations.
