# ADR-005: Mouse eager capture and poll-diff gate

Status: Accepted

## Context

We can't react to the child's mouse negotiation the way the original plan assumed:
the child's `CSI ?1006h` and friends are DECSET private modes that vt100 absorbs
into its screen state, so they never reach `unhandled_csi`. That is true whoever
reads the input fd, so a per-frame poll of the screen is the only mirror point
available.

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
- The input side is now gutter's own scanner (ADR-020), not crossterm's decoder.
  The gate's contract is unchanged; only the type of its input moved from a
  decoded event to an extracted SGR report, whose button byte — modifier bits and
  all — is passed through rather than rebuilt.
- gutter writes the outer enable/disable bytes itself rather than using
  crossterm's bundle, deliberately omitting `?1015h`: the scanner recognises the
  SGR shape only, and reports in the urxvt encoding would leak to the child.
- The child's own mouse-mode escapes stay absorbed. ADR-022 mirrors three other
  modes vt100 swallows out to the real terminal by this same poll-diff, and
  deliberately excludes the mouse: two authorities over the terminal's mouse state
  would fight, and a child that disabled reporting would turn gutter's own capture
  off underneath it. `vt100::Screen::input_mode_diff` bundles the mouse modes in
  with those three, which is the single most likely way this gets broken by
  accident — hence the hand-rolled diff and its negative test.

## Code anchors

- `src/mouse.rs` — the gate, the button-held tracker, coordinate translation
- `src/scan.rs` — SGR extraction on the input side
- `src/terminal.rs` — the hand-written enable/disable byte strings
- `src/main.rs` — the one eager capture call
- `src/render.rs` — the per-frame live poll
