# ADR-005: Mouse eager capture and poll-diff gate

Status: Accepted

## Context

We can't react to the child's mouse negotiation the way the original plan assumed:
the child's `CSI ?1006h` and friends are DECSET private modes that vt100 absorbs
into its screen state, so they never reach `unhandled_csi`. crossterm also has no
"mode changed" event to hook.

## Decision

Capture the outer terminal's mouse once at startup (after raw mode, before
alt-screen setup) and disable it once at teardown — no reactive toggling. Each
frame, after `parser.process()`, read the child's live `(mode, encoding)` from the
screen and run the mouse gate: translate coordinates (subtract the margin, drop
anything outside the band), down-filter motion to the child's granularity, and
re-encode as SGR-1006. Only SGR is emitted; a non-SGR encoding bails loudly.

## Consequences

- Eager capture kills the dropped-first-click race: the outer terminal is already
  reporting SGR before the child turns its mode on.
- The gate polls `mouse_protocol_mode()` and `mouse_protocol_encoding()` each frame,
  read on the render thread with no lock.
- Motion is filtered by the child's mode: `None` swallows, `ButtonPress`/
  `ButtonRelease` drop motion, `ButtonMotion` forwards only while a button is held,
  `AnyMotion` forwards all.
- The SGR final byte is load-bearing: `M` for press/motion, `m` for release. The
  wrong byte is a stuck-button bug.
- Coordinate translation subtracts the margin with `checked_sub`: a click that
  lands in the gutter underflows to `None` and is dropped, never forwarded to the
  child.

## Code anchors

- `src/mouse.rs` — the gate, the button-held tracker, coordinate translation
- `src/main.rs` — the one eager capture call
- `src/render.rs` — the per-frame live poll
