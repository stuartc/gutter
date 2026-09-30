# ADR-020: Raw input passthrough

Status: Accepted

## Context

ADR-002 concluded that gutter must re-encode every keystroke, because crossterm 0.29
hands over a decoded `KeyEvent` with no access to the bytes the terminal actually
sent. That conclusion followed from crossterm owning the input fd, not from anything
about gutter.

Re-encoding was a source of quiet, permanent bugs rather than a neutral cost. The
encoder dropped modifiers on every non-character key, produced nothing at all for
Delete, Home, End, PageUp, PageDown, Insert and F1–F12, always emitted arrows in CSI
form even when the child had set DECCKM and expected SS3, and dropped the ESC prefix
on Alt+<char>. Each of those is a separate table entry somebody has to get right, for
every key, at every protocol level, for ever — and gutter has no way to know which
level the *terminal* is speaking, only which one the child asked for.

tmux, screen and zellij all forward raw bytes instead. gutter's case is simpler than
tmux's: tmux re-encodes because detach and reattach mean the terminal a child
negotiated with may no longer be the one attached. gutter never detaches, so the
terminal sending the bytes is always the terminal the child negotiated with.

## Decision

Read raw bytes from the outer tty and write them to the child's PTY untouched.
Extract only what genuinely cannot be passed through. gutter then understands no
keyboard protocol at all, and there is no encoding table to get wrong.

The scanner (`src/scan.rs`) splits input into a small, closed set of tokens:

- **Mouse reports.** SGR-1006 reports (`ESC [ < b ; x ; y (M|m)`) are extracted,
  because their coordinates need margin translation (ADR-005). gutter enables
  SGR-1006 on the outer terminal itself, so there is only one mouse encoding to
  recognise.
- **Whole escape sequences and ordinary byte runs**, so the render thread can match
  the reserved resize chord and the in-mode resize keys (ADR-016) against complete
  units. The scanner itself knows neither the chord nor whether resize mode is
  active.
- **Bracketed pastes.** Between the paste guards nothing is extracted or matched, so a
  chord byte inside a paste is forwarded rather than acted on (ADR-022).
- **String sequences.** OSC/DCS/APC/PM/SOS payloads are delimited and forwarded, so an
  arbitrary payload — a terminal's colour or clipboard reply — is never matched
  against anything.
- A sequence split across reads is buffered until it completes.

Everything else is `write_all` to the PTY.

Taking the input fd back from crossterm is all-or-nothing: two readers on one fd steal
bytes from each other, so dropping crossterm's decoding for keys means dropping it for
mouse and resize too. SIGWINCH becomes gutter's own job (via `signal-hook`, already a
direct dependency), and the startup cursor-position query that captures the inline
anchor (ADR-013) is hand-rolled — writing `ESC[6n` and reading to `R` — rather than
going through crossterm's event machinery, which would otherwise buffer the user's
keystrokes into a queue nobody drains. Any bytes that read collects besides the reply
are sent to the render thread as the first `Msg::Input`, ahead of anything Thread 3
reads.

### The scanner runs on the render thread, not the reader thread

Thread 3 stays what Thread 1 already is: a dumb `read()` pump that forwards raw chunks
and interprets nothing. The scanner, the ESC-hold and the paste gate live on the
render thread, beside the parser and the clock.

This is not a matter of taste. The hold needs a deadline on the same injected clock
the coalescing loop runs on (ADR-007), so it can be tested on virtual time and so a
pending hold can wake the loop; the paste gate reads mode state the render thread
owns; and the chord and the in-mode keys feed a resize mode that is render-thread
state. Putting the scanner on Thread 3 would need a second clock, a shared flag, or
both, for no benefit. Keeping Thread 3 dumb also leaves ADR-009's single-owner
argument unchanged: one thread reads the fd, one thread interprets, and they share a
channel rather than state.

The outer terminal's keyboard mode becomes purely child-driven. gutter used to push a
kitty level at startup only so crossterm could decode modified keys; under
passthrough that push would make the terminal send `CSI 27 u` for Escape to a child
that never asked for it. It is gone, along with the capability probe that fed it and
the made-up `CSI ? <flags> u` gutter used to answer the child's kitty query with.
Silence is the protocol's designed way of saying "I do not implement this", and it is
now the truth. What the child asks for is relayed to the terminal instead (ADR-021).

### The ESC-hold

This is the one real cost. To decide whether an incoming `ESC` begins a mouse report
or is a bare Escape keypress, the scanner withholds an incomplete sequence until it
completes or a short timeout fires. This is tmux's well-known escape-time problem, and
it means bare Escape reaches the child slightly later than it would without the hold.

The hold arms only when a `read()` chunk ends partway through a sequence, so in
practice it engages for exactly two things: a deliberate Escape keypress, and a
sequence split by the transport. Every other key arrives whole in one chunk and is
passed on with no timer at all.

Two rules make it safe. **On timeout, flush the whole pending buffer verbatim and in
order** — never split it, never reinterpret it; the child's own parser reassembles
across reads, so a late tail still lands correctly. **Nothing overtakes the hold
buffer** — a character arriving behind a held `ESC` is part of `ESC <char>`
(Alt+char), and reordering would corrupt it.

The timeout is a single constant, `ESC_HOLD`, of 25 ms. tmux shipped 500 ms until
3.5 and now defaults to 10 ms; Neovim's `ttimeoutlen` is 50 ms. 25 ms sits between
them and is within the noise of the 16 ms frame the coalescing loop already costs
every keystroke. The injectable clock in `src/clock.rs` lets the whole timer be tested
on virtual time, with no wall-clock sleeps.

## Consequences

- Delete, Home, End, PageUp, PageDown, Insert, Shift+Tab and F1–F12 all reach the
  child.
- Modifiers on every key survive, because gutter never inspects them.
- Cursor-key encoding is no longer gutter's problem: whatever form the terminal sends
  is the form the child gets. For the terminal to send the SS3 form the child asked
  for, DECCKM has to be mirrored outward (ADR-022).
- The encoder, the kitty level enum, the outer capability probe, the startup push and
  the clamp are all deleted. No keyboard protocol knowledge is left in the binary.
- gutter owns SIGWINCH, the startup cursor-position read, and the tty read fd. Thread
  3 is still the only reader of that fd; the exclusivity argument in ADR-009 is
  unchanged.
- Bare Escape is slower by the hold period. Every other key is faster by one
  decode-and-re-encode round trip.
- Resize mode's key handling (ADR-016) becomes a small byte decoder, active only while
  the mode is.
- The outer mouse enable is written by hand without `?1015h`, so the terminal only
  ever sends the one report shape the scanner extracts (ADR-005).
- A scanning mistake is visible end to end: a child that echoes its stdin in caret
  notation shows exactly which bytes arrived.

## Code anchors

- `src/input.rs` — Thread 3, the dumb tty read pump
- `src/scan.rs` — the scanner: `Token`, SGR extraction, paste tracking, the
  `ESC_HOLD` constant
- `src/chord.rs` — the reserved chord and the byte forms it matches
- `src/render.rs` — `apply_message` (the feed and the hold deadline) and
  `walk_tokens` (the token walk and the passthrough write)
- `src/sigwinch.rs` — Thread 5
- `src/msg.rs` — `Msg::Input` carries raw bytes; `Msg::Resize` carries SIGWINCH
- `src/anchor.rs` — `probe_cursor_row`, the hand-rolled startup cursor-position query
- `src/main.rs` — seeds the probe's leftover bytes as the first `Msg::Input`

Supersedes [ADR-002](0002-keyboard-always-re-encoded.md). The mouse gate it feeds is
[ADR-005](0005-mouse-eager-capture-poll-gate.md); the chord it reserves is
[ADR-016](0016-modal-resize.md); the loop its deadline joins is
[ADR-007](0007-coalescing-loop.md). The keyboard-mode relay that depends on this
passthrough is [ADR-021](0021-child-driven-mode-relay.md).
