# How it works

## One PTY per command

`fleetcom` executes each command in its own pseudo-terminal, emulated with `alacritty_terminal`. The emulator answers cursor-position and device-attribute queries, renders `?2026` synchronized updates as whole frames, and reflows history after a resize. The dashboard preview, peek overlay, and attached view all read the same emulated screen grid, so terminal state survives changes between views, including for full-screen programs such as `vim` and `htop`. Backgrounding changes client focus without notifying the child.

## Input fidelity

The child's reported terminal modes determine how `fleetcom` encodes attached input. It encodes modified Enter as `ESC CR` when the terminal reports the modifier, adds bracketed-paste markers only when the child enables them, and forwards mouse events only under a requested mouse protocol. Alternate-scroll input reaches full-screen children only with DECSET 1007 enabled; otherwise, wheel events are suppressed.

Each task retains 2,000 lines of scrollback by default; you can configure the depth at supervisor startup. For inline children, scroll up or press `Shift+PageUp` to enter history. Use paging keys to navigate, then `Esc` or ordinary input to return to live output. See [`commands.md`](commands.md) for exact routing rules.

## One activity window for grouping

Group tasks by state (In use / Running / Idle / Completed), working directory, or names assigned with `g`. After 10 s without PTY output, Fleetcom marks a running task Idle, changes its glyph from `✻` to `∙`, and moves its row under Idle on the same state-grouped refresh. Fleetcom preserves row order within directory and custom sections. When Fleetcom receives output every 1–2 s from a task such as `top`, it keeps that task under Running with `✻`.

An agent may keep working while its PTY is quiet. Fleetcom reads the agent CLI's spinner row or title frame to identify an active turn, then shows `●` regardless of output timing. Fleetcom applies the same signal to hand-typed commands and Agent-page launches.

Preview text is resolved independently of activity grouping. When a stronger source disappears, the resolver retains the previous preview for 600 ms before displaying a weaker source. This avoids repaint flicker without changing grouping. Claude's on-disk `waiting` status and screen-derived agent statuses both belong to the top `anchor` tier, with the on-disk status taking precedence when both are present. If only the screen-derived status remains, the resolver displays it immediately because the tier has not changed.
