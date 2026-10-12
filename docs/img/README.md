# README screenshots

Each `.ansi` file is one dashboard frame rendered from a fabricated fleet.
Display it with `cat` to reproduce the live layout in a terminal.

| Frame | Shows |
| -- | -- |
| `home.ansi` | grouped by directory, the list outgrowing its region |
| `quickpeek.ansi` | grouped by state, peek open over a finished run |
| `groups.ansi` | grouped by custom group: five directories in one section, and one directory represented in two sections |
| `controls.ansi` | the `?` overlay over the dir-grouped dashboard |
| `agent.ansi` | the spawn prompt's Agent page (`n`, then `Tab`) over the dir-grouped dashboard, every supported agent installed |

The same 21 tasks are rendered in all five frames. Capture `attach.png` from
a live session while attached.

The fixture is `write_readme_screenshot_fixtures` in `src/app_readme_tests.rs`. Edit the
fleet there (task names, previews, ages, tags, selection), then regenerate:

```sh
cargo test -- --ignored write_readme_screenshot_fixtures
```

Every duration in the fixture is constant, so repeated runs with the same `$HOME`
write identical bytes. Section labels abbreviate that path to `~`.

## Capturing

```sh
clear; cat docs/img/home.ansi;      read -rsk 1; printf '\033[?25h'
clear; cat docs/img/quickpeek.ansi; read -rsk 1; printf '\033[?25h'
clear; cat docs/img/groups.ansi;    read -rsk 1; printf '\033[?25h'
clear; cat docs/img/controls.ansi;  read -rsk 1; printf '\033[?25h'
clear; cat docs/img/agent.ansi;     read -rsk 1; printf '\033[?25h'
```

`read` pauses until a keypress, leaving the frame on screen without further output.
Take the screenshot during this pause, then press a key so the trailing `printf`
restores the hidden cursor.
The flags are zsh's; use `read -rs -n1` in bash, or `read -r` in either shell
to wait for Enter.

The cursor is placed on the terminal's last row, outside centered overlays,
then hidden. Without this placement after `render_peek`, the cursor would be
left inside the peek box during `read`; without hiding it, the Agent page's
filter caret would leave a visible cursor cell in the parked position.

Use a terminal at least 107×30, the frame size. Run `clear` first: only the
frame's rows are painted, leaving stale scrollback visible beneath them in a
taller window.

## Compressing

A window capture is a 32-bit RGBA PNG of about 550 KB with color-profile and
EXIF chunks. Quantize it to a 256-color palette and strip the metadata:

```sh
pngquant --force --ext .png --strip 256 docs/img/<frame>.png
```

The result is about 200 KB and matches the committed files' format.
