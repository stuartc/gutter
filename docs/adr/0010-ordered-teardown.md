# ADR-010: Explicit ordered teardown

Status: Accepted

## Context

We exit via `process::exit`, so no destructors run. Every terminal state we set up
has to be torn down by hand, and some steps depend on order — leave the alt screen
after disabling raw mode and the terminal is left corrupted.

## Decision

Restore explicitly, in order, before exiting: leave the alt screen (or hand back
inline) → undo whatever input modes gutter set on the child's behalf → disable
mouse → show cursor → disable raw mode. Each step is conditional on what was
actually set up.

## Consequences

- All cleanup is in one teardown path; nothing relies on `Drop`.
- The alt-leave only fires if the child exited in the alt screen. A plain command's
  output is left on the primary screen.
- gutter never resets a mode it did not set. Under raw passthrough (ADR-020) it
  sets no keyboard mode of its own at all, so that slot is currently empty; the
  rule is what keeps it honest when the child's own requests start being relayed.
- Disabling mouse, showing the cursor, and disabling raw mode are always safe to
  call unconditionally.

## Code anchors

- `src/render.rs` — the ordered teardown path
- `src/terminal.rs` — the `OuterTerminal` restore methods and an order-asserting mock

The inline hand-back is [ADR-013](0013-inline-anchor-scroll-paint.md); the
screen-mode mirroring it depends on is
[ADR-012](0012-screen-mode-mirroring.md).

The suspend/resume cycle's park path ([ADR-019](0019-suspend-resume-cycle-ordering.md))
reuses this exact order — minus the exit-status line — to hand the terminal back to
the shell on Ctrl-Z, and its resume path runs the order in reverse to take it back.
