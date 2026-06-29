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

## Code anchors

- `src/oracle/gate.rs` — replay pipeline and the two-emulator type assertion
- `src/oracle/cellview.rs` — the cell comparison surface
- `tests/equivalence_pty.rs` — the gate test
