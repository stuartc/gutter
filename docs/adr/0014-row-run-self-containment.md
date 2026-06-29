# ADR-014: Row-run self-containment at offset

Status: Accepted

## Context

vt100 builds each row for a grid exactly `W` columns wide. When we paint that row at
a margin offset, two things can bleed outside the band: the previous row's trailing
attributes crossing the row boundary, and `ESC[K` (erase to right edge), which would
erase to the *real* terminal's edge — a reverse-video status line would flood the
gutter.

## Decision

Make every row self-contained within the `W`-wide rectangle before painting it, in
`prepare_row()`. Prepend `ESC[m` to reset attributes so nothing bleeds down from the
row above. Rewrite the row-final `ESC[K` into a `W`-bounded fill: `(W - col)` spaces
under the active SGR, then a `CUB` back to the cursor, so the erase stops at column
`W` and any relative bytes after it still compute from the right position.

## Consequences

- Every painted row starts with `ESC[m`.
- Only the row-final `ESC[K` is rewritten; `ESC[1K` and `ESC[2K` are copied verbatim.
- The column tracker in `clip_row_to_width()` has to follow every cursor move —
  absolute `CUP`/`CHA`, relative `C`/`D`, backspace — to get the fill length right.

## Code anchors

- `src/rowclip.rs` — the clip algorithm, cursor tracking, and its tests
- `src/render.rs` — `prepare_row()` and the `ESC[m` prepend

The edge-safety rule this rests on is [ADR-006](0006-band-fit-margin-rule.md).
