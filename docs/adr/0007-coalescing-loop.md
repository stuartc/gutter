# ADR-007: 60fps coalescing loop

Status: Accepted

## Context

Under a multi-MB PTY flood, `recv_timeout()` never actually times out — there is
always another queued message — so a loop that waits for a timeout to repaint would
starve rendering completely.

## Decision

Run the render loop as a fixed-deadline ~60fps coalescer. Each frame: block for the
first message, handle it, then capture `deadline = now + FRAME` (16ms) once, drain
messages until `now >= deadline`, and render once. The explicit `now >= deadline`
check is mandatory — it is the only thing that breaks the drain under a saturating
burst.

## Consequences

- The `now >= deadline` check cannot be removed or replaced with a timeout: a
  message can arrive at or past the deadline and the loop must still exit to paint.
- Exactly one render per frame, however many messages the frame drained.
- With nothing pending, the loop parks in a single blocking `recv()` and costs no
  CPU while idle. When the ESC-hold ([ADR-020](0020-raw-input-passthrough.md)) or
  resize mode's idle timer ([ADR-016](0016-modal-resize.md)) is armed, that first
  wait is bounded by the nearer of the two deadlines instead, so a quiet child still
  wakes the loop for the flush or the auto-exit.
- The PTY reader is throttled upstream by a bounded `sync_channel`
  (`pty::STAGING_DEPTH`, 64 chunks), so the flood has somewhere to back up.
- The clock is injectable (the `Clock` trait owns the receiver as well as the time),
  so the cap, starvation, idle-park and input-liveness tests run on virtual time with
  no wall-clock sleeps.

## Code anchors

- `src/render.rs` — the frame loop and the `FRAME` budget; the `VirtualClock` test
  double and the loop tests (`cap_and_completeness`,
  `starvation_regression_renders_still_fire`, `idle_park_zero_renders_zero_wakeups`,
  `input_liveness_under_load`)
- `src/clock.rs` — the `Clock` trait and the production `RealClock`

The unbounded merge that keeps input admissible is
[ADR-009](0009-merged-unbounded-channel.md).
