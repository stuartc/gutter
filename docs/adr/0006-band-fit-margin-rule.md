# ADR-006: Band-fit margin rule

Status: Accepted

## Context

When we paint a row at a left-margin offset, anything that extends past column `W`
bleeds into the gutter. The danger cases are wide glyphs (two columns) at the band
edge and `ESC[K` (erase to right edge), the only unbounded sequence vt100 emits
into a row.

## Decision

Rely on vt100's margin rule for right-edge safety: a wide glyph never has its lead
cell at column `W-1`; vt100 wraps it to the next row instead. So every cell's right
edge sits within `W` when painted at the offset, and the `rows_diff` path needs no
cell-by-cell traversal to keep it there. `render_cell_walk()` is a tested-but-dormant
cell-by-cell fallback for traversing wide glyphs — it has no production caller,
because there is no runtime signal that `rows_diff` is ever unsafe under the margin
rule.

## Consequences

- The primary `rows_diff` path trusts vt100 never to wrap a wide glyph badly.
  Corruption is only possible if that rule breaks.
- `render_cell_walk()` carries `#[allow(dead_code)]` and is reachable only by a
  deliberate future edit — there is no chooser at the call site.
- If a corrupting cell is ever observed on the live `rows_diff` path (most plausibly
  in the wide-edge fixture), this decision reopens with a concrete trigger and the
  cell-walk gets wired in.

## Amendment — the margin rule holds, but a run can still position absolutely

The margin rule itself was re-checked against vt100 0.16.2 and stands, including the
case that would break it most directly: an explicit `ESC[1;10H` followed by a CJK
glyph on an empty 10-column grid puts the glyph at row 1, column 0, not straddling
the edge. No wide glyph's lead cell reaches `W-1`, so no painted cell crosses into
the right gutter.

What was wrong is the clause that used to follow: that the `rows_diff` path therefore
never positions absolutely. It does. vt100 repairs a row's soft-wrap state by
rewriting the row's last cell whenever the wrapped flag flips, and `MoveFromTo` emits
a relative `CUF` only for a forward move — a backward move falls through to an
absolute `CUP`. Feeding `0123456789X` to a 10-column grid in one frame yields the run
`0123456789ESC[1;10H9`; the same content split across two frames yields the relative
`ESC[9C9`. The trigger is a row's wrap state flipping inside one coalesced frame,
which the `wide-edge.cast` fixture does at 80 columns.

A run is painted after a single `move_to(left_margin, phys_row)`, so an absolute
coordinate inside it addresses the raw screen and leaves the band on both axes.
Re-expressing it in the band's own physical coordinates is
[ADR-014](0014-row-run-self-containment.md)'s job, alongside the row-final erase. What
this record guarantees is where a glyph's right edge lands, not where a run says to put
the cursor.

The cell-walk stays dormant. The reopening trigger in Consequences is a corrupting
*cell* on the `rows_diff` path, and this was not one — the cells vt100 produced were
right, the move that placed them was not, and traversing cells one at a time would
not have changed that.

The equivalence gate cannot catch this class of fault at all: it diffs vt100's grid
against wezterm's, never the bytes gutter writes to the real terminal. A misplaced
move is invisible to it by construction — as it is to the render tests, for a separate
reason ([ADR-001's amendment](0001-two-emulator-equivalence-gate.md)).

## Code anchors

- `src/render.rs` — the `rows_diff` paint and the dormant `render_cell_walk()`
- `src/geometry.rs` — the band-fit margin computation

The row-final erase rewrite is [ADR-014](0014-row-run-self-containment.md).
