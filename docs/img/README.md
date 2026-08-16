# README screenshots

Each `.ansi` file is one dashboard frame written by the real renderer over a
fabricated fleet, so a terminal `cat`-ing it paints exactly what a live run
paints.

| Frame | Shows |
| -- | -- |
| `home.ansi` | grouped by directory, the list outgrowing its region |
| `quickpeek.ansi` | grouped by state, peek open over a finished run |
| `groups.ansi` | grouped by custom group: one section spans five directories, and one directory feeds two sections |
| `controls.ansi` | the `?` overlay over the dir-grouped dashboard |

All four render the same 21 tasks. `attach.png` is captured from a live session,
because it needs a live attach.

The fixture is `write_readme_screenshot_fixtures` in `src/app_readme_tests.rs`. Edit the
fleet there (task names, previews, ages, tags, selection), then regenerate:

```sh
cargo test -- --ignored write_readme_screenshot_fixtures
```

Every duration in the fixture is constant. With the same `$HOME`, two runs write
identical bytes; section labels abbreviate that path to `~`.

## Capturing

```sh
clear; cat docs/img/home.ansi;      read -rsk 1; printf '\033[?25h'
clear; cat docs/img/quickpeek.ansi; read -rsk 1; printf '\033[?25h'
clear; cat docs/img/groups.ansi;    read -rsk 1; printf '\033[?25h'
clear; cat docs/img/controls.ansi;  read -rsk 1; printf '\033[?25h'
```

`read` blocks until a keypress, so the screenshot is taken with nothing emitted
after the frame; the trailing `printf` restores the cursor, which the frame
hides. The flags are zsh's; bash spells the same thing `read -rs -n1`, and
`read -r` waits for Enter in both.

Each frame parks the cursor on the terminal's last row, outside centered
overlays. This matters for `render_peek`, which otherwise leaves the cursor
inside the peek box while `read` waits.

The terminal must be at least 107×30, the size the frames are painted at. `clear`
first: a frame only paints its own rows, so a taller window would show stale
scrollback beneath it.
