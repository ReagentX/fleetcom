# How it works

## One PTY per command

Every task runs in its own pseudo-terminal, emulated with `alacritty_terminal`. `fleetcom` answers cursor-position and device-attribute queries, renders `?2026` synchronized updates as whole frames, and reflows history after a resize. The dashboard preview, peek overlay, and attached view all read the same emulated screen grid, so full-screen programs such as `vim` and `htop` retain one consistent terminal state across views. Backgrounding changes client focus without notifying the child.

## Input fidelity

Attached input follows the terminal modes reported by the child. Modified Enter becomes `ESC CR` when the terminal reports the modifier. Paste receives bracketed-paste markers only when the child enables them. Mouse events go to children that request a mouse protocol. Full-screen children receive alternate-scroll input only while DECSET 1007 is enabled; otherwise `fleetcom` suppresses wheel events. Each task retains 2,000 lines of scrollback by default; the supervisor can set another depth at startup. For inline children, wheel-up or `Shift+PageUp` enters history; paging keys navigate it, while `Esc` or ordinary input returns to live output. [`commands.md`](commands.md) documents the exact routing rules.

## Grouping follows one activity window

The dashboard groups tasks by state (In use / Running / Idle / Completed), working directory, or names assigned with `g`. One 10-second window drives both idle signals: after 10 s without output, the row glyph changes from `✻` to `∙` and, under state grouping, the task moves to the Idle section in the same refresh. Idle state does not affect row order within directory or custom sections. A tool such as `top`, which prints every 1–2 s, never crosses the window, so it stays `✻` under Running. Preview text is independent: a status from a weaker source must persist for 600 ms before it replaces a stronger one, which absorbs repaint flicker without affecting grouping. The strongest source is not the screen at all. When a supported agent CLI publishes a live session record saying it is blocked on the user — `claude` does, and only that state is read — the preview takes the CLI's own word for it, above anything scraped from the terminal: the record changes before the dialog finishes painting and says the same thing at every terminal width.
