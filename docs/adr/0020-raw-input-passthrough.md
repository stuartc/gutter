# ADR-020: Raw input passthrough

Status: Accepted

## Context

ADR-002 concluded that gutter must re-encode every keystroke, because crossterm
0.29 hands over a decoded `KeyEvent` with no access to the bytes the terminal
actually sent. That conclusion followed from crossterm owning the input fd, not
from anything about gutter.

Re-encoding turned out to be a source of quiet, permanent bugs rather than a
neutral cost. The encoder dropped modifiers on every non-character key, silently
produced nothing at all for Delete, Home, End, PageUp, PageDown, Insert and
F1–F12, always emitted arrows in CSI form even when the child had set DECCKM and
expected SS3, and dropped the ESC prefix on Alt+<char>. Each of those is a
separate table entry somebody has to get right, for every key, at every protocol
level, forever — and gutter has no way to know which level the *terminal* is
speaking, only which one the child asked for.

tmux, screen and zellij all forward raw bytes instead. gutter's case is cleaner
than tmux's: tmux re-encodes because detach and reattach mean the terminal a
child negotiated with may no longer be the one attached. gutter never detaches,
so the terminal sending the bytes is always the terminal the child negotiated
with.

## Decision

Read raw bytes from the outer tty and write them to the child's PTY untouched.
Extract only what genuinely cannot be transparent. gutter then understands no
keyboard protocol at all, and there is no encoding table to get wrong.

The scanner's grammar is small and closed:

- **Extract** SGR-1006 mouse reports (`ESC [ < b ; x ; y (M|m)`), because their
  coordinates need margin translation (ADR-005). gutter enables SGR-1006 on the
  outer terminal itself, so there is no "which mouse encoding" question on the
  input side.
- **Recognise** the reserved resize chord (ADR-016).
- **Track** bracketed-paste guards, so a literal chord byte inside a paste is
  forwarded rather than interpreted.
- **Buffer** sequences split across reads.
- Everything else is `write_all` to the PTY.

Taking the input fd back from crossterm is all-or-nothing: two readers on one fd
steal bytes from each other, so dropping crossterm's decoding for keys means
dropping it for mouse and resize too. SIGWINCH becomes gutter's own job (via
`signal-hook`, already a direct dependency), and the startup CPR that captures
the inline anchor (ADR-013) is hand-rolled — writing `ESC[6n` and reading to `R`
— rather than going through crossterm's event machinery, which would otherwise
buffer user keystrokes into a queue nobody drains once crossterm is out of the
input path. Leftover bytes from that read are prepended to the scanner's stream.

### The scanner runs on the render thread, not the reader thread

Thread 3 stays what Thread 1 already is: a dumb `read()` pump that forwards raw
chunks and interprets nothing. The scanner, the ESC-hold and the paste gate live
on the render thread, in `src/scan.rs`, beside the parser and the clock.

This is not a stylistic choice. The hold needs a deadline on the same injected
clock the coalescing loop runs on (ADR-007), so it can be tested on virtual time
and so a pending hold can wake the loop; the paste gate needs to read mode state
the render thread owns; and the chord and in-mode decoder feed a resize mode that
is render-thread state. Putting the scanner on Thread 3 would need a second clock,
a shared flag, or both — for no benefit. Keeping Thread 3 dumb also preserves
ADR-009's single-owner argument unchanged: one thread reads the fd, one thread
interprets, and they share a channel rather than state.

The outer terminal's keyboard mode becomes purely child-driven. The startup kitty
push existed only so crossterm could decode modified keys; under passthrough it
would make the terminal emit `CSI 27 u` for Escape to a child that never asked
for it. It is deleted, along with the capability probe that fed it — and with the
fabricated `CSI ? <flags> u` gutter used to answer the child's kitty query with.
Silence is the protocol's designed "I do not implement this", and it is now the
truth.

### The ESC-hold

The one genuine cost. To decide whether an incoming `ESC` begins a mouse report
or is a bare Escape keypress, the scanner withholds ambiguous prefixes until the
sequence completes or a short timeout fires. This is tmux's well-known
escape-time problem, and it means bare Escape reaches the child a few tens of
milliseconds later than it does today.

The hold arms only when a `read()` chunk ends with something incomplete, so in
practice it engages for exactly two things: a deliberate Escape keypress, and a
sequence split by the transport. Every other key arrives whole in one chunk and
is emitted with no timer at all.

Two rules make it safe. **On timeout, flush the whole pending buffer verbatim and
in order** — never split it, never reinterpret it; the child's own parser
reassembles across reads, so a late tail still lands correctly. **Nothing
overtakes the hold buffer** — a character arriving behind a held `ESC` is part of
`ESC <char>` (Alt+char), and reordering would corrupt it.

The timeout is a single `const ESC_HOLD` of 25 ms. tmux shipped 500 ms until
3.5 and now defaults to 10 ms; Neovim's `ttimeoutlen` is 50 ms. 25 ms sits in
that bracket and is within the noise of the 16 ms frame the coalescing loop
already costs every keystroke. `src/clock.rs`'s injectable clock makes the whole
timer virtual-clock testable, with no wall-clock sleeps.

## Consequences

- Delete, Home, End, PageUp, PageDown, Insert, Shift+Tab and F1–F12 work for the
  first time.
- Modifiers on every key survive, because gutter never inspects them.
- DECCKM stops being gutter's problem: whatever form the terminal sends is the
  form the child gets.
- The encoder, the kitty level enum, the outer capability probe, the flag push
  and the clamp are all deleted. There is no keyboard protocol knowledge left in
  the binary, and the child's `CSI ? u` gets silence.
- gutter owns SIGWINCH, the startup CPR read, and the tty read fd. Thread 3 is
  still the sole reader of that fd; the exclusivity argument in ADR-009 is
  unchanged.
- Bare Escape is slower by the hold period. Everything else is faster by one
  decode-and-re-encode round trip.
- The resize mode's key handling (ADR-016) becomes a small byte decoder, active
  only while the mode is.
- The outer mouse enable is hand-written without `?1015h`: the scanner recognises
  the SGR shape only, so a terminal that honoured the urxvt encoding would send
  reports gutter does not extract.
- A mis-scan is observable end to end: a child that echoes its stdin in caret
  notation shows exactly which bytes arrived.

## Code anchors

- `src/input.rs` — Thread 3, the dumb tty read pump
- `src/scan.rs` — the scanner: SGR extraction, paste tracking, the `ESC_HOLD`
  constant
- `src/chord.rs` — the reserved chord and the byte forms it matches
- `src/render.rs` — the hold deadline, the token walk, the passthrough write
- `src/sigwinch.rs` — Thread 5
- `src/msg.rs` — `Msg::Input` carries raw bytes; `Msg::Resize` carries SIGWINCH
- `src/main.rs` — the hand-rolled startup CPR

Supersedes [ADR-002](0002-keyboard-always-re-encoded.md). The mouse gate it feeds
is [ADR-005](0005-mouse-eager-capture-poll-gate.md); the chord it reserves is
[ADR-016](0016-modal-resize.md); the loop its deadline joins is
[ADR-007](0007-coalescing-loop.md).
