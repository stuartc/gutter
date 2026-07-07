# ADR-015: Modal resize

Status: Accepted

## Context

gutter's whole purpose is a comfortable reading width, but until now that width was
fixed at launch. PRD 0001 (Feature 2) asks for a live resize interaction without
teaching the child anything about it — and without gutter ever swallowing a keystroke
in normal operation, which it has never done before.

## Decision

Reserve exactly one chord (`--resize-key`, default Ctrl-\) to toggle a modal resize
state. The chord — and, while in the mode, every key — is classified and intercepted
in the render thread's key-dispatch arm *before* `encode_key` runs (ADR-002), so the
child never sees it. In mode, `h`/`l` (aliases `←`/`→`, `-`/`+`) step ±1 unit, `H`/`L`
step ±10, `Esc` or the chord again exits, and an unrecognised key is swallowed —
consumed but not forwarded — so a mistimed exit can never leak a stray character into
the child's prompt.

Three supporting choices:

- **Legacy control-byte alias.** crossterm's legacy (non-kitty) byte parser decodes
  the raw bytes `0x1C`..`0x1F` as `Ctrl-4`..`Ctrl-7`, not the literal control
  character — so the default Ctrl-\ (`0x1C`) arrives as `Char('4')+CONTROL` on a
  non-kitty outer terminal and `Char('\\')+CONTROL` under kitty. `KeyChord::matches`
  folds this alias both ways for CONTROL chords, so the same `--resize-key` value
  works regardless of which decode path is live.
- **Release/repeat guard.** With kitty `REPORT_EVENT_TYPES` on, every press is
  followed by a release event. The chord only fires on `Press`, so its own release
  can never re-toggle the mode, and a held, auto-repeating chord doesn't flicker
  enter/exit on every repeat. Step keys are the opposite: they act on repeats (hold
  `h` to keep shrinking).
- **Unit-preserving steps.** `step_width` nudges a `Width` in its own unit — columns
  for `Cols`, percent for `Percent` — and never converts between them, clamped to
  `[MIN_W, real_cols]`. The floor never exceeds the *current* effective width, so a
  band already narrower than `MIN_W` (legal via `--width 10`) only pins in place on a
  shrink press; it never snaps up.

The manual width change (`apply_resize_step`) mirrors `handle_resize`'s ADR-008
ordering — resize the PTY first, then `set_size(rows, cols)` — but holds `real_cols`
fixed and steps `width_config` instead of recomputing from a new terminal size.

The ~3s idle auto-exit is loop-owned, not renderer-owned: a `ResizeCtl` holds an
`Option<Instant>` deadline in `run`'s scope, using the same injected [`Clock`]
(ADR-007) the coalescing loop already runs on, so idle behaviour is virtual-clock
testable with no wall-clock sleeps. Not-in-mode, the loop's single blocking `recv()`
park is untouched (zero idle CPU, zero wakeups); in mode, Phase A becomes a bounded
`recv_until(deadline)` so a silent child still wakes the loop to auto-exit, and a
top-of-frame check catches a flooding child that never lets Phase A block.

A swallowed key does **not** refresh the idle deadline — idle means "no *resize*
key", so a mistimed stray keypress still auto-exits on schedule rather than
extending the mode indefinitely.

## Consequences

- The chord is the first and only key gutter ever withholds from the child; every
  other key still reaches `dispatch` → `encode_key` unchanged.
- `apply_message` wraps `dispatch` at both live-loop call sites (Phase A and Phase
  B); the shutdown drain keeps calling `dispatch` directly since resize keys during
  teardown are irrelevant.
- The visual overlay (rails, width readout, uniform strip-clear) is a separate seam
  (`enter_resize_overlay` / `refresh_resize_overlay` / `clear_resize_overlay`) that
  this decision's implementation ships as stubs — `refresh_resize_overlay`'s stub
  keeps parity with `handle_resize`'s alt-screen-only gutter clear so an in-mode
  shrink doesn't strand stale columns, but the primary-screen generalisation is a
  separate cross-cutting concern.
- `base_row` is untouched by a width-only change, keeping the inline anchor
  (ADR-013) exactly as `handle_resize` does on a width-only resize.

## Code anchors

- `src/keyboard/chord.rs` — `KeyChord`, `parse_chord`, the legacy alias fold
- `src/render.rs` — `classify_key`, `apply_resize_step`, `ResizeCtl`, `apply_message`,
  and the `run` loop's idle-check wiring
- `src/geometry.rs` — `step_width`
- `src/cli.rs` — `--resize-key`

The resize ordering this reuses is [ADR-008](0008-resize-ordering.md); the
coalescing loop it extends is [ADR-007](0007-coalescing-loop.md); the keyboard
re-encoding it withholds from is [ADR-002](0002-keyboard-always-re-encoded.md).
