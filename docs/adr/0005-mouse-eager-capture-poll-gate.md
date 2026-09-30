# ADR-005: Mouse eager capture and a per-report forwarding gate

Status: Accepted

## Context

gutter cannot react to the child's mouse negotiation as it happens. The child's
`CSI ?1000h`, `CSI ?1006h` and friends are DECSET private modes that vt100 absorbs
into its screen state, so they never reach `unhandled_csi`. The only place to learn
what the child asked for is the parser's live screen state, read on demand.

Mouse reports also carry coordinates. The real terminal reports physical columns,
while the child's grid is `W` columns wide and starts at the band's left margin, so
every report has to be translated before the child sees it.

## Decision

Enable mouse reporting on the outer terminal once at startup (after raw mode and the
startup cursor-position probe, before the first frame) and disable it once at
teardown. There is no reactive toggling as the child changes its mode.

For each SGR-1006 report the input scanner extracts (ADR-020), read the child's live
`(mode, encoding)` from the screen and run the report through the gate: translate the
coordinates (subtract the margin, drop anything outside the band), filter motion down
to what the child's mode asks for, and re-encode as SGR-1006.

Only SGR is ever sent to the child. A report the child could only receive in another
encoding is dropped rather than sent in a shape that would desync its parser.

Dropped, not fatal. This used to panic, on the reading that a child negotiating a
non-SGR encoding was a gap worth failing loudly on. It is not a gap: `CSI ?1000h`
with no `?1006h` is what plenty of older TUIs ask for, and the outer terminal reports
in SGR regardless because the capture is eager. One click in the band took the
render thread down with raw mode and mouse reporting still on and the ordered restore
(ADR-010) never run. A shell that needs `reset` is a worse outcome than a child that
gets no mouse.

## Consequences

- Eager capture removes the dropped-first-click race: the outer terminal is already
  reporting SGR before the child turns its own mode on.
- The gate reads `mouse_protocol_mode()` and `mouse_protocol_encoding()` from the live
  screen on the render thread, with no lock. Every `Msg::Pty` that arrived ahead of
  the report has already been applied, so a mode change the child has just sent is
  already visible.
- Motion is filtered by the child's mode: `None` swallows everything,
  `ButtonPress`/`ButtonRelease` drop motion, `ButtonMotion` forwards motion only while
  a button is held, and `AnyMotion` forwards all of it.
- The SGR final byte matters: `M` for press and motion, `m` for release. Getting it
  wrong leaves a button stuck down in the child.
- Coordinate translation subtracts the margin with `checked_sub`: a click in the left
  gutter underflows to `None` and is dropped, never forwarded.
- The report's button byte, modifier bits and all, is passed through from the
  scanner's `MouseReport` rather than rebuilt.
- gutter writes the enable and disable bytes itself rather than using crossterm's
  `EnableMouseCapture`, and leaves out `?1015h`: the scanner recognises the SGR shape
  only, so a terminal that honoured the urxvt encoding would send reports gutter does
  not extract, and they would leak to the child as literal bytes.
- The child's own mouse-mode escapes stay absorbed and are never mirrored outward.
  ADR-022 mirrors three other absorbed modes to the real terminal, and deliberately
  leaves the mouse out: two authorities over the terminal's mouse state would fight,
  and a child that disabled reporting would turn gutter's own capture off underneath
  it. `vt100::Screen::input_mode_diff` bundles the mouse modes in with those three,
  which is the most likely way this gets broken by accident. Hence ADR-022's
  hand-rolled diff, and the negative tests `mouse_modes_are_not_mirrored`
  (`src/modes.rs`) and `no_mouse_mode_ever_reaches_the_outer_terminal`
  (`src/render.rs`).

## Code anchors

- `src/mouse.rs` — the gate, the button-held tracker, coordinate translation, and the
  `MOUSE_ENABLE` / `MOUSE_DISABLE` byte strings
- `src/scan.rs` — SGR extraction on the input side (`Token::Mouse`, `MouseReport`)
- `src/terminal.rs` — `OuterTerminal::enable_mouse` / `disable_mouse`, which write
  those bytes
- `src/main.rs` — the one eager capture call
- `src/render.rs` — the `Token::Mouse` arm of `walk_tokens`, which reads the live
  mode and runs the gate
