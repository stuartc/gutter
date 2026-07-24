# ADR-003: Two independent kitty states

Status: Superseded by [ADR-021](0021-child-driven-mode-relay.md)

## Context

The kitty keyboard protocol has two levels that must not be conflated: what the
outer terminal can support, and what the child has asked for. The child's request
is observable only through `CSI > N u` / `CSI < u` in its output, which vt100
surfaces to the `unhandled_csi` callback — there is no side-band.

## Decision

Track two independent kitty states. The outer terminal's capability is probed once
at startup and never changes; it clamps everything. The child's level is a push/pop
stack driven by the `unhandled_csi` watcher, clamped to the outer capability. The
watcher and the keyboard encoder are the only two paths into this state, and they
share a single callbacks struct that carries no hidden coupling — the kitty watcher
touches only `kitty_state`, never the clipboard.

## Consequences

- `KittyState` is a plain field on the callbacks struct, read lock-free from the
  render thread's encode path. No mutex.
- We never offer the child a level the real terminal can't support.
- Teardown pops only the level it actually pushed.
- Only the live parser carries kitty state; the diff-baseline parsers get a clamped
  `false`.

## Code anchors

- `src/callbacks.rs` — the shared struct and the `unhandled_csi` watcher
- `src/keyboard/kitty_state.rs` — the push/pop stack and clamp
- `src/main.rs` — the startup capability probe

---

Superseded by [ADR-021](0021-child-driven-mode-relay.md). gutter no longer holds
a kitty state of its own on either side: the outer terminal's capability is never
probed, and the child's requests are relayed out rather than clamped. In place of
the two clamped states there is one byte log of what the child asked for, so
teardown can undo exactly that and nothing more. Everything above is kept for the
record; none of it describes current behaviour.
