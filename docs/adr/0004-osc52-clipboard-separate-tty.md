# ADR-004: OSC-52 clipboard on its own open of the terminal

Status: Accepted

## Context

The child writes clipboard requests as OSC-52 sequences. If gutter forwarded them
through the same file descriptor it repaints the band on, a clipboard write and a
frame repaint could race for that descriptor's state.

## Decision

Write reconstructed OSC-52 requests through the clipboard's **own open** of the
terminal gutter resolved at startup (ADR-023), opened read-write. vt100 hands over the
complete, already-decoded sequence through its `copy_to_clipboard()` callback; gutter
reconstructs `ESC ] 52 ; ty ; data BEL` byte for byte (the base64 payload is forwarded
verbatim, never re-encoded) and writes it to that handle.

What keeps the two apart is that the clipboard's handle is a separate `open`, not that
it points at a different device — the band paints on the same terminal. Two independent
opens are two independent file descriptions, each with its own offset and flags, so a
clipboard write and a frame repaint cannot fight over them. A `dup` of the sink would
share one description and lose that.

## Consequences

- The clipboard sink is a `Box<dyn Write + Send>` injected into the callbacks:
  production passes the terminal handle, tests pass a buffer.
- If the open fails, startup prints `gutter: terminal unavailable, clipboard disabled`
  to stderr and the sink falls back to `io::sink()` — clipboard is lost, nothing
  crashes. This is all but unreachable, since startup has already refused the run if no
  terminal would open (ADR-023), but it is kept: it costs one match arm, and the
  alternative is a crash on a path nobody can rehearse.
- A failed write is reported on stderr (`gutter: clipboard write failed`) and otherwise
  ignored.
- The write runs inline on the render thread, inside `parser.process()`, but on its
  own handle so it cannot fight the repaint.
- Only the live parser's callbacks carry the real sink; the baseline parsers get
  `io::sink()`.
- The handle is opened read-write so a later read-response relay has somewhere to
  read from. The clipboard's own read half is unused. `open_tty_read_write` is shared:
  `anchor::open_input_tty` makes the keyboard handle through it too, and that one is
  read.

## History

This record was first written while the band was painted on stdout, so "a separate
descriptor from the one we repaint on" was in practice "not stdout", and the code and
its test said so in those words. When ADR-023 moved the band onto the terminal device
as well, the rule was restated as above; the decision itself did not change.

## Code anchors

- `src/clipboard.rs` — `open_tty_read_write`, `reconstruct_osc52`, `forward_osc52`
- `src/callbacks.rs` — the `copy_to_clipboard` callback and the injected sink
- `src/main.rs` — the clipboard's open of the resolved path, and the fall-back to
  `io::sink()`

The terminal model this open is one of three in is
[ADR-023](0023-controlling-terminal-fd-model.md).
