# README screenshots

Each `.ansi` file is one dashboard frame written by the real renderer over a
fabricated fleet, so a terminal `cat`-ing it paints exactly what a live run
paints. Screenshot the frame, not a real fleet.

| Frame | Shows |
| -- | -- |
| `home.ansi` | grouped by directory, the list outgrowing its region |
| `quickpeek.ansi` | grouped by state, peek open over a finished run |
| `groups.ansi` | grouped by custom group: one section spans five directories, and one directory feeds two sections |
| `controls.ansi` | the `?` overlay over the dir-grouped dashboard |

All four render the same 21 tasks. `attach.png` is captured from a live session,
since it presupposes one.

The fixture is `write_readme_screenshot_fixtures` in `src/app_tests.rs`. Edit the
fleet there — task names, previews, ages, tags, selection — then regenerate:

```sh
cargo test -- --ignored write_readme_screenshot_fixtures
```

Every duration in the fixture is a constant, so two runs write identical bytes.
`$HOME` is the one environment input: the section labels come from abbreviating
it to `~`.

## Capturing

```sh
clear; cat docs/img/home.ansi; read -rsk 1; printf '\033[?25h'
```

`read` blocks until a keypress, so the screenshot is taken with nothing emitted
after the frame; the trailing `printf` restores the cursor, which the frame
hides. The flags are zsh's — bash spells the same thing `read -rs -n1`, and
`read -r` waits for Enter in both.

Each frame ends by parking the cursor on its last row. Without that,
`render_peek` would leave it inside the peek box and a shell prompt would print
through the middle of the capture.

The terminal must be at least 107×30, the size the frames are painted at. `clear`
first: a frame only paints its own rows, so a taller window would show stale
scrollback beneath it.
