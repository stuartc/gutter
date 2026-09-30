# ADR-009: Merged unbounded channel

Status: Accepted

## Context

Four threads feed the render loop: the PTY reader, the input reader, the waiter and
the `SIGWINCH` thread. A `select!` over several channels, or a blocking receive on a
bounded one, would let a PTY burst starve keystrokes. A shared parser behind a mutex
would add a serialisation point and a lock to every frame.

## Decision

The render thread reads from a single merged unbounded `mpsc::channel` carrying one
message enum, `Msg`. PTY data arrives through a bounded `sync_channel` staging step
(`pty::STAGING_DEPTH`, 64 chunks) that blocks the PTY reader when full; input,
resize and the waiter's exit, stop and continue messages go straight onto the merge
without blocking. The render thread owns the parser exclusively — no
`Arc<Mutex<…>>`, no shared mutable state.

## Consequences

- A keystroke's `send()` never blocks, even under a multi-MB flood: the merge is
  unbounded and the backpressure lives upstream in the PTY staging channel.
- The parser has one owner, so there is no parser mutex and no lock on the frame
  path.
- Dispatch is a single `match msg` site (`dispatch` in `src/render.rs`).
- Each sender's messages arrive in the order it sent them, and nothing more. The
  PTY forwarder sends `Msg::PtyEof` after its last `Msg::Pty`, so seeing it proves
  the child's output has all landed; the shutdown drain relies on that
  ([ADR-013](0013-inline-anchor-scroll-paint.md)). Across senders there is no order,
  which is why the waiter's `ChildExited` can overtake the child's final bytes.

## Code anchors

- `src/main.rs` — the unbounded merge and the bounded PTY staging channel
- `src/pty.rs` — `STAGING_DEPTH`, the reader and `forward_to_merge`
- `src/msg.rs` — the merged message enum

The deadline coalescer that drains this channel is [ADR-007](0007-coalescing-loop.md).
