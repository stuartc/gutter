# ADR-017: Uniform margin management

Status: Accepted

## Context

Two things now need gutter to actively paint and clear the margin/gutter columns on
the primary (inline) screen — something it deliberately did not do before (ADR-008's
gutter-clear ran on the alt screen only):

1. **Resize-mode rails.** PRD 0001 (Feature 2) asks for faint eighth-block rails and
   a width readout that hug the band edges while resize mode is active, sliding as
   the band resizes and vanishing on exit.
2. **Shrinking strands content regardless of rails.** When `W` narrows, columns that
   *were* band become margin but still hold the old band glyphs gutter painted
   there; the per-frame `rows_diff` repaint only touches the new, narrower band. On
   the primary screen those stale columns previously persisted until something
   scrolled over them — shrink only worked correctly on the alt screen, where
   gutter already owned (and cleared) the whole viewport.

## Decision

Generalise the gutter-clear from "alt-screen only, `[0, rows)`" to a row-span clear
that runs on **both** screen modes: `clear_gutter` takes an explicit `row_start,
row_end` span, and the caller supplies `0..rows` on the alt screen or `base_row..rows`
on the primary screen. The row span is what makes the primary-screen clear safe —
history above the inline band's launch row is never in the cleared span.

A single entry point, `repaint_margins`, does the row-span clear and then — only
while resize mode is active — draws the rails and width readout over it (pure
layout in `geometry::rail_layout`/`readout_text`; the terminal only emits the
escapes via a new `OuterTerminal::draw_rails`). Rails and the readout live strictly
in gutter columns (never `[margin, margin + W)`, ADR-006), except the readout's
documented in-band fallback when there is no gutter room.

**The clears-to-blank assumption (carried from the PRD verbatim):** while a child
owns the band, nothing but gutter writes to the band's rows, so the margin columns
across the band's row-span are reliably blank. gutter therefore clears-to-blank on
shrink and on mode-exit rather than capturing and restoring pre-existing margin
content, which it has no model of and does not need here. This holds for the real
use cases (nvim, Claude Code — both alt-screen, where gutter already owns the
margins — and a running inline child). **Known edge case:** a child launched with
pre-existing content *below* the cursor on the visible primary screen (rare: a
mid-screen cursor after a custom clear, a `:term` split) will see that content
blanked in the gutter columns on the first resize, since gutter has no way to
distinguish it from its own prior paint.

**The exit-clear gap for the in-band readout.** On a full-width (or near-full-width)
band the readout has no gutter room and falls back inside the band's bottom-right
corner. `reset_prev_baseline`'s diff-based repaint cannot be relied on to erase it
on mode exit: a diff against a blank baseline only re-emits rows that changed, and a
blank child row produces no diff run. `repaint_margins(.., false)` therefore blanks
the readout's cells explicitly (`move_to` + `write_row`, no new primitive) whenever
the current layout places a readout — a harmless double-clear when it sat in the
gutter, and restored by the paired `reset_prev_baseline` repaint when it overlapped
live child content.

**The grow-strands-old-rails gap.** Growing the band while resize mode is active
moves the old rail columns (`prev margin - 1`, `prev band_end`) INSIDE the new,
wider band — past the reach of both the new-geometry `clear_gutter` and
`draw_rails`, and `render_once` never repaints a blank child row, so the stale
glyphs would otherwise survive indefinitely, even past mode exit. `refresh_resize_overlay`
takes a `BandGeom` snapshot of the geometry as it stood before the change and
blanks exactly the cells it describes (rail columns, readout span) before
repainting at the current geometry — a harmless double-clear on shrink, where the
old cells already fall in the new gutter.

## Consequences

- `OuterTerminal::clear_gutter` gained two parameters (`row_start`, `row_end`); all
  three implementations (`CrosstermTerminal`, `MockTerminal`, `RecordingGrid`) and
  the mock's `Call::ClearGutter` variant changed arity to match.
- A new `OuterTerminal::draw_rails(&Rails)` method, backed by pure geometry
  (`geometry::Rails`/`Readout`/`rail_layout`/`readout_text`) so the rail/readout
  placement is unit- and property-testable without a terminal.
- `handle_resize`'s step 5 (ADR-008) now calls `repaint_margins` unconditionally
  instead of gating the clear on `outer_alt_active`.
- Rails/readout never enter `render_once`'s `rows_diff` path or the equivalence
  gate — they are gutter-only chrome painted outside the per-frame band repaint, so
  ADR-006's cell-walk-free purity test is unaffected.
- A terminal resize while resize mode is active repaints the rails via a second,
  independent call path (`apply_message`'s post-dispatch check), not by threading a
  live mode flag into `handle_resize` itself — that flag lives in the render loop's
  local scope, not on `Renderer`. The two calls are both queued, never flushed,
  inside the same coalescing frame (ADR-007), so this costs one redundant queued
  clear on that path, never a visible flicker.

## Amendment — band-interior clear on a widen

`clear_gutter` only ever blanks the columns *outside* the band. That leaves a twin of
the "grow strands old rails inside the new band" gap this ADR closes: on a widen the
old, narrower band's *content* now sits inside the new band's columns, where the gutter
clear never reaches and the blank diff baseline never repaints over it (a blank cell
yields no diff run), so the stale glyphs survive indefinitely. `OuterTerminal::clear_row_span(row_start, row_end)`
blanks whole physical rows (band interior included) across the same ADR-017 span —
`0..rows` on the alt screen, `base_row..rows` on the primary — and is called just before
`repaint_margins` in both `handle_resize` and `apply_resize_step`'s grow branch. On the
suspend/resume catch-up the primary-screen anchor is reseeded to the bottom *before* that
clear, so it never wipes the shell history the stale `base_row` still points into.

## Code anchors

- `src/geometry.rs` — `Rails`, `Readout`, `readout_text`, `rail_layout`
- `src/terminal.rs` — `OuterTerminal::clear_gutter` (row-span), `clear_row_span`,
  `draw_rails`, and the `MockTerminal`/`RecordingGrid` implementations
- `src/render.rs` — `repaint_margins`, `handle_resize`'s step 5, and the
  `enter_resize_overlay` / `refresh_resize_overlay` / `clear_resize_overlay` seam

Supersedes, in part, [ADR-008](0008-resize-ordering.md) step 5 (the alt-screen-only
clear). The band-fit margin rule the rails must respect is
[ADR-006](0006-band-fit-margin-rule.md); the resize-mode state machine this is
called from is [ADR-016](0016-modal-resize.md).
