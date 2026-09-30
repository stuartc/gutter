# ADR-011: Width resolution

Status: Accepted

## Context

`--width` takes either an absolute column count (`--width 80`) or a proportion of the
real terminal (`--width 50pct`, `50%`, or `full` for 100%). The two behave
differently when the terminal resizes, and both the band width `W` and the left
margin depend on the real terminal width, so they have to be recomputed together.

## Decision

Resolve `W` through one pure function, `geometry::resolve_width()`, called at startup
and again on every resize:

- `Width::Cols(N)` resolves to `N`, clamped to the real width and to at least 1. It
  has no `MIN_W` floor, so `--width 10` is a legal 10-column band.
- `Width::Percent(P)` resolves to `real_cols * P / 100`, floored at `MIN_W = 20` and
  capped at `real_cols`.

The child is always told it owns `W` columns: its PTY and the parser are both sized
to `W`. The real terminal width is used only for the margin (`geometry::margin()`)
and the proportional recompute.

## Consequences

- An absolute width holds at `N` across resizes, except that a terminal narrower than
  `N` clamps the band to the terminal until it grows back.
- A proportional width tracks the terminal.
- With one function doing the resolution, the margin and width cannot drift apart,
  and the PTY size and the parser size always agree.
- An omitted `--width` is `Width::Cols(100)` ([ADR-015](0015-default-width.md)), and
  resize mode steps a `Width` in its own unit before it comes back through here
  ([ADR-016](0016-modal-resize.md)).

## Code anchors

- `src/geometry.rs` — `resolve_width()`, `MIN_W`, `margin()`
- `src/cli.rs` — `parse_width()`, which accepts `N`, `Npct`, `N%` and `full`
- `src/render.rs` — `handle_resize`, the recompute on every resize

The resize ordering this slots into is [ADR-008](0008-resize-ordering.md).
