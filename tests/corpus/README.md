# PTY capture corpus

Synthetic escape sequences isolate parser rules, but they do not reproduce the
state transitions emitted by real terminal programs. This corpus keeps their
raw PTY output so the emulator tests can replay those transitions byte for
byte. The fixtures provide the evidence for display, parser, and resume-hint
assertions that would otherwise depend on synthetic approximations.

## Capture method

Each fixture is raw output from a 40×120 PTY with `TERM=xterm-256color`. Tests
feed the bytes to the emulator verbatim.

## Fixtures

| Fixture | Scenario | Coverage |
| --- | --- | --- |
| `claude_resume.bin` | `claude --session-id`: one prompt, reply, `/exit` | alternate-screen exit followed by the primary-screen resume hint (`claude --resume <uuid>`); the scrape target for harness exit capture |
| `codex_resume.bin` | `codex resume` | top-anchored DECSTBM scroll regions (`CSI 1;N r`), reverse index, inline-TUI history insertion, and an SGR-split resume hint for harness exit capture |
| `grok_resume.bin` | `grok --session-id`: one prompt, reply, `/exit` | primary-screen exit followed by the resume hint (`grok --resume <uuid>`); the scrape target for harness exit capture |
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
CRLF. Claude and Grok use the alternate screen; Codex is inline. Identifying
and user-configured text is replaced with alignment-preserving synthetic
values. Geometry is 40×120 unless noted.

Most Codex fixtures were sanitized from a live 0.144.6 session. Two were built
instead from codex 0.147.0's own snapshot tests, whose expectations are the
rendered rows themselves: `preview_codex_reasoning.bin` from a full-screen
vt100 snapshot at 40×80, and `preview_codex_queued.bin` at 40×36. Both are
bottom-anchored on a 40-row screen the way an inline TUI paints. Sourcing rows
from the CLI's own test suite pins them to a named upstream version, which a
sanitized capture cannot do.

The Codex hint-row and approval fixtures use approximate indentation, so their
tests match trimmed heads and column-0 structure. The Claude waiting fixture
omits the welcome box and includes agent-roster rows below the input box. The
Claude task-list fixtures use generic phase names in a task-list layout; both
omit the welcome box. The Claude workflow-wait fixture uses generic wording,
omits the welcome box, and includes a long blank gap above the input box and a
roster below it.

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
| `preview_codex_reasoning.bin` | codex 0.147.0 status row headed by the model's own reasoning phrase, over an `• Explored` group and a reply bullet, composer carrying text, no status line | `codex:working` keeps the CLI's header verbatim; the `•`-headed rows above it never surface, and no status line means no model prefix |
| `preview_codex_queued.bin` | codex 0.147.0 `• Working` row separated from the composer by a `• Queued follow-up inputs` block, default `status_line` below | `codex:working` survives the queued-message heads; the model label reads the default `model-with-reasoning · current-dir` shape |
| `preview_codex_body_menu.bin` | modal-shaped menu quoted in the body, live composer below | negative: the composer's presence suppresses the modal match; floor tier reports |
| `preview_claude_waiting.bin` | claude waiting on a backgrounded subagent, `⏺` prose and agent roster around the box | `claude:waiting` extracts the ellipsis-less row verbatim; no model label mid-session |
| `preview_claude_workflow_wait.bin` | claude waiting on a dynamic workflow, with 19 blank rows before the input box and a workflow roster below it | `claude:waiting` matches across the blank rows; the roster is excluded |
| `preview_grok_working.bin` | grok braille spinner with elapsed/throughput ticker | `grok:spinner` cut at the label's `…`; border label read |
| `preview_grok_worked.bin` | grok `Worked for 8.7s` completion row above the box | `grok:worked` kept verbatim |
| `preview_grok_still_running.bin` | grok `◎ 1 subagent still running` above the idle box | `grok:still-running`; border label read |
| `preview_grok_subagent_scrollback.bin` | grok idle with `Subagent running:` in the body, no `◎` row | fall-through; body-shaped text is not status |
| `preview_grok_idle.bin` | grok idle session | fall-through to the marker |
| `preview_grok_splash.bin` | grok launch splash with resume hint above the box | fall-through; distinct views never anchor |
| `preview_trunc_claude.bin` | synthetic 40×80: spinner row truncated inside its parenthetical | head match still extracts `Hashing…` |
| `preview_trunc_codex.bin` | synthetic 40×80: working row truncated inside the `/ps` hint | the head still matches and the key-hint suffix is omitted |
| `preview_trunc_grok.bin` | synthetic 40×80: spinner label truncated with the CLI's ellipsis | extraction keeps the CLI's own `…` verbatim |
| `preview_wrap_grok.bin` | synthetic 40×30: the status row wraps its ellipsis onto the next row | the structure check fails and resolves to the marker |

## What the fixtures prove

The fixtures provide evidence for three distinct boundaries:

- `tmux_split`, `vim_session`, `less_altscreen`, `top_live`, `shell_colors`,
  and `build_log` pin displayed state: every plain-text row, the cursor, and
  selected styled cells.
- `codex_resume`, `wide_emoji`, `dec_scrollregion`, and `topregion_scroll` pin
  parser semantics: scrollback retention, intensity stacking, charset
  translation, and VS16 width.
- `claude_resume`, `codex_resume`, and `grok_resume` verify that retained
  terminal text preserves the exit hints consumed by their harnesses.

`src/golden.rs` contains the absolute display and parser expectations.
`src/harness/claude.rs`, `src/harness/codex.rs`, and `src/harness/grok.rs`
contain the agent-resume scrape expectations. `src/harness/summary.rs`
contains the preview-fixture expectations.
