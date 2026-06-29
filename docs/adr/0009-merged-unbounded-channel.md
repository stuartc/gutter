# ADR-009: Merged unbounded channel

Status: Accepted

## Context

Four threads feed the render loop: PTY reader, input reader, waiter, and the loop
itself. A `select!` over several channels, or a blocking receive on a bounded one,
would let a PTY burst starve keystrokes. A shared parser behind a mutex would add a
serialisation point and a lock to every frame.

## Decision

The render thread reads from a single merged unbounded `mpsc::channel`. PTY data
arrives through a bounded `sync_channel(64)` staging step that blocks the PTY reader
when full; input and child-death arrive directly on the merge without blocking. The
render thread owns the parser exclusively — no `Arc<Mutex<…>>`, no shared mutable
state.

## Consequences

- A keystroke's `send()` never blocks, even under a multi-MB flood: the merge is
  unbounded and the backpressure lives upstream in the PTY staging channel.
- The parser has one owner, so there is no parser mutex and no lock on the frame
  path.
- Dispatch is a single `match msg` site.

## Code anchors

- `src/main.rs` — the unbounded merge and the bounded PTY staging seam
- `src/msg.rs` — the merged message enum

The deadline coalescer that drains this channel is [ADR-007](0007-coalescing-loop.md).
