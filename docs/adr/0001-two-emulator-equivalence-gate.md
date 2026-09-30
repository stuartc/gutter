# ADR-001: Two-emulator equivalence gate

Status: Accepted

## Context

We need a check that catches emulation regressions — sequences gutter's parser
drops or mangles. Replaying bytes through gutter's own parser and comparing the
result to itself proves nothing: anything vt100 silently swallows matches itself,
so the bug hides.

## Decision

Replay the same settled VT byte stream through two parsers at the same width:
vt100 (gutter's live parser) and wezterm-term (an independent emulator). Diff the
grids cell by cell and classify each difference by its content. A *corrupting*
cell — different characters, meaning vt100 dropped or mangled a glyph wezterm
rendered — fails the gate. A *benign* cell — same character, different attributes —
is a convention difference between the two emulators, not a defect in vt100; each
one is reviewed once and recorded in an `.allowlist`. The gate passes on zero
corrupting cells.

wezterm-term is a second opinion, not ground truth: the bar is zero corrupting
cells, not zero differences from wezterm.

## Consequences

- The two sides are distinct parser types, and the replay asserts it on their type
  names, so a slip that fed both sides through one parser would fail rather than
  quietly compare vt100 with itself.
- Fixtures (`.cast` files) are captured at an exact PTY size and replayed at that
  size. They are SGR-dense and include CJK and emoji at the band edge, not thin
  ASCII, so the comparison exercises the hard cases.
- Benign divergences live in reviewed `.allowlist` files beside the fixtures. An
  unreviewed one does not fail the gate's own verdict, but the gate test fails on
  it until a reviewer adds it to the allowlist.
- The whole gate is feature-gated (`--features oracle`); the default build never
  pulls in the wezterm dependency tree.

## Amendment — what the gate cannot see, and the same trap in the render tests

The gate compares two grids built from the *child's* bytes. It never looks at the
bytes gutter writes to the real terminal, so a fault in where those bytes land — as
opposed to what vt100 made of the child's output — is invisible to it. A cursor move
that lands on the wrong cell passes with zero corrupting cells, because neither
emulator was ever shown gutter's output.

The render-thread unit tests do read gutter's own output back, through
`RecordingGrid` in `src/terminal.rs`, the mock that stands in for the outer
terminal. But `RecordingGrid` is itself a `vt100::Parser`, and so is the model
gutter reasons with when it decides where to put the cursor (the row clipper's
column tracker, [ADR-014](0014-row-run-self-containment.md)). That is the Context
above repeating itself one layer out: reading gutter's output back through the
emulator gutter is built on checks the band arithmetic and says nothing about the
terminal. A mock built on vt100 cannot catch vt100 being wrong. The concrete case is
deferred wrap, which every real terminal implements and vt100 does not: a glyph
written into the screen's last column leaves a real cursor on that column with the
wrap pending, and a vt100 cursor one column past it. A move that a real terminal
would get wrong reads back perfectly.

Both blind spots were live at once, and a wrong cursor rewrite in the row clipper
passed the whole suite. Catching that needs a test that replays the bytes *gutter*
writes — not the child's — through the independent emulator and asserts on the
cells that land. That is the painted-band check in `src/oracle/band.rs`, under the
same `oracle` feature as the gate. It paints real frames through the production
render path into a `Tape`, replays the tape through wezterm-term at the *physical*
screen size, and diffs the band's rectangle against the child's own `W`-column vt100
grid. That includes the configuration the rest of the suite cannot reach, where the
band's right edge is the screen's and deferred wrap is live on the cells being
checked. A second pass checks that nothing at all lands outside the band's
rectangle. Any claim that gutter positions the cursor correctly has to come from
this check or from a real PTY, never from `RecordingGrid`.

## Amendment — the check replays production bytes, and covers the primary fixture

`Tape` wraps a `CrosstermTerminal<Vec<u8>>` (built with `from_writer`) and
delegates every `OuterTerminal` method to it, so the bytes replayed through
wezterm-term are the bytes the production path writes to a real terminal, not a
second copy of the code that produces them. A hand-maintained copy would let a
placement bug that lives only in `CrosstermTerminal`'s emitters pass the check and
still ship. The tape also starts with host autowrap off, as gutter's own setup
leaves the real terminal.

The cases include `claude-code-flow.cast`, the SGR-dense fixture the equivalence
gate treats as primary, at its recorded 80×24, in two geometries: `margin == 0`
(band edge on the screen edge, deferred wrap live on the checked cells) and a
centred band with gutters either side. Both pass with zero corrupting cells and
needed no new `.allowlist` entry.

The fixture enters the alt screen in its first byte and never leaves it. That does
not matter to `painted_band_matches_the_child_grid_on_a_real_terminal`, which
compares the band with the child grid whichever screen is active. The centred case
also runs `nothing_is_painted_outside_the_band`: on the alt screen that check has no
shell history to seed around the band, so it expects blank cells outside it rather
than sentinel text. The margin-0 case is skipped there because the band fills the
screen and there is no outside to check. Neither case reaches the scroll and
blank-row checks, which need the primary screen and build their own cases.

## Code anchors

- `src/oracle/gate.rs` — the replay, diff and classifier, the type-name assertion,
  and the gate tests (`recorded_target_equivalence_gate_passes`,
  `wide_edge_equivalence_gate_passes`, `gate_uses_two_different_emulators`)
- `src/oracle/cellview.rs` — the cell view both emulators are compared through
- `tests/fixtures/*.cast`, `*.allowlist` — the fixtures and their reviewed benign
  divergences
- `src/oracle/band.rs` — the painted-band check: `Tape`, `BandRect` and its tests
- `src/render.rs` — `paint_frames_to_tape`, which drives production frames into a
  `Tape`
- `tests/equivalence_pty.rs` — not the gate: end-to-end runs of the same fixtures
  through the real binary over a real PTY, checking that gutter survives them
