# ADR-002: Keyboard always re-encoded

Status: Superseded by [ADR-020](0020-raw-input-passthrough.md)

## Context

crossterm 0.29 hands us a decoded `KeyEvent` with no access to the raw bytes the
terminal sent. There is nothing to forward verbatim — the decoded event is the
only thing we have.

## Decision

Always re-encode each `KeyEvent` into bytes before writing it to the child. The
child's negotiated level chooses the form: legacy VT tables when kitty is off,
kitty `CSI … u` when it is on.

## Consequences

- `encode_key()` is the only keyboard path. There is no raw-forward shim.
- The same physical key can produce different bytes depending on level. Shift+Enter
  and Enter are distinct sequences under kitty but both collapse to `\r` under
  legacy.
- A mis-encode is observable: a test asserts on the grid the child paints back.

## Code anchors

- `src/keyboard/encode.rs` — `encode_key()` and the legacy/kitty byte forms
- `src/render.rs` — input dispatch re-encodes at the child's live level

See also [ADR-003](0003-two-independent-kitty-states.md) for how the level is tracked.

---

Superseded by [ADR-020](0020-raw-input-passthrough.md). The premise — that
crossterm hands us a decoded `KeyEvent` with no raw bytes — held only while
crossterm owned the input fd. gutter now reads the tty itself, so the raw bytes
are available and re-encoding is no longer forced. Everything above is kept for
the record; none of it describes current behaviour.
