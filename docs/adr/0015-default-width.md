# ADR-015: Default width

Status: Accepted

## Context

An omitted `--width` used to resolve to `Width::Percent(100)` — a full-width band
that tracks the real terminal, making gutter a transparent passthrough. Running
`gutter <cmd>` with no flags did nothing visible, which is a poor first-run
experience for a tool whose whole purpose is a narrower reading width.

## Decision

Default an omitted `--width` to `Width::Cols(100)` instead of `Width::Percent(100)`.
Resolved once through the existing `resolve_width()` (ADR-011), this gives a
100-column band, centred by the existing default `Layout`, clamped to the terminal
width on anything narrower than 100 columns — no special-casing, just the identity
+ clamp `Cols` already had.

Add `--width full` as a readable alias for `--width 100%` (`Width::Percent(100)`),
the explicit escape hatch back to the old tracking, full-width behaviour.

## Consequences

- An unspecified-width session is now absolute, not proportional: it no longer
  tracks the terminal on resize, it stays at 100 columns (clamped). This is a
  user-visible behaviour change from the previous default.
- `--width full` (and the pre-existing `--width 100%`) restores the old
  passthrough behaviour for anyone who wants it.
- `resolve_width()` and `margin()` are unchanged; only the input `Width` value an
  absent flag produces is different, so no new geometry code path is introduced.

## Code anchors

- `src/main.rs` — the `width_config` default in `run()`
- `src/cli.rs` — the `full` literal in `parse_width()`

See [ADR-011](0011-width-resolution.md) for the resolution mechanics this slots into.
