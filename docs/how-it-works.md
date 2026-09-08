# How it works

## One PTY per command

Each command is executed in its own pseudo-terminal, emulated with `alacritty_terminal`. Cursor-position and device-attribute queries are answered, `?2026` synchronized updates are rendered as whole frames, and history is reflowed after a resize. The same emulated screen grid is rendered in the dashboard preview, peek overlay, and attached view: terminal state is preserved across views, including for full-screen programs such as `vim` and `htop`. On backgrounding, client focus is changed without notifying the child.

## Input fidelity

Attached input is encoded according to the child's reported terminal modes. Modified Enter is encoded as `ESC CR` when the modifier is reported. Bracketed-paste markers are added only when enabled by the child. Mouse events are forwarded only under a requested mouse protocol. Alternate-scroll input is forwarded to full-screen children only with DECSET 1007 enabled; otherwise wheel events are suppressed. By default, 2,000 lines of scrollback are retained per task, with depth configurable at supervisor startup. For inline children, scroll up or press `Shift+PageUp` to enter history. Use paging keys to navigate, then `Esc` or ordinary input to return to live output. See [`commands.md`](commands.md) for exact routing rules.

## One activity window for grouping

Group tasks by state (In use / Running / Idle / Completed), working directory, or names assigned with `g`. After more than 10 s without output, a running task is classified as Idle: its glyph is changed from `✻` to `∙` and, under state grouping, it is moved to Idle in the same refresh. Row order is unchanged within directory or custom sections. With output every 1–2 s, as from `top`, a task is kept under Running and marked `✻`.

Preview text is resolved independently. After losing a stronger source, the previous preview is retained for 600 ms before a weaker source is rendered, avoiding repaint flicker without changing grouping. Claude's on-disk `waiting` status and screen-derived agent statuses are both classified as top-tier `anchor` sources. The on-disk status is preferred when both are present. When only the screen-derived status is available again, it is rendered immediately because the tier is unchanged.
