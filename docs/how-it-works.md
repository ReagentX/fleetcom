# How it works

## One PTY per command

`fleetcom` executes each command in its own pseudo-terminal, emulated with `alacritty_terminal`. The emulator answers cursor-position and device-attribute queries, renders `?2026` synchronized updates as whole frames, and reflows history after a resize. The dashboard preview, peek overlay, and attached view all read the same emulated screen grid, so terminal state survives changes between views, including for full-screen programs such as `vim` and `htop`. Backgrounding changes client focus without notifying the child.

## Input fidelity

The child's reported terminal modes determine how `fleetcom` encodes attached input. It encodes modified Enter as `ESC CR` when the terminal reports the modifier, adds bracketed-paste markers only when the child enables them, and forwards mouse events only under a requested mouse protocol. Alternate-scroll input reaches full-screen children only with DECSET 1007 enabled; otherwise, wheel events are suppressed.

Each task retains 2,000 lines of scrollback by default; you can configure the depth at supervisor startup. For inline children, scroll up or press `Shift+PageUp` to enter history. Use paging keys to navigate, then `Esc` or ordinary input to return to live output. See [`commands.md`](commands.md) for exact routing rules.

## One activity window for grouping

Group tasks by state (In use / Running / Idle / Completed), working directory, or names assigned with `g`. After more than 10 s without output, a running task becomes Idle: its glyph changes from `✻` to `∙` and, under state grouping, its row moves to Idle in the same refresh. Row order stays unchanged within directory or custom sections. A task producing output every 1–2 s, such as `top`, stays under Running with the `✻` glyph.

Preview text is resolved independently of activity grouping. When a stronger source disappears, the resolver retains the previous preview for 600 ms before displaying a weaker source. This avoids repaint flicker without changing grouping. Claude's on-disk `waiting` status and screen-derived agent statuses both belong to the top `anchor` tier, with the on-disk status taking precedence when both are present. If only the screen-derived status remains, the resolver displays it immediately because the tier has not changed.
