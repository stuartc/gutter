# ADR-010: Explicit ordered teardown

Status: Accepted

## Context

gutter exits via `process::exit`, so no destructors run. Every terminal state it set
up has to be torn down by hand, and some steps depend on order — disable raw mode
before leaving the alt screen and the terminal is left corrupted.

## Decision

Restore explicitly, in this order, before exiting:

1. leave the alt screen, or hand back inline (ADR-013);
2. end the attribute run (`CSI 0 m`);
3. reset the cursor shape (`CSI 0 SP q`);
4. turn off the input modes gutter mirrored for the child (ADR-022);
5. undo the keyboard modes gutter relayed for the child (ADR-021);
6. disable mouse capture (ADR-005);
7. turn autowrap back on (ADR-014);
8. show the cursor, which flushes the buffered sink;
9. disable raw mode, **last**.

Each step runs only if what it undoes was actually set up. Every step is attempted
whatever an earlier one returned; the first error is kept and returned. The order is
fixed and must not change — it is what makes the restore correct when everything
succeeds, which is every run but the pathological one.

## Consequences

- All cleanup is in one function, `ordered_restore` in `src/render.rs`, collecting
  into a `BestEffort`. Nothing relies on `Drop`.
- The alt-leave only fires if the child's screen was alt at exit. A plain command's
  output is left on the primary screen and handed back inline.
- gutter never resets a mode it did not set. It sets no keyboard mode of its own
  (ADR-020), so step 5 undoes only what the child asked the terminal for (ADR-021):
  the kitty stack pops back to the depth the child opened, and `CSI > 4 ; 0 m` turns
  modifyOtherKeys off, each only if gutter relayed it.
- Step 4 sits immediately before step 5 so that every input-encoding restore is
  together, with the coarsest last. The same conditional rule holds — a mode never
  mirrored on is never turned off, so a shell with its own paste protection keeps it.
- Mouse disable, autowrap on and show cursor are safe to issue whatever the terminal's
  state. Mouse disable still only writes if gutter enabled capture; autowrap on is
  unconditional, since gutter does not read the mode before turning it off (ADR-014).
- Steps 2 and 3 undo what the child's output left on the outer terminal. `CSI 0 m`
  because the band's last painted row leaves its own attributes live — and gutter
  always painted the band, so this reset is unconditional. `CSI 0 SP q` because a
  DECSCUSR the child emitted was mirrored outward. Without them the shell comes back
  tinted, or wearing the child's cursor. The cursor reset fires only when gutter
  actually *wrote* a shape, which is not the same as the child having asked for one: a
  DECSCUSR emitted while the resize overlay owns the cursor is recorded and never
  mirrored, and resetting on the request would replace the shape the user configured
  for their own shell with the default.
- `CSI 0 SP q` is what terminals that treat DECSCUSR as resettable — kitty, VTE,
  Ghostty, iTerm2 — read as "back to the configured shape". On xterm's own table
  `Ps = 0` is a blinking block, the same as `Ps = 1`. There is no portable "whatever it
  was before", so on those terminals a run whose child changed the shape hands back the
  block rather than the user's own. The reset is still worth having, because the
  alternative is handing back the child's shape on every terminal.
- Nothing logs the returned error: the exit path discards it, there being no terminal
  worth printing on when the restore itself is failing. The return value is for the
  tests, which assert which step broke.
- The sink is a `BufWriter`, so step 8's flush is what puts the restore on screen, and
  best-effort is what guarantees it is reached. Nothing writes to the sink after
  teardown returns. The `BufWriter` is a local of `run()`, so its drop-flush does still
  run before `main` reaches `process::exit` — but it swallows its result and lands the
  bytes after the restore has stopped controlling the order, so it is a backstop, not
  the mechanism (ADR-023).
- The suspend cycle's park (ADR-019) runs the same `ordered_restore`, with the inline
  hand-back always in its exit-0 shape (no `Exited with: N` line), then flushes before
  the self-stop. Its unpark re-takes the terminal with raw mode first, then turns
  autowrap off again.

## Why best-effort

Teardown used to propagate errors with `?`. That made the order fixed and the sequence
fragile: one failing step — a flush inside the alt-leave against a terminal that has
gone away — skipped every step after it, `disable_raw_mode` included, and handed the
user's shell back in raw mode. That is exactly the failure worth protecting against,
since it leaves the terminal unusable. Park already had to work this way —
`disable_raw_mode` must run before the self-stop or the shell gets a raw terminal — so
the two paths now share one shape and one step list.

## Code anchors

- `src/render.rs` — `ordered_restore`, `BestEffort`, `run_teardown`, `park`,
  `hand_back_inline`
- `src/terminal.rs` — the `OuterTerminal` restore methods and the order-asserting mock

The inline hand-back is [ADR-013](0013-inline-anchor-scroll-paint.md); the screen-mode
mirroring it depends on is [ADR-012](0012-screen-mode-mirroring.md). The suspend
cycle that reuses this order is [ADR-019](0019-suspend-resume-cycle-ordering.md).
