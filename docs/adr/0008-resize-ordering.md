# ADR-008: Resize ordering

Status: Accepted

## Context

A resize has to change two sizes: the child's, through `TIOCSWINSZ` on the PTY
(which makes the kernel send the child `SIGWINCH`), and the parser's grid. The
child's redraw comes back as ordinary PTY bytes on the merged channel. If the two
sizes are changed at different moments, bytes the child wrote for its new size can
be parsed into a grid of the old one.

## Decision

Handle `Msg::Resize` on the render thread (the only parser owner), reading the real
size there, and do the whole change in one turn in strict order:

1. Recompute `W` from the new real width: a proportional width scales with it, and
   an absolute width is clamped to it when the terminal is now narrower than `N`.
2. `resizer.resize(W, rows)` — `TIOCSWINSZ` to the child *first*.
3. `parser.set_size(rows, W)` — note the `(rows, cols)` argument order. The scroll
   tracker ([ADR-013](0013-inline-anchor-scroll-paint.md)) is resized alongside.
4. Recompute the margin.
5. Reset the diff baseline and clear the gutter. This step originally cleared the
   gutter on the alt screen only; [ADR-017](0017-uniform-margin-management.md)
   supersedes that part and clears the band's row span on both screen modes.

Because no other message is handled until the turn ends, the parser is at the size
the child was told before any byte the child writes in reply is processed.

## Consequences

- `resizer.resize()` runs before `set_size()`, and `PtyResizer` is always told the
  band width `W`, never the real width. Both live in one helper,
  `resize_pty_then_grids`, which `handle_resize` and `apply_resize_step` share.
  `ordering_pty_resize_precedes_set_size` enforces the order: its resizer panics,
  and the parser and scroll tracker must still be at the old size. That pins source
  order within one dispatch turn, which nothing inside the turn can observe; the
  promise the rest of gutter relies on is that both sizes have changed before the
  next message is handled. `ordering_master_resize_then_set_size` checks the PTY
  was told `W` and the parser ended at `(rows, W)`.
- `set_size(rows, cols)` takes its arguments in the opposite order to the PTY size
  `(cols, rows)` — the transposition is an easy copy-paste trap, hence the explicit
  note.
- After `set_size`, the grid is internally consistent (`COLUMNS == W`, cursor in
  bounds) even before any new child bytes arrive. Bytes the child wrote before it saw
  the resize land in the new grid degraded but never out of bounds.
- The same order is reused by resize mode's width step
  ([ADR-016](0016-modal-resize.md)) and by the size catch-up after a resume
  ([ADR-019](0019-suspend-resume-cycle-ordering.md)).

## Code anchors

- `src/render.rs` — `handle_resize`, `apply_resize_step`, `resize_pty_then_grids`,
  and the `resize` test module (`ordering_pty_resize_precedes_set_size`,
  `ordering_master_resize_then_set_size`, the `mid_burst_case_*` tests)
- `src/pty.rs` — the `PtyResizer` trait and the production `MasterResizer`

Proportional recompute is [ADR-011](0011-width-resolution.md).
