# ADR-011: Width resolution

Status: Accepted

## Context

`--width` accepts two kinds of value: an absolute column count and a proportion of
the real terminal. They behave differently on resize, and the band width and left
margin both depend on the real terminal width, so they have to be recomputed
together.

## Decision

Resolve `W` through one function, `geometry::resolve_width()`, called once at
startup and again on every resize. An absolute `Cols(N)` is identity (clamped to the
real width) and so a no-op on resize. A proportional `Percent(P)` is
`real_cols * P / 100`, floored at `MIN_W = 20` and capped at `real_cols`, recomputed
each time. The child is always told it owns `W` columns; the real terminal width is
used only for the margin and the proportional recompute.

## Consequences

- Absolute width stays fixed for the session; proportional width tracks the terminal.
- `resolve_width()` is the single source of truth, so the margin and width never
  drift apart.
- The child never sees a width that disagrees between its PTY size and the parser.

## Code anchors

- `src/geometry.rs` — `resolve_width()`, the floor/cap, the pure layout maths
- `src/cli.rs` — parsing the two `--width` forms
- `src/render.rs` — the recompute on resize

The resize ordering this slots into is [ADR-008](0008-resize-ordering.md).
