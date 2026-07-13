# PTY capture corpus

Raw byte captures of real programs under a 40×120 PTY (`TERM=xterm-256color`),
recorded via a scripted `pty.fork` driver. Inputs to the emulator golden
suites (EMULATOR_MIGRATION.md, "Testing"): fed verbatim to a parser, never
edited. Regenerating a fixture means re-recording, not patching bytes.

| fixture | source | exercises |
| --- | --- | --- |
| `codex_resume.bin` | real `codex resume` session replay | top-anchored DECSTBM scroll regions (`CSI 1;N r`), reverse index, inline-TUI history insertion — the defect that motivated the migration |
| `tmux_split.bin` | tmux new-session, two splits, a command in each | scroll regions, pane borders, full redraws |
| `vim_session.bin` | vim -u NONE: insert, navigate, :set number, :q! | alt screen, cursor addressing, line editing |
| `less_altscreen.bin` | less over /usr/share/dict/words: page, G, g, q | alt screen enter/exit, full-screen paging |
| `top_live.bin` | live `top` for ~4s, then q | rapid full-screen redraws, HPA/VPA addressing |
| `shell_colors.bin` | ls --color, git log --color, 16/256/truecolor SGR | SGR runs, color depth coverage |
| `build_log.bin` | cargo check + clippy, CARGO_TERM_COLOR=always | bulk scrolling output, styled diagnostics |
| `wide_emoji.bin` | printf: VS16 emoji, CJK, combining marks | width semantics — vt100 and alacritty *disagree* here (VS16); semantic-suite candidate |
| `dec_scrollregion.bin` | printf: DEC line drawing (ESC ( 0), explicit top-anchored scroll region | charset shifts; scrollback-retention delta between backends; semantic-suite candidate |

Captures marked semantic-suite candidates exercise known parser-level deltas
(see the migration plan's classifier whitelist); the rest are
compatibility-suite candidates. Final classification happens where the suites
are defined, not here.
