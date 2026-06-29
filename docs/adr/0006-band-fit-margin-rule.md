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
edge sits within `W` when painted at the offset, and the `rows_diff` path never
needs to position absolutely or emit an unbounded erase. `render_cell_walk()` is a
tested-but-dormant cell-by-cell fallback for traversing wide glyphs — it has no
production caller, because there is no runtime signal that `rows_diff` is ever
unsafe under the margin rule.

## Consequences

- The primary `rows_diff` path trusts vt100 never to wrap a wide glyph badly.
  Corruption is only possible if that rule breaks.
- `render_cell_walk()` carries `#[allow(dead_code)]` and is reachable only by a
  deliberate future edit — there is no chooser at the call site.
- If a corrupting cell is ever observed on the live `rows_diff` path (most plausibly
  in the wide-edge fixture), this decision reopens with a concrete trigger and the
  cell-walk gets wired in.

## Code anchors

- `src/render.rs` — the `rows_diff` paint and the dormant `render_cell_walk()`
- `src/geometry.rs` — the band-fit margin computation

The row-final erase rewrite is [ADR-014](0014-row-run-self-containment.md).
