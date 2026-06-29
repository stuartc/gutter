# ADR-004: OSC-52 clipboard to a separate /dev/tty

Status: Accepted

## Context

The child writes clipboard requests as OSC-52 sequences. If we forward them on the
same file descriptor we use to repaint the band, a clipboard write and a frame
repaint can race for the fd's state.

## Decision

Write reconstructed OSC-52 requests to a separate `/dev/tty`, opened read-write at
startup. vt100 hands us the complete, already-decoded sequence via its
`copy_to_clipboard()` callback; we reconstruct `ESC ] 52 ; ty ; data BEL`
byte-for-byte (the base64 payload is forwarded verbatim, never re-encoded) and
write it to that separate fd. Errors are swallowed.

## Consequences

- The clipboard sink is a `Box<dyn Write>` injected into the callbacks: production
  passes `/dev/tty`, tests pass a buffer.
- If `/dev/tty` won't open, the sink degrades to `io::sink()` — clipboard is lost,
  nothing crashes.
- The write runs inline on the render thread, inside `parser.process()`, but on its
  own fd so it can't fight the repaint.
- Only the live parser's callbacks carry the real sink; baseline parsers get
  `io::sink()`.
- The read half is unused today; the fd is opened read-write as a hook for a later
  read-response relay.

## Code anchors

- `src/clipboard.rs` — sequence reconstruction and the fd seam
- `src/callbacks.rs` — the `copy_to_clipboard` callback and injection point
- `src/main.rs` — opening `/dev/tty`, degrading to `io::sink()`
