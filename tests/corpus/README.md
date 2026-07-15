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
contain the agent-resume scrape expectations.
