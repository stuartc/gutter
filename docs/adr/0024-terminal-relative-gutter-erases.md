# ADR-024: Gutter erases are terminal-relative, never sized from a cached width

Status: Accepted

## Context

gutter blanks the columns outside the band — both gutters, across the band's rows —
every time the geometry changes. `repaint_margins` calls `clear_gutter`, and
`handle_resize` calls `repaint_margins`, so the clear fires repeatedly while someone
drags a window edge.

The clear used to paint runs of spaces sized from `real_cols`: the terminal width
gutter read the last time it handled a `SIGWINCH`. The right run started at
`band_end` and was `real_cols - band_end` spaces long, so it ended in what gutter
*believed* was the last column.

While the window sits still that is exactly right. During a drag it is a race gutter
always loses. The terminal resizes continuously; gutter composes a frame from the
size it last read; the bytes arrive at a terminal that has already moved. A
130-column fill reaching a 128-column terminal overruns by two. The two spaces wrap
onto the row below, landing on band content the diff baseline will not repaint — and,
worse, the terminal marks the overrun line **soft-wrapped**, so the next reflow joins
it with its neighbour. The result is a smeared line that outlasts the drag.

It is the same hazard as [ADR-014](0014-row-run-self-containment.md)'s, seen from
the other side: a byte run whose reach depends on a width the terminal might no
longer have.

## Decision

Outside the band, clears use the terminal's own erase escapes, which act on its
*actual* edges whatever gutter currently believes:

- **Right gutter:** `MoveTo(band_end, row)` then `ESC[K` — erase from the cursor to
  the real right edge, inclusive. A stale `real_cols` cannot overrun: a terminal that
  shrank just gets less erased, and one that grew gets its newly exposed columns
  cleared too, which the sized run never did.
- **Left gutter:** `MoveTo(margin - 1, row)` then `ESC[1K` — erase from the line start
  through the cursor, inclusive (ECMA-48 EL 1), covering `[0, margin)` exactly. The
  left run was far less exposed than the right, but one rule for both gutters is
  simpler than two, and it means the width-independence test below covers the whole
  clear.

Both erases fill with the current background, so the clear still opens with an SGR
reset.

Inside the band, ADR-014's bounded fills stand, and the two are one rule read from
either side. A raw `ESC[K` in a row run would take the right gutter with it, so there
the erase is rewritten into a fill of the band's own columns. In the gutter the sized
fill is the hazard and the unbounded erase is exactly right, because the gutter really
does extend to the terminal's edge, wherever that currently is. **Never let a byte
run's reach depend on a width the terminal might no longer have; use the terminal's
own edge when the edge is what you mean.**

This works alongside the host autowrap-off that ADR-014 records, not instead of it.
Autowrap off limits the damage an overrunning run can do; this decision removes the
overrun.

## Consequences

- **One emitter.** `clear_gutter_bytes` in `src/terminal.rs` produces the bytes, and
  every implementation goes through it: `CrosstermTerminal` writes them, the oracle's
  `Tape` delegates to `CrosstermTerminal`, and the `RecordingGrid` mock feeds them to
  its parser. What production writes, what the painted-band check replays and what the
  mock reads back are the same stream, so a change here cannot leave the check
  replaying bytes production no longer writes.
- **The standing test checks that the width makes no difference, not the byte
  shape.** `gutter_clear_does_not_depend_on_the_believed_width` composes the same
  clear at two very different believed widths and requires identical bytes. Any future
  fill sized from the cached width breaks it immediately, however it is spelled.
- **The gutter cells are erased, not written.** A readback sees them empty rather than
  holding a space.
- **A full-width band clears nothing.** With no left gutter and the band already
  ending at the believed edge, only the SGR reset goes out; `handle_resize`'s row-span
  clear covers the columns a grown terminal exposes.
- **A per-row clear drops from tens of bytes of spaces to three or four bytes of
  escape.**
- **Accepted: the `MoveTo` columns still come from cached geometry.** If the terminal
  shrank below `band_end`, the move stops at the last column and `ESC[K` erases a cell
  or two of what gutter thinks is band interior. This lasts one frame: the pending
  `SIGWINCH` reaches `handle_resize`, which clears the band's row span and resets the
  diff baseline for a full repaint. Nothing lasting is left behind, unlike the
  soft-wrap flag a space run plants. Under an extreme shrink the left `ESC[1K` could
  erase across the whole visible line, and the same argument covers it.
- **Accepted: the band paint's own `left_margin` is cached too.** Every row is painted
  at `move_to(left_margin, phys_row)` from the same geometry, and a `W`-wide run can
  overrun if the terminal shrinks below `margin + W` before the `SIGWINCH` is handled.
  This is not fixed here and cannot be closed in principle — there is no way to read
  the size and have the bytes arrive in the same instant. The only candidate is
  re-reading the size every frame and clipping speculatively, which costs an `ioctl`
  per frame to narrow a race it cannot close, and cuts against
  [ADR-008](0008-resize-ordering.md)'s rule that geometry changes happen in the resize
  handler's ordered sequence. The removed version fired on every clear across the
  whole gutter width; what is left needs a shrink to eat into the band itself within a
  one-frame window. If a real capture ever shows band-interior rows wrapping mid-drag,
  that is its own piece of work.

## Code anchors

- `src/terminal.rs` — `clear_gutter_bytes`, the one emitter;
  `CrosstermTerminal::clear_gutter` and the `RecordingGrid` mock, both routed through
  it; `gutter_clear_does_not_depend_on_the_believed_width`,
  `a_stale_width_clear_cannot_reach_the_row_below`,
  `gutter_clear_erases_relative_to_the_real_edges`, `a_full_width_band_clears_nothing`
- `src/oracle/band.rs` — `a_gutter_clear_composed_at_a_stale_width_stays_on_its_rows`:
  a clear composed at 130 columns replayed through wezterm-term at 128, with the host's
  autowrap deliberately left on so autowrap-off cannot hide the overrun
- `src/oracle/mod.rs` — `WeztermGrid::row_wrapped`, the soft-wrap flag that check reads
- `src/render.rs` — `repaint_margins`, the only caller of `clear_gutter`, and
  `handle_resize`'s row-span clear and baseline reset that the accepted cases above
  rely on

The band-interior counterpart is [ADR-014](0014-row-run-self-containment.md); the
ordered geometry change is [ADR-008](0008-resize-ordering.md); the row span a clear is
allowed to touch is [ADR-017](0017-uniform-margin-management.md)'s.
