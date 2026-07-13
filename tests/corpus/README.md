# PTY capture corpus

Terminal emulators are difficult to test with synthetic escape sequences
alone. This corpus preserves the raw output of real terminal programs so the
emulator tests can replay the same byte stream into each backend.

## Capture method

Each fixture was recorded under a 40×120 PTY with
`TERM=xterm-256color`, using a scripted `pty.fork` driver. Tests feed these
bytes to the parsers verbatim. If a fixture needs to change, record a new
session; do not patch the captured bytes.

## Fixtures

| Fixture | Source | Coverage |
| --- | --- | --- |
| `codex_resume.bin` | real `codex resume` session replay | top-anchored DECSTBM scroll regions (`CSI 1;N r`), reverse index, inline-TUI history insertion |
| `tmux_split.bin` | `tmux` session with two splits and one command per pane | scroll regions, pane borders, full redraws |
| `vim_session.bin` | `vim -u NONE`: insert, navigate, `:set number`, `:q!` | alternate screen, cursor addressing, line editing |
| `less_altscreen.bin` | `less` over `/usr/share/dict/words`: page, `G`, `g`, `q` | alternate-screen entry and exit, full-screen paging |
| `top_live.bin` | live `top` session for approximately four seconds, then `q` | rapid full-screen redraws, HPA/VPA addressing |
| `shell_colors.bin` | `ls --color`, `git log --color`, and 16-color, 256-color, and truecolor SGR | SGR runs, color-depth coverage |
| `build_log.bin` | `cargo check` and `cargo clippy` with `CARGO_TERM_COLOR=always` | bulk scrolling output, styled diagnostics |
| `wide_emoji.bin` | `printf` output containing VS16 emoji, CJK, and combining marks | wide and zero-width character semantics; both configured backends treat VS16 as zero-width |
| `dec_scrollregion.bin` | `printf` output with DEC line drawing (`ESC ( 0`) and `CSI 5;20r` | DEC charset translation; the region does not scroll because it is not top-anchored, and both backends retain the later full-screen scroll |
| `topregion_scroll.bin` | `printf` output with `CSI 1;20r`, 34 newlines through the bottom margin, and an isolation line below the region | top-anchored-region retention: vt100 retains no rows, while alacritty retains 35, including the row preserved by `ESC[2J` |

## Test classification

The differential suite uses `tmux_split`, `vim_session`, `less_altscreen`,
`top_live`, `shell_colors`, and `build_log` to verify compatible displayed
state. It uses `codex_resume`, `wide_emoji`, `dec_scrollregion`, and
`topregion_scroll` to assert exact semantic behavior where the backends differ
or where parity is significant.
