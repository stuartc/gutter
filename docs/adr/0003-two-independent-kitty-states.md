# ADR-003: Two independent kitty states

Status: Accepted

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

**Amended by [ADR-020](0020-raw-input-passthrough.md).** Both states are gone.
gutter no longer probes the outer terminal's capability, no longer pushes flags
of its own, and no longer tracks or clamps the child's level — under raw
passthrough there is nothing to encode at, so there is nothing to know. What
remains of this record is the reasoning for why the two sides were ever
independent, which is still the right frame for the relay that replaces them.
