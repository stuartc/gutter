# ADR-006: Band-fit margin rule

Status: Accepted

## Context

A row is painted at a left-margin offset, so anything that reaches past column `W`
lands in the right gutter. Two things can do that: a wide (two-column) glyph at the
band's edge, and an erase such as `ESC[K`, which runs to the real terminal's edge.
This record covers the glyphs. The erases are
[ADR-014](0014-row-run-self-containment.md)'s.

## Decision

Rely on vt100's margin rule for right-edge safety: a wide glyph never has its lead
cell at column `W-1`; vt100 wraps it to the next row instead. Every cell's right edge
therefore sits within `W` when painted at the offset, and the `rows_diff` paint needs
no cell-by-cell walk to keep it there.

`render_cell_walk()` is a tested cell-by-cell painter that handles wide glyphs
explicitly. It stays dormant — no production caller, no runtime switch — because
nothing has shown `rows_diff` to be unsafe under the margin rule.

## Consequences

- The `rows_diff` path trusts vt100's margin rule. A wide glyph can only corrupt the
  right gutter if that rule breaks.
- `render_cell_walk()` carries `#[allow(dead_code)]`. Putting it live is a deliberate
  code change, not a flag.
- `primary_path_does_not_walk_cells` reads `render_once`'s source and fails if a
  per-cell walk creeps into it.
- The trigger to reopen this decision is a corrupting cell on the `rows_diff` path.
  The likeliest place to see one is the wide-edge equivalence gate
  (`wide_edge_equivalence_gate_passes`, over `wide-edge.cast`), whose failure message
  names this record. If it fires, the cell-walk gets wired in.

## Amendment — the margin rule holds, but a run can still position absolutely

The margin rule was re-checked against vt100 0.16.2 and stands, including the case
that would break it most directly: an explicit `ESC[1;10H` followed by a CJK glyph on
an empty 10-column grid puts the glyph at row 1, column 0, not straddling the edge. No
wide glyph's lead cell reaches `W-1`, so no painted cell crosses into the right gutter.

An earlier version of this record went on to claim that the `rows_diff` path
therefore never positions the cursor absolutely. It does. vt100 repairs a row's
soft-wrap state by rewriting the row's last cell whenever the wrapped flag flips, and
`MoveFromTo` emits a relative `CUF` only for a forward move — a backward move falls
through to an absolute `CUP`. Feeding `0123456789X` to a 10-column grid in one frame
yields the run `0123456789ESC[1;10H9`; the same content split across two frames
yields the relative `ESC[9C9`. The trigger is a row's wrap state flipping inside one
coalesced frame, which the `wide-edge.cast` fixture does at 80 columns.

A run is painted after a single `move_to(left_margin, phys_row)`, so an absolute
coordinate inside it addresses the raw screen and leaves the band on both axes.
Re-expressing it in the band's own physical coordinates is
[ADR-014](0014-row-run-self-containment.md)'s job. This record guarantees where a
glyph's right edge lands, not where a run says to put the cursor.

The cell-walk stays dormant. The reopening trigger is a corrupting *cell* on the
`rows_diff` path, and this was not one: the cells vt100 produced were right, the move
that placed them was not, and walking the cells one at a time would not have changed
that.

The equivalence gate cannot catch this kind of fault: it diffs vt100's grid against
wezterm's, never the bytes gutter writes to the real terminal. The render unit tests
cannot either, for a different reason
([ADR-001's first amendment](0001-two-emulator-equivalence-gate.md)). The painted-band
check in `src/oracle/band.rs` is the one that sees it.

## Code anchors

- `src/render.rs` — `render_once`'s `rows_diff` paint, the dormant
  `render_cell_walk()`, and `primary_path_does_not_walk_cells`
- `src/oracle/gate.rs` — `wide_edge_equivalence_gate_passes`, the reopening trigger

The row-run rewrites are [ADR-014](0014-row-run-self-containment.md).
