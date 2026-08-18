# ADR-014: Row-run self-containment at offset

Status: Accepted

## Context

vt100 builds each row for a grid exactly `W` columns wide. When we paint that row at
a margin offset, two things can bleed outside the band: the previous row's trailing
attributes crossing the row boundary, and `ESC[K` (erase to right edge), which would
erase to the *real* terminal's edge — a reverse-video status line would flood the
gutter.

## Decision

**Amended below — read the amendment before acting on this section.** The `CUB` back to
the cursor is gone, a prepared run is correct at exactly one placement rather than at any
offset, and the row-final `ESC[K` is not the only erase rewritten.

Make every row self-contained within the `W`-wide rectangle before painting it, in
`prepare_row_into()`. Prepend `ESC[m` to reset attributes so nothing bleeds down from the
row above. Rewrite the row-final `ESC[K` into a `W`-bounded fill: `(W - col)` spaces
under the active SGR, then a `CUB` back to the cursor, so the erase stops at column
`W` and any relative bytes after it still compute from the right position.

## Consequences

- Every painted row starts with `ESC[m`.
- Every erase is rewritten into a fill of the band's own columns; `ESC[3K` is dropped.
  See *The other erases* below.
- The column tracker in `clip_row_to_width_into()` has to follow every cursor move —
  absolute `CUP`/`CHA`, relative `C`/`D`, backspace — to get the fill length right.

## Amendment — a run's absolute moves are re-expressed in the band's physical coordinates

Tracking the column is necessary but not sufficient. A run is written after a single
`move_to(left_margin, phys_row)` and nothing inside it re-establishes that origin, so
every coordinate in the run is read by the terminal in raw screen coordinates. An
absolute `CUP` or `CHA` therefore leaves the band on both axes — the wrong column,
and for `CUP` the wrong row too. The cursor stamps a cell outside the rectangle the
caller believes it painted, and since the diff baseline only models the band, nothing
ever repaints over it: the cell survives until that row's real content changes.

These moves are ordinary output, not a corner case — vt100 emits one whenever a row's
wrapped flag flips and the repair moves backwards
([ADR-006](0006-band-fit-margin-rule.md)).

`clip_row_to_width_into` rewrites both kinds into an absolute move onto the same cell
in the band's own physical coordinates: `ESC[{phys_row + 1};{left_margin + target + 1}H`,
where `target` is the in-band column the original named, clamped to `W - 1` so no
emitted move can put the cursor on the first gutter column. The row-final `ESC[K` fill
ends with that same move. The `CUB` back to the cursor described in the Decision above
is gone — it was a third instance of the same fault, and the oldest of them: on a row
whose fill ran to the screen's right edge, the walk back landed one column early and
everything written after it was shifted a column left.

### Why absolute and not a relative hop

Because of deferred wrap. Writing a glyph into the *screen's* last column leaves the
real cursor sitting on that column with the wrap pending, while the clipper's column
tracker has already counted the glyph and moved on to column `W`. Wherever the band's
right edge is the screen's right edge the two disagree by one, and a `CUB(1)` from
there lands a column short and destroys a cell. Checked against wezterm-term: on a
10-column screen, `MoveTo(0, 0)` then `0123456789ESC[1D9` renders `0123456799` — the
`8` overwritten — where the absolute form renders `0123456789`.

That is not a corner case, it is the default configuration. `resolve_width` clamps an
absolute width to the terminal's own and `margin(Center, cols, cols)` is 0, so plain
`gutter claude` on an 80-column terminal runs with `left_margin == 0` and `W == cols`,
as does every `--width full` run. An absolute move re-establishes the position
whatever the cursor was doing, so it is right on a narrow band and a full-width one
alike.

### What the clipper now needs from its caller

`clip_row_to_width_into` takes a `Placement { left_margin, phys_row, grid_row }`: it
cannot name a physical cell without knowing where the band starts. Both call sites —
the per-row diff paint and `emit_scroll_stream` — build the `Placement` and use it for
their own `term.move_to` as well, so the move that establishes the origin and the clip
that assumes it cannot disagree.

`grid_row` places nothing. It feeds a `debug_assert` that a `CUP`'s row parameter names
the run's own grid row, which is what makes discarding that parameter safe rather than
merely convenient. Every position vt100 builds inside a run does name that row: in both
`write_contents_formatted` and `write_contents_diff` the *destination* of every
`MoveFromTo` is a `Pos { row, .. }` for the row being written, and `MoveTo` writes the
destination. On the scroll path the *source* can be the row above — that is the seed
`rows_formatted` hands in when the previous row wrapped — but the source never reaches
the bytes.

The assert guards a debug build only. `debug_assert_eq!` compiles out of every release
build, so a future vt100 that named another row would panic the test suite and have its
row parameter dropped in silence in a shipped binary. That silence is still
inside the band: the rewrite emits an absolute `CUP` on the run's own physical row with
the column clamped to `[0, W)`, so the worst a mismatched row can do is stamp a cell on
the wrong line of the band. It cannot reach the gutter, which is the fault this whole
amendment is about. Dropping the move instead would be worse — the glyphs after it
would land at whatever column the cursor happened to be at, and the tracker would
desync — so the clamp is the release behaviour, deliberately.

So the requirement on a run leaving the clipper is: every position it carries is a
physical cell this `Placement` computed, and no erase runs past `W`.

### What vt100 actually puts in a `rows_diff` run

Measured over gutter's own model — the live parser plus a `prev` rebuilt from
`contents_formatted`, scanning every non-empty `rows_diff` run — across all three
fixtures at their declared 80×24 in chunks of 1, 7, 64, 256, 4096 and whole-stream,
plus a synthetic sweep at widths 4/8/10/20/80 and 2/3/5/10 rows over an alphabet built
to force backward movement (wrap flips, `ESC[K`, `ESC[2J`, explicit `CUB`, `CHA`, CJK,
`ICH`/`DCH`/`ECH`/`IL`/`DL`, `SU`/`SD`, tabs, a literal backspace, absolute `CUP`s).
186,716 runs: `CUF` 80,383, `CUP` 26,258, `EL` 89,674, `ECH` 37,767, `SGR` 84,816 —
and zero `CUB`, zero bare backspace, zero `CHA`.

So the only backward positioning vt100 puts in a `rows_diff` run is the absolute `CUP`,
and that is the one the clipper re-expresses. The `C` arm is copied through verbatim and
is safe: vt100 emits a `CUF` only when the source column is strictly below the target and
the target is inside the row, so the cursor is never in the pending-wrap state when one
runs. Nothing left in a clipped run can slip by a column.

The `CHA` and `D` arms are defensive. vt100 0.16.2 has no writer for either — its own
`term.rs` has no CHA in it at all, and `MoveRight` is the only relative move it can
write — so neither can appear today. They stay because if vt100 ever did emit a backward
relative move it would need the same rewrite as the `CUP`, and the arm is where that
would go.

### The `\r` and `\n` arms

vt100's `MoveFromTo` writer has a branch that emits a bare `\r\n` when the target is
the start of the next row, and one that emits a bare `\r` for the start of the current
one. Neither byte can reach a clipped run today — a `rows_diff` run keeps
`from.row == to.row` structurally, and all three `rows_formatted` sites are blocked by
the wrapping seed — but the branch is live elsewhere in vt100 (`contents_diff`), and
both bytes are invisible to the `RecordingGrid` mock the render tests read back
through: only the painted-band check could see the misplacement. So they get arms of
their own rather than falling into the C0 catch-all, on the same footing as the `CHA`
and `CUB` arms above.

A `\r` copied through would put the cursor on *physical* column 0 — the left gutter
whenever the margin is non-zero — and every glyph after it with it. It is rewritten
into the absolute move to the band's own column 0, the same shape as the `CHA` arm,
with the tracker set to 0.

A `\n` has no in-band translation at all: a run is defined for one physical row, so
written out it would paint the rest of the run a row below the placement, and on the
screen's bottom row scroll the whole physical screen with nothing in the baseline to
repair it. It is dropped, with a `debug_assert!` beside it, exactly as the `CUP` arm
handles a row parameter it cannot honour: loud in a debug build, silent and inside the
band in a shipped one. The tracker is left where the preceding `\r` put it.

### The backspace arm is not defensive — the scroll path needs it

The measurement above scanned `rows_diff` runs. It says nothing about the other producer:
`emit_scroll_stream` feeds the clipper `rows_formatted` output, and the two go through
different writers in vt100.

`Screen::rows_diff` calls `write_contents_diff` with `wrapping: false`, `prev_wrapping:
false` and a per-row `prev_pos` of `{ row: i, col: start }`, which switches off every
branch in that writer that can emit a literal `0x08`. That is why the measurement found
zero backspaces, and it is structural, not luck. `rows_formatted` does the opposite: it
threads `wrapping = row.wrapped()` down from the row above, and `write_contents_formatted`
opens the run with `' ' 0x08 ESC[X` whenever the row above wrapped and this row's first
cell is default. It can emit `' ' 0x08` a second time at the trailing erase, when the
erase starts at column 0 of a wrapped-into row. Both are routine on a scrolling frame.

So the tracker's `0x08` case is live code on the scroll path. Delete it and the tracker
reads column 1 where the cursor is really at column 0 for the whole of that run: the
row-final `ESC[K` fill then comes out one space short and the band's last column is left
unpainted every time a row scrolls in under a wrapped one.

### The other erases

A run is painted after a bare `move_to(left_margin, phys_row)` and nothing re-bases the
line, so every erase in it is read against the *physical* row: passed through, `ESC[2K`
would erase the whole row and `ESC[1K` everything from physical column 0 to the cursor —
the entire left gutter. Either would take out whatever the user's shell left there, and
since the diff baseline only models the band nothing repaints over the hole.

Both are rewritten the same way `ESC[K` is: an absolute move to the band's own column 0,
the fill under the active SGR, and an absolute move back to the tracked column so the
rest of the run still aligns. `ESC[1K`'s fill includes the cursor's own cell, which is
what ECMA-48 EL 1 erases. At the pending-wrap column the move back is skipped — the
tracker's `W` names the first gutter column, and the fill has already left the cursor
where the erase found it.

`ESC[3K` erases the scrollback's saved copy of the line and paints nothing, so there is
no band to bound it to, and a parameter of 4 or more is undefined, so a terminal ignores
it. Both are dropped rather than forwarded — the saved lines `ESC[3K` would clear are the
outer terminal's, holding history the band never owned — which leaves no erase that
reaches the physical line.

None of them can arrive today. `ClearRowForward` (`ESC[K`) is the only row erase in
vt100 0.16.2's writer, so the other arms are unreachable from a child stream.

### Two more leaks, both on the scroll path

Bounding what a row *writes* is only half of it. A scrolling frame also moves the
screen and paints over rows nothing holds a baseline for, and both escaped the
rectangle:

**A `newline` under a painted row's attributes.** `emit_scroll_stream` and
`scroll_to_make_room` scroll the real terminal with `OuterTerminal::newline`, and a
terminal fills the line that scrolls in with the *active* background across its full
width. The row painted just before it ends on whatever attribute its run left live —
for an `ESC[K`-erased row under a background SGR, that colour — so the line arriving
at the bottom came in tinted from column 0 to the screen's last, both gutters
included. `clear_gutter` only runs on a geometry change, so it stayed. The reset now
belongs to `newline` itself rather than to its callers: every caller scrolls from
wherever the previous row left the cursor, so there is no call site that wants the
other behaviour and none that can forget.

**A run that describes less than the row it lands on.** `rows_formatted` encodes only
the cells that differ from a blank one, so a band row that is now empty produces an
empty run — and the rows `emit_scroll_stream` paints in place still carry the last
frame's paint, which no diff accounts for. The empty run wrote nothing, the glyphs
under it survived, and the next `newline` carried them up the screen into rows the
baseline never revisits. Those in-place rows now blank the band's columns first, using
the clipper's own `ESC[K` fill so the blank stops at the band's right edge; a real
`ESC[K` there would be the gutter flood this whole record is about. Rows written after
a `newline` need none of it — the line they land on scrolled in blank.

Both were invisible to every test in the repo before the painted-band check, and the
first is invisible to vt100 by construction: it has no background-colour erase on
scroll, so the mock the render tests read back through fills the scrolled-in line with
blanks whatever the SGR.

### The title is now loose

"Self-containment" described a run that could be painted at any offset. That is no
longer what the clipper produces: a clipped run is correct at one placement and
nowhere else, and painting it at a different margin or row would put every rewritten
move somewhere wrong, silently. The decision itself is unchanged — a painted row stays
inside its `W`-wide rectangle — so the record keeps its number and title; only the way
it is met has moved from "carry no absolute position" to "carry the caller's own".

### What the absolute coordinate assumes

An absolute `CUP` names one cell only because gutter never puts the outer terminal into
origin mode (DECOM, `ESC[?6h`) and never sets a scroll region (DECSTBM, `ESC[…r`).
Under either the terminal reads the coordinate relative to the region instead of the
screen, and every rewritten move would address the wrong cell. Nothing in gutter emits
them: `src/relay.rs`'s allowlist is keyboard-mode sequences only, and no DECSET is ever
relayed ([ADR-022](0022-absorbed-mode-mirroring.md)). If either ever changes, this
rewrite has to change with it.

### Why the tests did not catch the relative form

The relative version of this rewrite passed every test in the repo. Neither the render
tests nor the equivalence gate can see a cursor move landing on the wrong cell, for two
independent reasons recorded in
[ADR-001's amendment](0001-two-emulator-equivalence-gate.md). Read it before trusting a
green suite on anything in this file. The check that does cover it is the painted-band
one in `src/oracle/band.rs`, which replays gutter's own bytes through wezterm-term at the
physical screen size; a new claim about where a rewritten move lands belongs there.

## Code anchors

- `src/rowclip.rs` — the clip algorithm, `Placement`, the cursor tracking and its tests
- `src/render.rs` — `prepare_row_into()` and the `ESC[m` prepend; `prepare_row_over_into()`
  for a row painted over cells with no baseline; the diff paint and `emit_scroll_stream()`
  build the `Placement` each run is clipped for
- `src/terminal.rs` — `OuterTerminal::write_row`, whose contract is that a prepared run
  is painted at the placement it was clipped for and nowhere else, and
  `OuterTerminal::newline`, which resets the SGR before it scrolls
- `src/oracle/band.rs` — the painted-band check, the only test in the repo that can see
  a rewritten move, or an erase, land outside the band

The edge-safety rule this rests on is [ADR-006](0006-band-fit-margin-rule.md).
