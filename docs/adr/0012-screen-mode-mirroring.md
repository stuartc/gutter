# ADR-012: Screen-mode mirroring

Status: Accepted

## Context

Forcing the alt screen at startup would bury the user's scrollback the moment gutter
launches, and a plain command like `ls` would blast into the alt screen instead of
leaving its output behind. The child should decide; gutter should follow.

## Decision

Do not force the alt screen. Read the child's `alternate_screen()` flag each frame
and edge-trigger the outer alt screen to match — enter or leave only when the child's
mode actually changes. A plain command stays on the primary screen, so its output
survives gutter's exit.

## Consequences

- Startup (raw mode, the anchor CPR, mouse capture) all happens on the primary screen;
  the child then drives the mode.
- `outer_alt_active` is an edge-trigger flag, mirrored only on change.
- An alt→primary edge forces a full repaint, because the cached alt frame is not a
  valid diff baseline for the primary screen.
- While in alt, the band paints at offset 0 and `base_row` is frozen.

## Code anchors

- `src/main.rs` — no forced alt at startup
- `src/render.rs` — the per-frame mode mirror and baseline reset
- `src/terminal.rs` — the outer alt-screen enter/leave

Teardown's conditional alt-leave is [ADR-010](0010-ordered-teardown.md); the inline
path for the primary screen is [ADR-013](0013-inline-anchor-scroll-paint.md).
