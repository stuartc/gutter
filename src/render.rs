//! Thread 2 — the render loop. Owns the `vt100::Parser` and the outer terminal
//! handle. Blocks on a single `recv()` over the merged `Msg` channel (one park
//! point, zero idle CPU), drains to a fixed 16ms deadline (the mandatory
//! `now >= deadline` burst-exit break), greedily swallows with `try_iter`,
//! then renders once. The ONLY thread that writes the PTY master and the ONLY
//! thread that touches the outer terminal for output/teardown.
//!
//! Render path: `rows_diff(prev, 0, W)` with our own `MoveTo(left_margin, row)`
//! per row; reposition the cursor inside the band after each repaint. See
//! ADR-006 (wide chars) and ADR-007 (the coalescing loop).
