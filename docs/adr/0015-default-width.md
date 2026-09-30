# ADR-015: Default width

Status: Accepted

## Context

With no `--width`, gutter used to resolve to `Width::Percent(100)`: a full-width band
that tracks the terminal, which makes gutter a transparent passthrough. Running
`gutter <cmd>` with no flags changed nothing on screen — a poor first run for a tool
whose whole purpose is a narrower reading width.

## Decision

An omitted `--width` defaults to `Width::Cols(100)`. It goes through
`resolve_width()` ([ADR-011](0011-width-resolution.md)) like any other absolute
width: a 100-column band, centred by the default layout, clamped to the terminal on
anything narrower than 100 columns. No special case is needed.

`--width full` is a readable alias for `--width 100%` (`Width::Percent(100)`): the
explicit way back to a full-width band that tracks the terminal.

## Consequences

- A session with no `--width` is absolute, not proportional. It stays at 100 columns
  across resizes (clamped on narrower terminals) instead of tracking the terminal —
  a user-visible change from the earlier default.
- `--width full` or `--width 100%` gives the full-width passthrough.
- `resolve_width()` and `margin()` are unchanged; only the value an absent flag
  produces is different.

## Code anchors

- `src/main.rs` — the `width_config` default in `run()`
- `src/cli.rs` — the `full` literal in `parse_width()`
