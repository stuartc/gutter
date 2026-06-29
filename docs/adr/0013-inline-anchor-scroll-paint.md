# ADR-013: Inline anchor, scroll-aware paint, and hand-back

Status: Accepted

## Context

On the primary screen, a plain command should grow downward from where gutter was
launched, not overwrite the scrollback above it, and lines that scroll off the top
should land in the terminal's own scrollback rather than a buffer of our own. When
gutter exits, the shell prompt should resume below the command's output.

## Decision

Anchor the band at `base_row`, the launch cursor row captured by a CPR query before
the input threads start. As the band grows past the bottom of the screen, scroll the
real terminal up to make room and let `base_row` fall monotonically toward 0. Once it
reaches 0 the band fills the screen, and lines that depart the top are emitted to the
terminal's scrollback. A second vt100 parser with bounded scrollback — the scroll
tracker — records which lines left the top of the `W`-window each frame, since the
live parser (scrollback 0) can't reconstruct that once a burst scrolls past a
screenful in one frame. On exit, a child still in the alt screen takes the normal
alt-leave; a child on the primary screen that ever painted inline gets a hand-back —
the cursor drops to a fresh line below the band, with a dim status line on a non-zero
exit. A TUI that went straight to alt and never painted inline leaves nothing behind.

## Consequences

- `base_row` starts at the launch row and only ever decreases (via make-room scroll).
- The scroll tracker is a per-frame detection device, reset to the live grid every
  frame, so it holds at most one frame's advance and the live parser stays at
  scrollback 0.
- The hand-back fires only when `ever_painted_inline` is set, so a straight-to-alt
  TUI leaves no stray status line.
- The status line prints only on a non-zero exit.

## Code anchors

- `src/render.rs` — the anchor, make-room scroll, scroll emit, and hand-back; the
  scroll-tracker field
- `src/main.rs` — the CPR anchor capture

The teardown ordering is [ADR-010](0010-ordered-teardown.md); the screen-mode
mirroring is [ADR-012](0012-screen-mode-mirroring.md).
