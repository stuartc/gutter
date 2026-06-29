# ADR-007: 60fps coalescing loop

Status: Accepted

## Context

Under a multi-MB PTY flood, `recv_timeout()` never actually times out — there is
always another queued message — so a loop that waits for a timeout to repaint would
starve rendering completely.

## Decision

Run the render loop as a fixed-deadline ~60fps coalescer. Each frame: block for the
first message and capture `deadline = now + 16ms` once, then drain messages until
`now >= deadline` and repaint. The explicit `now >= deadline` check is mandatory —
it is the only thing that breaks the drain under a saturating burst. One render per
frame; a single blocking `recv()` means zero idle CPU.

## Consequences

- The `now >= deadline` check cannot be removed or replaced with a timeout: a
  message can arrive at or past the deadline and the loop must still exit to paint.
- The PTY reader is throttled upstream by a bounded `sync_channel(64)`, so the flood
  has somewhere to back up.
- The clock is injectable (`Clock` trait), so the cap, starvation, idle-park and
  liveness tests run on virtual time with no wall-clock sleeps.

## Code anchors

- `src/render.rs` — the frame loop and the `FRAME = 16ms` budget
- `src/clock.rs` — the injectable clock and its virtual test double

The unbounded merge that keeps input admissible is
[ADR-009](0009-merged-unbounded-channel.md).
