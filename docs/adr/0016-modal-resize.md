# ADR-016: Modal resize

Status: Accepted

## Context

gutter's whole purpose is a comfortable reading width, but until now that width was
fixed at launch. PRD 0001 (Feature 2) asks for a live resize interaction without
teaching the child anything about it — and without gutter ever swallowing a keystroke
in normal operation, which it has never done before.

## Decision

Reserve exactly one chord (`--resize-key`, default Ctrl-\) to toggle a modal resize
state. The chord — and, while in the mode, every key — is classified and intercepted
in the render thread's input arm *before* the passthrough write (ADR-020), so the
child never sees it. In mode, `h`/`l` (aliases `←`/`→`, `-`/`+`) step ±1 unit, `H`/`L`
step ±10, `Esc` or the chord again exits, and an unrecognised key is swallowed —
consumed but not forwarded — so a mistimed exit can never leak a stray character into
the child's prompt.

Three supporting choices:

- **Byte-form chord matching.** With no decoding layer, `Ctrl-\` is the literal
  byte `0x1C` and the crossterm `Ctrl-4` alias disappears entirely — the two
  spellings now compute the same byte, so they match each other for free. But the
  *same physical key* arrives in a different encoding once a keyboard mode is
  live on the outer terminal: `ESC[92;5u` under kitty, `ESC[27;5;92~` under
  modifyOtherKeys. The chord therefore matches a small set of byte forms rather
  than one. False positives are not a concern: a bare `0x1C` cannot appear inside
  a terminal-generated escape sequence, because C0 controls abort or execute
  mid-sequence.
- **Release-form guard.** gutter no longer pushes `REPORT_EVENT_TYPES` itself, so
  releases only arrive when the *child* asked for them. When that happens, the
  chord's release form must not be matched as a second chord press, or the mode
  would toggle straight back out on key-up. kitty carries the event type as a
  sub-parameter of the *modifier* field, not the key field, so that form is
  `ESC[92;5:3u`. The matcher recognises the press forms only. Step keys are the
  opposite: they act on repeats (hold `h` to keep shrinking), which falls out for
  free, since an auto-repeating legacy key simply sends its byte again.
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
  other byte is forwarded to the PTY unchanged.
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
- Inside a bracketed paste the chord is not matched at all (ADR-022). Pasted text is
  data, not input protocol, so a `0x1C` in it is forwarded like any other byte —
  which removes the live misbehaviour where a paste containing that byte dropped the
  user into resize mode and the rest of the paste was eaten as commands. The
  suppression is gated on gutter having mirrored `?2004h` outward, so a program that
  never asked for paste protection cannot disable the chord by sending the guards
  itself.
- The in-mode arrow aliases match both cursor-key encodings, `ESC [ D` and `ESC O D`.
  With DECCKM mirrored (ADR-022) the terminal really does send the SS3 form, and a
  decoder matching only the CSI form would silently drop it.

## Code anchors

- `src/chord.rs` — `Chord`, `parse_chord`, the byte-form matcher
- `src/render.rs` — `classify_unit`, `apply_resize_step`, `ResizeCtl`, `apply_message`,
  and the `run` loop's idle-check wiring
- `src/geometry.rs` — `step_width`
- `src/cli.rs` — `--resize-key`

The resize ordering this reuses is [ADR-008](0008-resize-ordering.md); the
coalescing loop it extends is [ADR-007](0007-coalescing-loop.md); the passthrough
it withholds from is [ADR-020](0020-raw-input-passthrough.md).
