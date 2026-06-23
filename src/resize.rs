//! Resize handling, run entirely on Thread 2 (the parser is never resized from
//! any other thread). On `Event::Resize`, in order:
//!
//! 0. If `--width` is proportional, recompute `W = resolve_width(pct, new_cols)`.
//! 1. `master.resize(PtySize { cols: W, rows, .. })` FIRST — the kernel sends
//!    SIGWINCH to the child's foreground group.
//! 2. `parser.screen_mut().set_size(rows, W)` immediately, same turn (no
//!    old-W drain — feeding old-width bytes into the resized grid is safe:
//!    clamp + wrap-flag reset, no panic).
//! 3. Full repaint of `[margin, margin+W)` plus the gutter clear; recompute
//!    `left_margin`. See ADR-008/011.
