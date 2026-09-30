# ADR-014: Row-run self-containment at offset

Status: Accepted

## Context

vt100 builds each row for a grid exactly `W` columns wide. gutter paints a changed
row with one `move_to(left_margin, phys_row)` followed by the row's byte run, and
nothing inside the run re-establishes that origin, so the terminal reads every
coordinate and every erase in it against the raw screen. Several things in a run can
therefore reach outside the band:

- **Attributes from the row above.** The previous row's trailing SGR carries across
  the bare `move_to`.
- **Erases.** `ESC[K` erases to the *real* terminal's right edge — a reverse-video
  status line floods the right gutter. `ESC[1K` and `ESC[2K` would reach from
  physical column 0, taking out the whole left gutter.
- **Absolute moves.** An absolute `CUP` or `CHA` names a raw screen cell, so it leaves
  the band on both axes — the wrong column, and for `CUP` the wrong row too. These are
  ordinary output, not a corner case: vt100 emits a `CUP` whenever a row's wrapped
  flag flips and the repair moves backwards ([ADR-006](0006-band-fit-margin-rule.md)).

Anything that lands outside the band is never repainted, because the diff baseline
only models the band. The stray cell survives until that row's real content changes.

## Decision

Before a row is painted, make it stay inside its `W`-wide rectangle **at the exact
placement it is painted at**. `prepare_row_into` in `src/render.rs` does this, using
`clip_row_to_width_into` in `src/rowclip.rs`:

- Prepend `ESC[m`, so nothing bleeds down from the row above.
- Rewrite every erase into a fill of the band's own columns under the active SGR,
  followed by an absolute move back to the tracked column so the rest of the run still
  aligns. `ESC[K` fills from the cursor to `W`, `ESC[1K` from the band's column 0
  through the cursor's own cell (ECMA-48 EL 1), `ESC[2K` the whole band row.
  `ESC[3K` and any parameter of 4 or more are dropped.
- Rewrite every absolute move into an absolute move onto the same cell in the band's
  physical coordinates: `ESC[{phys_row + 1};{left_margin + target + 1}H`, where
  `target` is the in-band column the original named, clamped to `W - 1` so no move
  can put the cursor on the first gutter column. A `\r` is rewritten the same way, to
  the band's column 0; a `\n` is dropped.
- Track the cursor's in-band column across the run — absolute `CUP`/`CHA`, relative
  `C`/`D`, backspace, glyph widths — so each fill has the right length and each move
  back goes to the right cell.

The same rule governs the paints that scroll the screen: `OuterTerminal::newline`
resets the SGR before it scrolls, and a row repainted in place during a scrolling
frame blanks the band's columns before it is written. For the rest of the run gutter
also turns the host terminal's autowrap off.

A clipped run is correct at one placement and nowhere else. The title's
"self-containment" originally meant a run that could be painted at any offset; the
rule it protects — a painted row stays inside its `W`-wide rectangle — is unchanged,
but it is now met by carrying the caller's own coordinates rather than carrying none.

## Consequences

- Every painted row starts with `ESC[m`.
- No erase in a painted run reaches the physical line: each is a bounded fill or
  dropped.
- Every position in a painted run is an absolute `CUP` onto a physical cell the
  caller's `Placement` computed. `OuterTerminal::write_row`'s contract says so: a
  prepared run is painted at the placement it was clipped for and nowhere else.
- The clipper needs the placement from its caller (see below), and its column tracker
  has to follow every cursor move in the run.
- The rewrite depends on gutter never enabling origin mode or a scroll region on the
  outer terminal (see *What the absolute coordinate assumes*).

## Why absolute and not a relative hop

The first version of this rewrite used relative moves: after the `ESC[K` fill it
walked back to the cursor with a `CUB`, and the relative form of the wrap-flip repair
restamped the last cell with `CUB(1)`. Both broke because of deferred wrap.

Writing a glyph into the *screen's* last column leaves the real cursor on that column
with the wrap pending, while the clipper's tracker has already counted the glyph and
moved on to column `W`. Wherever the band's right edge is the screen's right edge the
two disagree by one, and a `CUB(1)` from there lands a column short and destroys a
cell. Checked against wezterm-term: on a 10-column screen, `MoveTo(0, 0)` then
`0123456789ESC[1D9` renders `0123456799` — the `8` overwritten — where the absolute
form renders `0123456789`. The `CUB` after a fill failed the same way: on a row whose
fill ran to the screen's right edge, the walk back landed a column early and
everything written after it shifted a column left.

That is the default configuration, not a corner case. `resolve_width` clamps an
absolute width to the terminal's own and `margin(Center, cols, cols)` is 0, so plain
`gutter claude` on an 80-column terminal runs with `left_margin == 0` and
`W == cols`, as does every `--width full` run. An absolute move re-establishes the
position whatever the cursor was doing, so it is right on a narrow band and a
full-width one alike.

## What the clipper needs from its caller

`clip_row_to_width_into` takes a `Placement { left_margin, phys_row, grid_row }`: it
cannot name a physical cell without knowing where the band starts. Both producers —
the per-row diff paint and `emit_scroll_stream` — build the `Placement` and use it for
their own `term.move_to` as well, so the move that sets the origin and the clip that
assumes it cannot disagree.

`grid_row` places nothing. It feeds a `debug_assert` that a `CUP`'s row parameter
names the run's own grid row, which is what makes discarding that parameter safe.
Every position vt100 builds inside a run does name that row: in both
`write_contents_formatted` and `write_contents_diff` the *destination* of every
`MoveFromTo` is a `Pos { row, .. }` for the row being written, and `MoveTo` writes the
destination. On the scroll path the *source* can be the row above — the seed
`rows_formatted` passes in when the previous row wrapped — but the source never
reaches the bytes.

The assert only runs in debug builds. In a release build a future vt100 that named
another row would have its row parameter dropped silently. That is still inside the
band: the rewrite emits an absolute `CUP` on the run's own physical row with the
column clamped to `[0, W)`, so the worst a mismatched row can do is stamp a cell on
the wrong line of the band, never in the gutter. Dropping the move instead would be
worse — the glyphs after it would land wherever the cursor happened to be and the
tracker would lose sync — so the clamp is the deliberate release behaviour.

## What vt100 actually puts in a `rows_diff` run

Measured over gutter's own model — the live parser plus a `prev` rebuilt from
`contents_formatted`, scanning every non-empty `rows_diff` run — across all three
fixtures at their declared 80×24 in chunks of 1, 7, 64, 256, 4096 and whole-stream,
plus a synthetic sweep at widths 4/8/10/20/80 and 2/3/5/10 rows over an alphabet built
to force backward movement (wrap flips, `ESC[K`, `ESC[2J`, explicit `CUB`, `CHA`, CJK,
`ICH`/`DCH`/`ECH`/`IL`/`DL`, `SU`/`SD`, tabs, a literal backspace, absolute `CUP`s).
186,716 runs: `CUF` 80,383, `CUP` 26,258, `EL` 89,674, `ECH` 37,767, `SGR` 84,816 —
and zero `CUB`, zero bare backspace, zero `CHA`.

So the only backward positioning vt100 puts in a `rows_diff` run is the absolute
`CUP`, and that is the one the clipper rewrites. `CUF` (`C`) is copied through
verbatim and is safe: vt100 emits one only when the source column is strictly below
the target and the target is inside the row, so the cursor is never waiting on a
deferred wrap when it runs.

The `CHA` (`G`) and `CUB` (`D`) arms are defensive. vt100 0.16.2 has no writer for
either — its `term.rs` has no `CHA` at all, and `MoveRight` is the only relative move
it can write. The `CHA` arm already rewrites to an absolute move; the `D` arm only
updates the tracker and copies the move through, and is where the same rewrite as the
`CUP` would go if vt100 ever emitted a backward relative move.

Only one erase is reachable today: `ClearRowForward` (`ESC[K`) is the only row erase
in vt100 0.16.2's writer. The `ESC[1K`, `ESC[2K` and `ESC[3K` arms cannot fire from a
child stream, but passed through, any of them would erase outside the band — which is
why each is rewritten or dropped rather than forwarded. `ESC[3K` paints no cell (it
clears the scrollback's saved copy of the line, and those saved lines are the outer
terminal's history, which the band never owned); a parameter of 4 or more is
undefined and a terminal ignores it. At the deferred-wrap column the move back after
a fill is skipped: the tracker's `W` names the first gutter column, and the fill has
already left the cursor where the erase found it.

## The `\r` and `\n` arms

vt100 writes either byte in one place only: the `\r\n` its `MoveFromTo` writer emits
when the target is the start of the next row. Neither can reach a clipped run today —
a `rows_diff` run keeps `from.row == to.row` by construction, and `rows_formatted`,
though it seeds each row with the wrapping state of the row above, guards the
cross-row move out at all three places it could write one — but the branch is live
elsewhere in vt100 (`contents_diff`). Both bytes are also invisible to
the `RecordingGrid` mock the render tests read back through; only the painted-band
check would see the misplacement. So they get arms of their own rather than falling
into the catch-all for other control bytes.

A `\r` copied through would put the cursor on *physical* column 0 — the left gutter
whenever the margin is non-zero — and every glyph after it with it. It is rewritten
into the absolute move to the band's own column 0, with the tracker set to 0.

A `\n` has no in-band translation: a run is defined for one physical row, so written
out it would paint the rest of the run a row below the placement, and on the screen's
bottom row scroll the whole screen with nothing in the baseline to repair it. It is
dropped, and the tracker stays where the preceding `\r` put it. For the only shape
vt100 writes — `\r\n`, "start of the next row", where the caller has already put the
cursor on the run's row — rewriting the `\r` and dropping the `\n` is the correct
translation, not a fallback.

Unlike the `CUP` arm, neither carries a `debug_assert!`: a mismatched `CUP` row is a
case the clipper cannot honour, while these two are handled. What the arms cannot do
is notice that vt100's row writers changed shape in a version bump, and a runtime
assert is the wrong tool for that — it fires only if a developer happens to run the
offending stream, and it takes a live session down when it does. That job belongs to
`no_row_run_carries_a_bare_carriage_return_or_line_feed` in `src/render.rs`, which
drives the wrap-flip stream and the wide-edge fixture through both producers and
asserts no run carries either byte. It runs in every build profile, so an upgrade
that loosens one of those guards fails in CI.

## The backspace arm is needed on the scroll path

The measurement above scanned `rows_diff` runs. It says nothing about the other
producer: `emit_scroll_stream` feeds the clipper `rows_formatted` output, and the two
go through different writers in vt100.

`Screen::rows_diff` calls `write_contents_diff` with `wrapping: false`,
`prev_wrapping: false` and a per-row `prev_pos` of `{ row: i, col: start }`, which
switches off every branch in that writer that can emit a literal `0x08`. That is why
the measurement found no backspaces, and it follows from the code rather than luck.
`rows_formatted` does the opposite: it passes `wrapping = row.wrapped()` down from the
row above, and `write_contents_formatted` opens the run with `' ' 0x08 ESC[X` whenever
the row above wrapped and this row's first cell is default. It can emit `' ' 0x08` a
second time at the trailing erase, when the erase starts at column 0 of a
wrapped-into row. Both are routine on a scrolling frame.

So the tracker's `0x08` case runs on the scroll path. Without it the tracker reads
column 1 where the cursor is really at column 0 for the rest of that run: the
row-final `ESC[K` fill comes out one space short and the band's last column is left
unpainted every time a row scrolls in under a wrapped one.

## Two more leaks, both on the scroll path

Bounding what a row *writes* is only half of it. A scrolling frame also moves the
screen and paints over rows nothing holds a baseline for, and both of these escaped
the rectangle before the rules below.

**A `newline` under a painted row's attributes.** `emit_scroll_stream` and
`scroll_to_make_room` scroll the real terminal with `OuterTerminal::newline`, and a
terminal fills the line that scrolls in with the *active* background across its full
width. The row painted just before ends on whatever attribute its run left live — for
an `ESC[K`-erased row under a background SGR, that colour — so the new bottom line
arrived tinted from column 0 to the screen's last, both gutters included, and
`clear_gutter` only runs on a geometry change. The reset belongs to `newline` itself
rather than its callers: every caller scrolls from wherever the previous row left the
cursor, so no call site wants the other behaviour and none can forget it.

**A run that describes less than the row it lands on.** `rows_formatted` encodes only
the cells that differ from a blank one, so a band row that is now empty produces an
empty run — and the rows `emit_scroll_stream` paints in place still carry the last
frame's paint, which no diff accounts for. The empty run wrote nothing, the old glyphs
survived, and the next `newline` carried them up the screen into rows the baseline
never revisits. Rows painted in place therefore go through `prepare_row_over_into`,
which blanks the band's columns first using the clipper's own `ESC[K` fill, so the
blank stops at the band's right edge; a real `ESC[K` there would be the gutter flood
this record exists to prevent. Rows written after a `newline` need none of this — the
line they land on scrolled in blank.

Both were invisible to every test before the painted-band check, and the first is
invisible to vt100 by construction: it does not fill a scrolled-in line with the
background colour, so the mock the render tests read back through shows a blank line
whatever the SGR.

## What the absolute coordinate assumes

An absolute `CUP` names one cell only because gutter never puts the outer terminal
into origin mode (DECOM, `ESC[?6h`) and never sets a scroll region (DECSTBM,
`ESC[…r`). Under either, the terminal reads the coordinate relative to the region
instead of the screen, and every rewritten move would address the wrong cell. Nothing
in gutter emits them: `src/relay.rs`'s allowlist is keyboard-mode sequences only, and
no DECSET is ever relayed ([ADR-022](0022-absorbed-mode-mirroring.md)). If either
ever changes, this rewrite has to change with it.

**Host autowrap (DECAWM, `?7`) is off for the whole run.** gutter writes `ESC[?7l` at
setup, next to the eager mouse capture, `ESC[?7h` in the ordered restore, and writes
`ESC[?7l` again on unpark after a Ctrl+Z/`fg` — without that, one suspend cycle would
leave autowrap on for the rest of the run, and nothing in gutter checks the mode
again. Like mouse capture it is a host-side mode gutter sets for itself, and nothing
about it is relayed in either direction, so it agrees with ADR-022: the child's own
DECAWM stays inside the band's `W`-column grid.

Unlike DECOM and DECSTBM, autowrap-off is not something the rewrite depends on; it
limits the damage when something else goes wrong. Every position gutter emits is
already absolute, so nothing in a clipped run wants the host to wrap on its behalf.
What wrap-off buys is that a byte which does overrun the screen's last column is
clamped there instead of landing on the next row and marking that line soft-wrapped,
where the next reflow joins the two and the damage outlives the frame. The gutter
clear, sized from a stale width, used to be one such overrun;
[ADR-024](0024-terminal-relative-gutter-erases.md) removed it and records the
overrun that remains.

The absolute-`CUP` rewrite is still required with autowrap off. Wrap-off removes
deferred wrap from the host, but not the disagreement the rewrite exists for: at the
band's last column the cursor stops at `W - 1` while the tracker has counted `W`, so a
relative hop computed from the tracker still lands a column early. Both relative forms
described under *Why absolute and not a relative hop* still corrupt a cell when
replayed with autowrap off, and
`the_relative_hop_scars_still_corrupt_under_autowrap_off` in `src/oracle/band.rs`
keeps that proven.

The teardown `ESC[?7h` is **unconditional**: gutter does not read the terminal's
DECAWM before turning it off, so a user whose terminal had autowrap off when gutter
started gets it turned back on. That is the same trade the mouse capture disable
makes ([ADR-005](0005-mouse-eager-capture-poll-gate.md)) — one enable at startup, one
disable at teardown, no attempt to restore what was there before — and it is accepted
for the same reason: autowrap on is what a shell expects, and querying the mode would
add a round trip to startup for a setting almost nobody changes. The write is one
best-effort step of the [ADR-010](0010-ordered-teardown.md) restore, after the mouse
disable and before `show_cursor`, whose flush is what actually puts the restore on
screen ([ADR-023](0023-controlling-terminal-fd-model.md)).

## Why the tests did not catch the relative form

The relative version of this rewrite passed every test in the repo. Neither the render
tests nor the equivalence gate can see a cursor move landing on the wrong cell, for
two separate reasons recorded in
[ADR-001's first amendment](0001-two-emulator-equivalence-gate.md). Read it before trusting
a green suite on anything in this file. The check that does cover it is the
painted-band check in `src/oracle/band.rs`, which replays gutter's own bytes through
wezterm-term at the physical screen size; a new claim about where a rewritten move
lands belongs there.

## Code anchors

- `src/rowclip.rs` — `clip_row_to_width_into`, `Placement`, the column tracker and
  its tests
- `src/render.rs` — `prepare_row_into` and the `ESC[m` prepend;
  `prepare_row_over_into` for a row painted over cells with no baseline; the diff
  paint and `emit_scroll_stream`, which build the `Placement` each run is clipped for;
  `no_row_run_carries_a_bare_carriage_return_or_line_feed`
- `src/terminal.rs` — `OuterTerminal::write_row` and its one-placement contract;
  `OuterTerminal::newline`, which resets the SGR before it scrolls;
  `OuterTerminal::set_autowrap`
- `src/main.rs` — the autowrap-off at setup; `src/render.rs`'s `ordered_restore` and
  `unpark` for the restore and the re-assert
- `src/oracle/band.rs` — the painted-band check, the only test in the repo that can
  see a rewritten move, or an erase, land outside the band

The edge-safety rule this rests on is [ADR-006](0006-band-fit-margin-rule.md).
