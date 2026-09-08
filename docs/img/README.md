# README screenshots

Each `.ansi` file is one dashboard frame rendered from a fabricated fleet.
Display it with `cat` to reproduce the live layout in a terminal.

| Frame | Shows |
| -- | -- |
| `home.ansi` | grouped by directory, the list outgrowing its region |
| `quickpeek.ansi` | grouped by state, peek open over a finished run |
| `groups.ansi` | grouped by custom group: five directories in one section, and one directory represented in two sections |
| `controls.ansi` | the `?` overlay over the dir-grouped dashboard |

The same 21 tasks are rendered in all four frames. Capture `attach.png` from
a live session while attached.

The fixture is `write_readme_screenshot_fixtures` in `src/app_readme_tests.rs`. Edit the
fleet there (task names, previews, ages, tags, selection), then regenerate:

```sh
cargo test -- --ignored write_readme_screenshot_fixtures
```

Every duration in the fixture is constant. With the same `$HOME`, identical bytes are written on
each run; that path is abbreviated to `~` in section labels.

## Capturing

```sh
clear; cat docs/img/home.ansi;      read -rsk 1; printf '\033[?25h'
clear; cat docs/img/quickpeek.ansi; read -rsk 1; printf '\033[?25h'
clear; cat docs/img/groups.ansi;    read -rsk 1; printf '\033[?25h'
clear; cat docs/img/controls.ansi;  read -rsk 1; printf '\033[?25h'
```

Use `read` to pause until a keypress, then take the screenshot with nothing
emitted after the frame. Restore the hidden cursor with the trailing `printf`.
The flags are zsh's; use `read -rs -n1` in bash, or `read -r` in either shell
to wait for Enter.

The cursor is placed on the terminal's last row, outside centered overlays.
Without this placement after `render_peek`, the cursor would be left inside
the peek box during `read`.

Use a terminal at least 107×30, the frame size. Run `clear` first: only the
frame's rows are painted, leaving stale scrollback visible beneath them in a
taller window.
