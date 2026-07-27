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

## Amendment — a distinct open, not a fd that differs from stdout

This record was written while the band was painted on stdout, so "a separate file
descriptor from the one we repaint on" was in practice "not stdout", and the code and
its test said so in those words. Since [ADR-023](0023-controlling-terminal-fd-model.md)
the band paints on `/dev/tty` as well, so the clipboard's handle is no longer told
apart by *which device* it points at. The rule is unchanged, only stated properly: what
matters is that it is its own `open`. Two independent opens of the same terminal are two
independent file descriptions, each with its own offset and flags, so a clipboard write
and a frame repaint still cannot fight over fd state.

Two smaller corrections follow from the same change. `open_tty_read_write` is now shared:
`anchor::open_input_tty` makes the keyboard handle through it and reads that one, so it is
the *clipboard's* read half specifically that stays unused. And the degrade to `io::sink()`
is now all but unreachable — startup has already refused the run if no terminal would
open — but it is kept, because it costs a match arm and the alternative is a crash on a
path nobody can rehearse.

## Code anchors

- `src/clipboard.rs` — sequence reconstruction and the fd seam
- `src/callbacks.rs` — the `copy_to_clipboard` callback and injection point
- `src/main.rs` — opening `/dev/tty`, degrading to `io::sink()`

The fd model this open is one of three in is
[ADR-023](0023-controlling-terminal-fd-model.md).
