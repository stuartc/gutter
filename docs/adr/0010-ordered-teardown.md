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
- gutter never resets a mode it did not set. It sets no keyboard mode of its own
  (ADR-020), so what that slot undoes is what the child asked the terminal for
  (ADR-021): the kitty stack pops back to the depth the child opened and
  `CSI > 4 ; 0 m` turns modifyOtherKeys off, both only if gutter relayed them.
- The slot has two steps, mirrored modes first: the input modes gutter mirrored on the
  child's behalf (ADR-022 — DECCKM, application keypad, bracketed paste) go off
  immediately before the keyboard-mode reset, keeping every input-encoding restore
  together with the coarsest last. Same conditional rule — a mode never mirrored on is
  never turned off, so a shell with its own paste protection keeps it.
- Disabling mouse, showing the cursor, and disabling raw mode are always safe to
  call unconditionally.

## Amendment — the order is fixed, the steps are best-effort

Teardown used to propagate with `?`, which made the order fixed *and* the sequence
fragile: one failing step — a flush inside the alt-leave against a terminal that has
gone away — skipped every step after it, `disable_raw_mode` included, and handed the
user's shell back in raw mode. The failure worth protecting against is exactly the one
that leaves the terminal unusable.

So every restore step is attempted regardless of what an earlier one returned, and the
first error is kept and returned for the caller to log. The order does not change and
must not: it is what makes the restore correct when everything succeeds, which is every
run but the pathological one. Only the error handling is different.

The park half of the suspend/resume cycle ([ADR-019](0019-suspend-resume-cycle-ordering.md))
already worked this way — `disable_raw_mode` has to run before the self-stop or the shell
gets a raw terminal — so the two paths now share one shape (`BestEffort` in
`src/render.rs`) rather than disagreeing.

The exit path leans on this for its bytes too: `show_cursor` flushes, so reaching it
unconditionally is what lands the hand-back line and anything else the restore queued.
Nothing writes to the sink after teardown returns, and `process::exit` runs no destructor
that would land a straggler.

## Code anchors

- `src/render.rs` — the ordered teardown path
- `src/terminal.rs` — the `OuterTerminal` restore methods and an order-asserting mock

The inline hand-back is [ADR-013](0013-inline-anchor-scroll-paint.md); the
screen-mode mirroring it depends on is
[ADR-012](0012-screen-mode-mirroring.md).

The suspend/resume cycle's park path ([ADR-019](0019-suspend-resume-cycle-ordering.md))
reuses this exact order — minus the exit-status line — to hand the terminal back to
the shell on Ctrl-Z, and its resume path runs the order in reverse to take it back.
