# ADR-001: Two-emulator equivalence gate

Status: Accepted

## Context

We need a gate that catches emulation regressions — sequences gutter's parser
drops or mangles. Replaying bytes through gutter's own parser and comparing the
result to itself proves nothing: anything vt100 silently swallows matches itself,
so the bug hides.

## Decision

Replay the same settled VT byte stream through two parsers at the same width:
vt100 (gutter's live parser) and wezterm-term (an independent oracle). Diff the
grids cell by cell with a content-led classifier. A *corrupting* cell — one where
vt100 dropped or mangled a glyph wezterm rendered — fails the gate. Benign
convention divergences are reviewed once and recorded in an `.allowlist`. The gate
passes on zero corrupting cells.

## Consequences

- The gate uses two distinct parser types, enforced by a runtime assertion on
  their type names — a bug in one can't be masked by the other.
- Fixtures (`.cast` files) are captured at an exact PTY width and are SGR-dense
  (CJK, emoji), not thin ASCII, so the comparison exercises the hard cases.
- Benign divergences live in reviewed `.allowlist` files alongside the fixtures.
- The whole gate is feature-gated (`--features oracle`); the default build never
  pulls the wezterm dependency tree.

## Amendment — what the gate cannot see, and the same trap in the render tests

The gate compares two grids built from the *child's* bytes. It never looks at the bytes
gutter writes to the real terminal, so a fault in where those bytes are placed rather
than in what vt100 made of the child's output is invisible to it: a cursor move landing
on the wrong cell passes with zero corrupting cells, because neither emulator was ever
shown gutter's output.

The render-thread unit tests do read gutter's own output back, through `RecordingGrid`
in `src/terminal.rs` — the mock that stands in for the outer terminal. But
`RecordingGrid` is itself a `vt100::Parser`, and so is the model gutter reasons with
when it decides where to put the cursor (the row clipper's column tracker, ADR-014).
That is the Context above repeating itself one layer out: reading gutter's output back
through the same emulator gutter is built on proves the band arithmetic and says
nothing about the terminal. A mock built on vt100 cannot catch vt100 being wrong. The
concrete case is deferred wrap, which every real terminal implements and vt100 does
not: a glyph written into the last column leaves a real cursor on that column with the
wrap pending and a vt100 cursor past it, so a move that a real terminal would get wrong
reads back perfectly.

Both blind spots were live at once, and a wrong cursor rewrite in the row clipper
passed the whole suite. Closing the gap needs a test that replays the bytes *gutter*
emits — not the child's — through the independent emulator and asserts on the cells
that land. That is the painted-band check in `src/oracle/band.rs`, under the same
`oracle` feature as the gate: it paints one real frame into a `Tape` (an
`OuterTerminal` that keeps the byte stream instead of a screen), replays the tape
through wezterm-term at the *physical* terminal size, and diffs the band's rectangle
against the child's own `W`-column vt100 grid — including the configuration the rest
of the suite structurally cannot reach, where the band's right edge is the screen's
and deferred wrap is live on the cells being asserted. It also checks that nothing at
all lands outside the band's rectangle. Any future claim that gutter positions the
cursor correctly has to come from there or from a real PTY, never from
`RecordingGrid`.

## Code anchors

- `src/oracle/gate.rs` — replay pipeline and the two-emulator type assertion
- `src/oracle/cellview.rs` — the cell comparison surface
- `tests/equivalence_pty.rs` — the gate test
- `src/oracle/band.rs` — the painted-band check: `Tape`, `BandRect` and its tests
- `src/render.rs` — `paint_to_tape`, which drives one production frame into a `Tape`
