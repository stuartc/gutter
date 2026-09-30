# ADR-021: Child-driven outer keyboard modes

Status: Accepted

## Context

The classic byte tables have no way to say "Enter, with shift held" — both keys are
`0x0D`. A program that needs to tell them apart has to ask the terminal to switch to a
richer encoding first, either xterm's modifyOtherKeys (`CSI > 4 ; Pv m`) or the kitty
keyboard protocol (`CSI > flags u` and friends, with `CSI ? u` asking whether the
terminal speaks it at all).

gutter is a terminal emulator, so the child's request is addressed to gutter and ends
inside its vt100 parser. But gutter generates no keystrokes: those come from the real
terminal, further out. The party that needed to hear the request never did, and the
party that heard it could do nothing with it. Under ADR-003 gutter went further and
answered the capability query itself — a claim about a terminal it had never asked —
which made a child settle for kitty on a terminal that ignored it, and never try
modifyOtherKeys, the one protocol that would have worked.

ADR-020 made the input path transparent, which this depends on: with crossterm gone,
a reply from the real terminal reaches the child as ordinary bytes.

## Decision

Relay the child's keyboard-mode negotiation out to the real terminal, and implement no
keyboard protocol at all. gutter keeps only enough state to put the terminal back the
way it found it.

**The allowlist is closed and short.** Matched in `unhandled_csi` on the
intermediate, the parameters and the final character:

| Sequence | What it is |
|---|---|
| `CSI > flags u` | kitty push |
| `CSI < n u` | kitty pop |
| `CSI = flags ; mode u` | kitty set |
| `CSI ? u` | kitty capability query |
| `CSI > 4 ; Pv m` | modifyOtherKeys |
| `CSI > 4 m` | modifyOtherKeys, restore the terminal's default |

Everything else is dropped. All six change only how the terminal *encodes* the keys it
sends: no cursor movement, no glyphs, no attributes, no screen mode. The DECSETs that
would be dangerous — mouse reporting, the alt screen, bracketed paste, application
cursor keys — are implemented by vt100 and so never reach this code path at all.
Requiring an intermediate is also what keeps out `CSI u`, the ANSI restore-cursor.

**Default deny, because the two ways to get it wrong are not equally bad.** Relaying
something that should not have been relayed writes outside gutter's model of the
band: the repaint does not know it happened, the diff baseline no longer matches the
screen, and the child has escaped its columns. Failing to relay something merely
leaves a feature unavailable — which was already the case for every sequence in the
table. So the list starts narrow and widens on evidence, with each addition carrying a
sentence on why it cannot touch the display.

**Canonical bytes, never a parameter join.** The callback receives parsed parameters,
not the bytes that produced them, and vte pushes the pending parameter — initial value
zero — unconditionally before dispatching. A child's paramless `CSI ? u` is therefore
indistinguishable from `CSI ? 0 u`. Re-serialising the parameters would send the query
out as `CSI ? 0 u`, whose meaning is undefined, and a bare pop as `CSI < 0 u`, which
pops nothing — and it would fail quietly, because a test that compares gutter's
output with its own re-serialisation stays green. The matcher
already knows which shape it matched, so it emits a fixed byte string for that shape
and fills in the protocol's default for an omitted parameter.

**The capability query is passed through, and silence is an answer.** A terminal that
speaks the kitty protocol replies `CSI ? flags u`; one that does not replies nothing at
all — there is no "no" reply, so silence *is* the no. Any answer gutter invented would
be a claim about a terminal it has not asked. So the question goes out, and the answer
comes back on the input fd and reaches the child through ADR-020's passthrough with no
relay code involved. The absence of an answer passes through the same way, by gutter
doing nothing. The match is on the paramless form only: a reply-shaped
`CSI ? flags u` in the child's *output* is dropped, so a child that echoes what it
reads cannot send the question back out again and again.

**gutter still answers device queries itself.** DA1, DSR status and above all
`CSI 6 n` are answered locally, because the cursor-position reply must be in the
child's `W`-column coordinates — the child believes it owns a `W`-column terminal
starting at column 1, while the real cursor sits at `left_margin + col`.

**The state is a log of bytes, and it is the undo.** gutter keeps the canonical bytes
of each kitty push and set still in effect, oldest first — a pop removes the entries
it undoes, and a set replaces the set before it on the same level — plus the last
non-zero modifyOtherKeys value. Storing bytes rather than parsed flags means gutter
never has to know that the set form's mode parameter chooses between assign, or, and
and-not: replay is "emit what you emitted before", correct by construction. Teardown
pops the depth the log records, clears a set made at depth zero (the one piece of
kitty state a pop cannot restore), and turns modifyOtherKeys off (`CSI > 4 ; 0 m`) if
and only if gutter turned it on. A pop from the child is clamped to that same depth,
and dropped entirely if gutter has relayed no push: a level pushed before gutter
launched belongs to whoever pushed it, and reaching into a stack gutter did not build
would break the program above it.

## Consequences

- Shift+Enter works: the child's modifyOtherKeys request reaches the terminal, and the
  terminal starts sending a sequence distinct from `\r`.
- gutter makes no capability claim of its own. On a terminal that speaks kitty, the
  child learns so from the terminal; on one that does not, the child falls back
  exactly as it would without gutter.
- Teardown and the suspend cycle undo the child's modes in ADR-010's slot for
  input-encoding restores, and resume re-applies them from the log, oldest first
  (ADR-019). A child killed mid-negotiation is covered because the log records what
  was actually relayed, not what was expected next: whatever it holds at exit is what
  gets undone.
- **A known hole: the DA1 ordering race.** A child that pairs its kitty query with a
  DA1 query — the usual timeout-free way to conclude "no kitty" — gets gutter's
  locally made DA1 answer in microseconds, while the kitty answer makes a full round
  trip to the terminal and back. The DA1 answer essentially always wins, so such a
  child concludes "no kitty" even on a terminal that speaks it. On iTerm2 that
  conclusion is correct; on a terminal implementing both protocols the child settles
  for modifyOtherKeys when kitty was available; on kitty and foot, which implement no
  modifyOtherKeys, it can leave the child with no working protocol. The fix would be a
  ~50 ms hold on a locally made DA1 answer when a query was relayed in the same batch.
  It is not built; it will be if a manual check on a kitty-capable terminal shows the
  symptom.
- Sniffing the terminal's reply so gutter could learn the real capability was
  rejected: it brings back the protocol parsing this design exists to remove, and it
  would still have to guess how long to wait before concluding "no reply".
- A passed-through reply that arrives while resize mode is active is swallowed by the
  in-mode default. Very rare and self-limiting — the child falls back — and recorded
  here so it is a known edge rather than a mystery.

## Code anchors

- `src/relay.rs` — `KeyModeRelay`: the allowlist matcher (`observe`), the canonical
  bytes, the log, and `reset_bytes()` / `replay_bytes()`
- `src/callbacks.rs` — the `unhandled_csi` branch, the buffered relay bytes
  (`drain_relay`) and the local device-query replies
- `src/render.rs` — the drain in `dispatch`'s `Msg::Pty` arm; `relay_reset` in
  `ordered_restore`, shared by `run_teardown` and `park`; `relay_replay` in `unpark`
- `src/terminal.rs` — `OuterTerminal::relay` and the mock's `Call::Relay`

Supersedes [ADR-003](0003-two-independent-kitty-states.md). The input path this
depends on is [ADR-020](0020-raw-input-passthrough.md); the restore slot it fills is
[ADR-010](0010-ordered-teardown.md) and, on the suspend cycle,
[ADR-019](0019-suspend-resume-cycle-ordering.md). The mouse modes it must never relay
are [ADR-005](0005-mouse-eager-capture-poll-gate.md)'s.
