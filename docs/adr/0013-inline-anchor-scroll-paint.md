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

## Amendment — the CPR query travels over the terminal that answers it

The probe used to write `ESC [ 6 n` to stdout and read the `ESC [ row ; col R` answer
back off `/dev/tty`: a question posed on one handle and an answer expected on another.
That worked only because the two usually happened to be the same terminal. With stdout
redirected the question went into the file, nothing could ever answer, and the anchor
fell back to `rows - 1` a full `CPR_TIMEOUT` later — 100 ms of dead startup on every
run of `gutter cmd > log`.

Since [ADR-023](0023-controlling-terminal-fd-model.md) the query goes out through the
band's own sink, so query and reply are the same terminal by construction. The "no tty
at all" arm of the capture is gone with it: startup has already refused a run with no
terminal to paint on, so the only fallback left is the one that always mattered — the
terminal did not answer in time, and the anchor takes `rows - 1`. Never row 0, which is
the overpaint this record exists to prevent.

## Code anchors

- `src/render.rs` — the anchor, make-room scroll, scroll emit, and hand-back; the
  scroll-tracker field
- `src/anchor.rs` — the hand-rolled probe and the input handle it reads the reply from
- `src/main.rs` — the CPR anchor capture and the `GUTTER_FORCE_ANCHOR_ROW` override

The teardown ordering is [ADR-010](0010-ordered-teardown.md); the screen-mode
mirroring is [ADR-012](0012-screen-mode-mirroring.md). The terminal the probe asks,
and the handle it asks on, are [ADR-023](0023-controlling-terminal-fd-model.md).
