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
- `outer_alt_active` records what the outer terminal was last told, and an enter or
  leave is written only when the child's flag differs from it.
- An alt→primary edge forces a full repaint, because the cached alt frame is not a
  valid diff baseline for the primary screen.
- While in alt, the band paints at offset 0 and the make-room scroll leaves
  `base_row` alone.
- Teardown reads `outer_alt_active` to choose between leaving the alt screen and the
  inline hand-back, so it must reflect the child's final bytes; the shutdown drain in
  [ADR-013](0013-inline-anchor-scroll-paint.md) makes sure it does.

## Code anchors

- `src/main.rs` — no forced alt at startup
- `src/render.rs` — the per-frame mode mirror at the top of `render_once`, and its
  baseline reset
- `src/terminal.rs` — `enter_alt_screen` / `leave_alt_screen`

Teardown's conditional alt-leave is [ADR-010](0010-ordered-teardown.md); leaving and
re-entering the alt screen across a suspend is
[ADR-019](0019-suspend-resume-cycle-ordering.md); the inline path for the primary
screen is [ADR-013](0013-inline-anchor-scroll-paint.md).
