# ADR-017: Uniform margin management

Status: Accepted

## Context

Two things need gutter to paint and clear the gutter columns on the primary (inline)
screen, which it deliberately did not do before: [ADR-008](0008-resize-ordering.md)'s
gutter clear ran on the alt screen only.

1. **Resize-mode rails.** PRD 0001 (Feature 2) asks for faint eighth-block rails and
   a width readout that hug the band's edges while resize mode is active, sliding as
   the band resizes and vanishing on exit.
2. **A shrink strands content, rails or not.** When `W` narrows, columns that were
   band become gutter but still hold the glyphs gutter painted there, and the
   per-frame `rows_diff` repaint only touches the new, narrower band. On the primary
   screen those stale columns stayed until something scrolled over them; shrinking
   only worked on the alt screen, where gutter already owned and cleared the whole
   viewport.

## Decision

Make the gutter clear a row-span clear that runs in **both** screen modes:
`clear_gutter` takes an explicit `row_start, row_end` span, and the caller passes
`0..rows` on the alt screen or `base_row..rows` on the primary screen. The span is what
makes the primary-screen clear safe: shell history above the row the inline band
started on is never in it.

One entry point, `repaint_margins`, does the row-span clear and then, only while
resize mode is active, draws the rails and width readout over it. The layout is pure
(`geometry::rail_layout` / `readout_text`); the terminal only emits the escapes, via
`OuterTerminal::draw_rails`. Rails and readout live strictly in gutter columns, never
inside the band, except the readout's in-band fallback when there is no gutter room.

**Clearing to blank, not restoring.** While a child owns the band, nothing but gutter
writes to the band's rows, so the gutter columns across the band's row span are
reliably blank. gutter therefore clears them to blank on a shrink and on leaving the
mode, rather than capturing and restoring what was there before — it has no model of
that content and does not need one here. This holds for the real use cases: nvim and
Claude Code (both on the alt screen, where gutter already owns the margins) and any
inline child. **Known edge case:** a child launched with existing content *below* the
cursor on the visible primary screen (rare: a mid-screen cursor after a custom clear,
a `:term` split) will see that content blanked in the gutter columns on the first
resize, since gutter cannot tell it from its own earlier paint.

**The in-band readout needs its own clear on exit.** On a full-width (or nearly
full-width) band the readout has no gutter room and falls back to just inside the
band's bottom-right corner. The diff repaint that follows `reset_prev_baseline` cannot
be relied on to erase it: a diff against a blank baseline only re-emits rows that
hold content, and a blank child row produces nothing. `repaint_margins(.., false)`
therefore blanks the readout's cells explicitly (`move_to` + `write_row`) whenever the
current layout places a readout. Where it sat in the gutter this is a harmless second
clear; where it covered live child content, the paired baseline reset repaints that
content next frame.

**Growing the band strands the old rails.** Growing the band while resize mode is
active moves the old rail columns (`prev margin - 1`, `prev band_end`) inside the new,
wider band — out of reach of both the new-geometry `clear_gutter` and `draw_rails` —
and `render_once` never repaints a blank child row, so the stale glyphs would survive
indefinitely, even past leaving the mode. `refresh_resize_overlay` takes a `BandGeom`
snapshot of the geometry from before the change and blanks exactly the cells it
describes (rail columns, readout span) before repainting at the current geometry. On a
shrink the old cells already fall in the new gutter, so this is a harmless second
clear.

## Consequences

- `OuterTerminal::clear_gutter` takes the row span (`row_start`, `row_end`). All four
  implementations — `CrosstermTerminal`, the oracle's `Tape`, and the `MockTerminal`
  and `RecordingGrid` test doubles — take it, as does the mock's `Call::ClearGutter`.
  The bytes themselves come from one emitter, `clear_gutter_bytes`
  ([ADR-024](0024-terminal-relative-gutter-erases.md)).
- `OuterTerminal::draw_rails(&Rails)` is backed by pure geometry
  (`geometry::Rails` / `Readout` / `rail_layout` / `readout_text`), so rail and
  readout placement is unit- and property-testable without a terminal.
- `handle_resize`'s final step (ADR-008 step 5) calls `repaint_margins` in both screen
  modes instead of gating the clear on `outer_alt_active`.
- Rails and readout never enter `render_once`'s `rows_diff` path or the equivalence
  gate. They are gutter chrome painted outside the per-frame band repaint, so
  [ADR-006](0006-band-fit-margin-rule.md)'s `primary_path_does_not_walk_cells` is
  unaffected.
- A terminal resize while resize mode is active repaints the rails through a second,
  separate call: `apply_message` checks after `dispatch` returns, rather than a mode
  flag being threaded into `handle_resize`, because that flag lives in the render
  loop's local scope, not on `Renderer`. Both calls are queued, never flushed, inside
  the same coalescing frame ([ADR-007](0007-coalescing-loop.md)), so this path costs
  one redundant queued clear, never a visible flicker.

## Amendment — clearing the band interior on a widen

`clear_gutter` only ever blanks the columns *outside* the band. That leaves a twin of
the stranded-rails problem above: on a widen, the old, narrower band's *content* now
sits inside the new band's columns, where the gutter clear never reaches and the diff
repaint against a blank baseline never paints over it (a blank cell yields no diff
run), so the stale glyphs survive indefinitely.
`OuterTerminal::clear_row_span(row_start, row_end)` blanks whole physical rows, band
interior included, across the same span — `0..rows` on the alt screen,
`base_row..rows` on the primary. `handle_resize` calls it just before
`repaint_margins`, and so does `apply_resize_step` whenever the width actually
changes. On resuming from a suspend, the primary-screen anchor is reseeded to the
bottom *before* the missed-resize catch-up runs that clear, so it never wipes the
shell history a stale `base_row` still points into
([ADR-019](0019-suspend-resume-cycle-ordering.md)).

## Code anchors

- `src/geometry.rs` — `Rails`, `Readout`, `readout_text`, `rail_layout`
- `src/terminal.rs` — `OuterTerminal::clear_gutter` (row span), `clear_row_span`,
  `draw_rails`, and the `MockTerminal` / `RecordingGrid` implementations
- `src/render.rs` — `repaint_margins`, `BandGeom`, `handle_resize`'s final step, and
  `enter_resize_overlay` / `refresh_resize_overlay` / `clear_resize_overlay`

Supersedes, in part, [ADR-008](0008-resize-ordering.md) step 5 (the alt-screen-only
clear). The resize-mode state machine this is called from is
[ADR-016](0016-modal-resize.md).
