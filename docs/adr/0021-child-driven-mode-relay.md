# ADR-021: Child-driven outer keyboard modes

Status: Accepted

## Context

The classic byte tables have no way to say "Enter, with shift held" — both keys are
`0x0D`. A program that needs to tell them apart has to ask the terminal to switch to
a richer encoding first, either xterm's modifyOtherKeys (`CSI > 4 ; Pv m`) or the
kitty keyboard protocol (`CSI > flags u` and friends, with `CSI ? u` asking whether
the terminal speaks it at all).

gutter is a terminal emulator, so the child's request is addressed to gutter and
terminates inside its vt100 parser. But gutter generates no keystrokes: those come
from the real terminal, further out. The party that needs to hear the request never
did, and the party that heard it could do nothing with it. Under ADR-003 gutter went
further and answered the capability query itself — a claim about a terminal it had
never asked — which made a child settle for kitty on a terminal that ignored it and
never try modifyOtherKeys, the one protocol that would have worked.

ADR-020 made the input path transparent, which is the prerequisite: with crossterm
gone, a reply from the real terminal reaches the child as ordinary bytes.

## Decision

Relay the child's keyboard-mode negotiation out to the real terminal, and implement
no keyboard protocol at all. gutter keeps only enough state to put the terminal back
the way it found it.

**The allowlist is closed and short.** Matched on the intermediate, the parameters
and the final character, in `unhandled_csi`:

| Sequence | What it is |
|---|---|
| `CSI > flags u` | kitty push |
| `CSI < n u` | kitty pop |
| `CSI = flags ; mode u` | kitty set |
| `CSI ? u` | kitty capability query |
| `CSI > 4 ; Pv m` | modifyOtherKeys |
| `CSI > 4 m` | modifyOtherKeys, restore the terminal's default |

Everything else is dropped, as it always was. All six change only how the terminal
*encodes* the keys it sends: no cursor movement, no glyphs, no attributes, no screen
mode. The DECSETs that would be dangerous — mouse reporting, the alt screen,
bracketed paste, application cursor keys — are implemented by vt100 and so never
reach this code path at all.

**Default deny, because the failure modes are not symmetric.** Relaying something
that should not have been relayed writes outside gutter's band model: the repaint
does not know it happened, the diff baseline becomes a lie, and the child has
escaped its columns. Failing to relay something merely leaves a feature unavailable
— which is the status quo for every sequence in the table. So the list starts narrow
and widens on evidence, with each addition carrying a sentence on why it cannot
touch the display.

**Canonical bytes, never a param join.** The callback receives parsed parameters,
not the bytes that produced them, and vte pushes the pending parameter — initial
value zero — unconditionally before dispatching. A child's paramless `CSI ? u` is
therefore indistinguishable from `CSI ? 0 u`. Re-serialising the parameters would
send the query out as `CSI ? 0 u`, whose meaning is undefined, and a bare pop as
`CSI < 0 u`, which pops nothing — the whole point of the relay dying quietly while
every test that compared gutter's output against its own re-serialisation stayed
green. The matcher already knows which shape it matched, so it emits a fixed byte
string for that shape and recovers the protocol's default for an omitted parameter.

**The capability query is a proxy, and silence is an answer.** A terminal that
speaks the kitty protocol replies `CSI ? flags u`; one that does not replies nothing
at all — there is no "no" reply, so silence *is* the no. gutter cannot help here:
any answer it invents is a claim about a terminal it has not asked. So the question
goes out, and the answer comes back on the input fd and reaches the child through
ADR-020's passthrough with no relay code involved. The absence of an answer relays
itself, for free, by gutter doing nothing. The match is on the paramless form only:
a reply-shaped `CSI ? flags u` appearing in the child's *output* is dropped, so a
child that echoes what it reads cannot drive the question back out again and again.

**gutter remains the authority for device queries.** DA1, DSR status and above all
`CSI 6 n` are still answered locally, because the cursor-position reply must be in
the child's W-grid coordinates — the child believes it owns a `W`-column terminal
starting at column 1, while the real cursor sits at `left_margin + col`.

**The state is a byte log, and it is the undo.** gutter keeps the exact canonical
bytes it relayed, oldest first, plus the last non-zero modifyOtherKeys value.
Storing bytes rather than parsed flags means gutter never has to know that the set
form's mode parameter selects between assign, or, and and-not: replay is "emit what
you emitted before", correct by construction. Teardown pops the depth the log
records, clears a set made at depth zero (the one kitty state a pop cannot restore),
and turns modifyOtherKeys off if and only if gutter turned it on. A pop from the
child is clamped to that same depth: a level pushed before gutter launched belongs
to whoever pushed it, and reaching into a stack gutter did not build would break the
program above it.

## Consequences

- Shift+Enter works: the child's modifyOtherKeys request reaches the terminal, and
  the terminal starts sending a sequence distinct from `\r`.
- gutter makes no capability claim of its own. On a terminal that speaks kitty, the
  child learns so from the terminal; on one that does not, the child falls back
  exactly as it would bare.
- Teardown and the suspend cycle undo and re-apply the child's modes in ADR-010's
  slot for input-encoding restores. A child killed mid-negotiation is handled by the
  log being authoritative rather than predictive: whatever it records at exit is what
  gets undone.
- **A known hole: the DA1 ordering race.** A child that pairs its kitty query with a
  DA1 query — the usual timeout-free way to conclude "no kitty" — gets gutter's
  synthesised DA1 answer in microseconds while the kitty answer makes a full round
  trip to the terminal and back. The DA1 answer essentially always wins, so such a
  child concludes "no kitty" even on a terminal that speaks it. On iTerm2 that
  conclusion is correct; on a terminal implementing both it costs fidelity; on kitty
  and foot, which implement no modifyOtherKeys, it can leave the child with no
  working protocol. The fix is a ~50 ms hold on a synthesised DA1 when a query was
  relayed in the same batch, and it is built if the manual check on a kitty-capable
  terminal shows the symptom.
- Sniffing the terminal's reply so gutter could learn the real capability was
  rejected: it re-introduces the protocol parsing this redesign exists to delete, and
  it would still have to guess how long to wait before concluding "no reply".
- A proxied reply arriving while resize mode is active is swallowed by the in-mode
  default. Vanishingly rare and self-limiting — the child falls back — and recorded
  here so it is a known edge rather than a mystery.

## Code anchors

- `src/relay.rs` — the allowlist matcher, the canonical emitter, the byte log, and
  `reset_bytes()` / `replay_bytes()`
- `src/callbacks.rs` — the `unhandled_csi` branch and the buffered relay bytes
- `src/render.rs` — the drain in the `Msg::Pty` arm, and the reset/replay in
  `run_teardown`, `park` and `unpark`
- `src/terminal.rs` — `OuterTerminal::relay` and the mock's `Call::Relay`

Supersedes [ADR-003](0003-two-independent-kitty-states.md). The input path this
depends on is [ADR-020](0020-raw-input-passthrough.md); the restore slot it fills is
[ADR-010](0010-ordered-teardown.md) and, on the suspend cycle,
[ADR-019](0019-suspend-resume-cycle-ordering.md). The mouse modes it must never
relay are [ADR-005](0005-mouse-eager-capture-poll-gate.md)'s.
