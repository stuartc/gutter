# ADR-008: Resize ordering

Status: Accepted

## Context

On resize, the kernel sends SIGWINCH to the child's foreground process group. If we
update our parser's grid before telling the child its new size, the child's response
can arrive before the parser is ready for it.

## Decision

Handle `Event::Resize` on the render thread (the only parser owner) in strict order:

1. Recompute `W` (a no-op for an absolute width, a recompute for proportional).
2. `master.resize(W, rows)` — TIOCSWINSZ to the child *first*.
3. `parser.set_size(rows, W)` — note the `(rows, cols)` argument order.
4. Recompute the margin.
5. Reset the diff baseline and clear the gutter (alt screen only — superseded in
   part by [ADR-017](0017-uniform-margin-management.md), which generalises this
   clear to a row-span on both screen modes).

## Consequences

- `master.resize()` must run before `set_size()`. A recording mock asserts the
  order.
- `set_size(rows, cols)` takes its arguments in the opposite order to the PTY size
  `(cols, rows)` — the transposition is an easy copy-paste trap, hence the explicit
  note.
- After `set_size`, the grid is internally consistent (`COLUMNS == W`, cursor in
  bounds) even before any new child bytes arrive.

## Code anchors

- `src/render.rs` — the resize handler and its ordering gate
- `src/pty.rs` — the `PtyResizer` trait (always told `W`, never the real width)

Proportional recompute is [ADR-011](0011-width-resolution.md).
