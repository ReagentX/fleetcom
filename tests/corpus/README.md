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

The `preview_*.bin` fixtures pin the summary adapters (`src/harness/summary.rs`):
per-state agent-CLI screens whose extraction, normalization, and refusal
behavior the adapter tests assert exactly. Unlike the raw recordings above,
each is a constructed repaint stream — an optional alt-screen entry (claude
and grok run on the alternate screen; codex is inline), clear, home, then the
captured screen's rows joined with CRLF — trimmed from per-state snapshots of
claude 2.1.215, codex-cli 0.144.6, and grok 0.2.102. All identifying content
(names, account identifiers, filesystem paths, MCP server names, and every
user-configured statusline row) is replaced with same-length synthetic values,
so box borders stay column-aligned. Geometry is 40×120 unless noted.

| Fixture | Scenario | Coverage |
| --- | --- | --- |
| `preview_claude_working.bin` | claude spinner with the tmux focus-events hint row | `claude:spinner` extraction through an indented hint row |
| `preview_claude_working_tool.bin` | claude spinner over an indented tool-attachment row | `claude:spinner`; the attachment row is not the action row |
| `preview_claude_action.bin` | claude `⏺ Running 1 shell command…` above the spinner | `claude:action-row` preferred over the rotating verb |
| `preview_claude_approval.bin` | claude file-write approval dialog, input box replaced | `claude:approval-menu` synthesizes `awaiting approval` |
| `preview_claude_idle.bin` | claude idle with the `Try "…"` placeholder | fall-through to the marker; the placeholder never anchors |
| `preview_claude_done.bin` | claude after a finished turn (`✻ Crunched for 4s` in the body) | fall-through; body completion rows are out of the pinned window |
| `preview_claude_body_menu.bin` | approval-menu text quoted in the body while the spinner runs | negative: the pinned spinner wins over body menu shapes |
| `preview_claude_body_menu_idle.bin` | approval-menu text touching the chrome window, input box intact | negative: a foreign column-0 row aborts to fall-through |
| `preview_codex_working.bin` | codex `• Working (7s • esc to interrupt) · 1 background terminal running · /ps to view · /stop to close` | `codex:working` normalization: affordances stripped, slow suffix kept |
| `preview_codex_working_over_ran.bin` | codex working with a `• Ran` row higher in the same turn | `codex:working` wins at the pin; the stale row never surfaces |
| `preview_codex_scrollback.bin` | codex finished turn, `• Ran` from the prior turn in scrollback | negative: the scan stops at the reply bullet; floor tier reports |
| `preview_codex_ran.bin` | codex transient completion (synthetic: no raw capture holds it) | `codex:ran` extraction through the `└` attachment row |
| `preview_grok_working.bin` | grok braille spinner with elapsed/throughput ticker | `grok:spinner` cut at the label's `…`; border label read |
| `preview_grok_worked.bin` | grok `Worked for 8.7s` completion row above the box | `grok:worked` kept verbatim |
| `preview_grok_idle.bin` | grok idle session | fall-through to the marker |
| `preview_grok_splash.bin` | grok launch splash with resume hint above the box | fall-through; distinct views never anchor |
| `preview_trunc_claude.bin` | synthetic 40×80: spinner row truncated inside its parenthetical | head match still extracts `Hashing…` |
| `preview_trunc_codex.bin` | synthetic 40×80: working row truncated inside the `/ps` hint | head match still extracts; dropped suffix was strippable anyway |
| `preview_trunc_grok.bin` | synthetic 40×80: spinner label truncated with the CLI's ellipsis | extraction keeps the CLI's own `…` verbatim |
| `preview_wrap_grok.bin` | synthetic 40×30: the status row wraps its ellipsis onto the next row | pathological width fails the structure check and falls through |

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
