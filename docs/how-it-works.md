# How it works

## One PTY per command

Every task runs in its own pseudo-terminal, emulated with `alacritty_terminal`. `fleetcom` answers cursor-position and device-attribute queries, renders `?2026` synchronized updates as whole frames, and reflows history after a resize. The dashboard preview, peek overlay, and attached view all read the same emulated screen grid, so full-screen programs such as `vim` and `htop` retain one consistent terminal state across views. Backgrounding changes client focus without notifying the child.

## Input fidelity

Attached input follows the terminal modes reported by the child. Modified Enter becomes `ESC CR` when the terminal reports the modifier. Paste receives bracketed-paste markers only when the child enables them. Mouse events go to children that request a mouse protocol. Full-screen children receive alternate-scroll input only while DECSET 1007 is enabled; otherwise `fleetcom` suppresses wheel events. Each task retains 2,000 lines of scrollback. For inline children, wheel-up or `Shift+PageUp` enters history; paging keys navigate it, while `Esc` or ordinary input returns to live output. [`commands.md`](commands.md) documents the exact routing rules.

## Grouping uses two activity windows

The dashboard groups tasks by state (In use / Running / Idle / Completed), working directory, or names assigned with `g`. Activity uses two separate windows. The row glyph changes from `✻` to `∙` after 600 ms without output, while state grouping moves the task to Idle after 10 s. A tool such as `top`, which prints every 1–2 s, can therefore alternate glyphs without moving sections. The glyph reports the short edge; the section reports the debounced state.
