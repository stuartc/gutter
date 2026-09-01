# ADR-024: Gutter erases are terminal-relative, never sized from a cached width

Status: Accepted

## Context

gutter blanks the columns outside the band — both gutters, across the band's rows —
every time the geometry changes. `repaint_margins` calls `clear_gutter`, and
`clear_gutter` is called from `handle_resize`, so it fires repeatedly while someone
drags a window edge.

It used to paint runs of spaces, and it sized them from `real_cols`: the terminal width
gutter read the last time it handled a `SIGWINCH`. The right run started at `band_end`
and was `real_cols - band_end` spaces long, so by construction it ended in what gutter
*believed* was the last column.

While the window sits still that is exactly right. During a drag it is a race gutter
always loses. The terminal is resizing continuously; gutter composes a frame from the
size it last read; the bytes arrive at a terminal that has already moved. A 130-column
fill reaching a 128-column terminal overruns by two. The two spaces wrap onto the row
below, landing on band content the diff baseline will not repaint — and, the durable
part, the terminal marks the overrun line **soft-wrapped**, so the next reflow joins it
with its neighbour. That is a single smeared line surviving long after the drag ended.

The same hazard, in the same shape, as [ADR-014](0014-row-run-self-containment.md)'s:
a byte run whose reach depends on a width the terminal might no longer have.

## Decision

Outside the band, clears use the terminal's own erase escapes, which are relative to
its *actual* edges whatever gutter currently believes:

- **Right gutter:** `MoveTo(band_end, row)` then `ESC[K` — erase from the cursor to the
  real right edge, inclusive. A stale `real_cols` cannot overrun: a terminal that shrank
  just gets less erased, and one that grew gets its newly exposed columns cleared, which
  the sized run never did. Better in both directions.
- **Left gutter:** `MoveTo(margin - 1, row)` then `ESC[1K` — erase from the line start
  through the cursor, inclusive (ECMA-48 EL 1), covering `[0, margin)` exactly. The
  left run is far less exposed than the right, but one rule for both gutters is easier
  to hold than two, and it lets the standing test cover the whole emission.

Both erases fill with the current background, so the SGR reset that already led the
emission stays.

Inside the band, ADR-014's bounded fills stand, and the two are one rule read from
either side. A raw `ESC[K` in a row run would take the right gutter with it, so there
the erase is rewritten into a fill of the band's own columns. In the gutter the sized
fill is the hazard and the unbounded erase is exactly right, because the gutter
genuinely does extend to the real edge — wherever that currently is. **Never let a byte
run's reach depend on a width the terminal might no longer have; use the terminal's own
edge when the edge is what you mean.**

This sits under, not instead of, the autowrap-off belt ADR-014's amendment records.
Wrap-off limits the damage an overrunning run can do; this removes the overrun.

## Consequences

- **One emitter.** `clear_gutter_bytes` in `src/terminal.rs` produces the bytes, and
  every implementation goes through it: `CrosstermTerminal` writes them, the oracle's
  `Tape` delegates to that, and the `RecordingGrid` mock feeds them to its parser. What
  production writes, the oracle replays and the mock reads back are the same stream by
  construction, so a change here cannot leave the painted-band check exercising bytes
  production no longer emits.
- **The standing guard is an invariance test, not a byte-shape one.** The same clear
  composed at two very different believed widths must emit identical bytes. Any future
  fill sized from the cached width breaks it immediately, whatever spelling it uses.
- **The gutter cells are erased, not written.** A readback sees them empty rather than
  holding a space. The recording mock's cell readbacks already asserted blank.
- **A per-row clear drops from tens of bytes of spaces to three or four of escape.**
- **Accepted residual: the `MoveTo` columns are still cached geometry.** If the terminal
  shrank below `band_end`, the move clamps to the last column and `ESC[K` erases a cell
  or two of what gutter thinks is band interior. Transient: the pending `SIGWINCH`
  reaches `handle_resize`, which clears the span and resets the diff baseline for a full
  repaint. Nothing durable survives, unlike the wrap flag a space run plants. Under an
  extreme shrink the left `ESC[1K` could erase across the whole visible line, and the
  same argument covers it.
- **Accepted residual: the band paint's own `left_margin` is cached too.** Every row is
  painted at `move_to(left_margin, phys_row)` from the same geometry, and a `W`-wide run
  can overrun if the terminal shrinks below `margin + W` before the `SIGWINCH` is
  handled. Not fixed here, and not fixable in principle — there is no way to read the
  size and have the bytes arrive atomically. The only candidate is re-reading the size
  every frame and clipping speculatively, which buys an `ioctl` per frame to narrow a
  race it cannot close, and fights [ADR-008](0008-resize-ordering.md)'s rule that
  geometry changes happen in the resize handler's ordered sequence. The version this
  record removes fired on every clear across the whole gutter width; what is left needs
  a shrink to eat into the band itself inside a one-frame window. If a real capture ever
  shows band-interior rows wrapping mid-drag, that is its own effort.

## Code anchors

- `src/terminal.rs` — `clear_gutter_bytes`, the one emitter; `CrosstermTerminal::clear_gutter`
  and the `RecordingGrid` mock both routing through it; the invariance, stale-width
  readback and byte-shape tests
- `src/oracle/band.rs` — `a_gutter_clear_composed_at_a_stale_width_stays_on_its_rows`:
  a clear composed at 130 columns replayed through wezterm-term at 128, with the host's
  autowrap deliberately left on so the wrap belt cannot hide the mechanism
- `src/oracle/mod.rs` — `WeztermGrid::row_wrapped`, the soft-wrap flag that check reads
- `src/render.rs` — `repaint_margins`, the only caller, and `handle_resize`'s span clear
  and baseline reset that the residuals above lean on

The band-interior counterpart is [ADR-014](0014-row-run-self-containment.md); the
ordered geometry change is [ADR-008](0008-resize-ordering.md); the row span a clear is
allowed to touch is [ADR-017](0017-uniform-margin-management.md)'s.
