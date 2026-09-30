# ADR-022: Absorbed-mode mirroring by poll-diff

Status: Accepted

## Context

gutter is a terminal emulator sitting between the child and the real terminal, so
every mode the child switches on lands in gutter's vt100 parser and stops there. For
anything that only affects the child's grid that is exactly right. For a handful of
modes it is wrong, because they change how the *real terminal* encodes the keys it
sends: the child asks for a behaviour, gutter absorbs the request, and the terminal
carries on with the old encoding.

Three of them matter in practice.

- **Application cursor keys (DECCKM, `CSI ? 1 h`).** Most full-screen programs set it
  at startup, and the terminal then reports arrows as `ESC O A` rather than `ESC [ A`.
  The two are interchangeable for readline-style consumers, which is why this rarely
  shows up as a hard breakage — but a program binding only the SS3 form sees its arrow
  keys do nothing.
- **Application keypad (DECKPAM, `ESC =`).** The keypad keys send `ESC O p` … `ESC O y`
  instead of plain digits, which is how an editor tells the keypad from the number row.
- **Bracketed paste (`CSI ? 2004 h`).** The terminal wraps pasted text in
  `ESC [ 200 ~` … `ESC [ 201 ~` so a program can tell a paste from typing. Without it a
  pasted multi-line prompt submits on its first newline — and, worse, the paste arrives
  as ordinary keystrokes, so a pasted `0x1C` fires gutter's own reserved chord and the
  rest of the paste is eaten as resize commands.

ADR-021 relays the keyboard-mode sequences vt100 does *not* implement, through
`unhandled_csi`. These three it does implement, so there is no callback to hook and
nothing for the relay to see. That is the same wall ADR-005 hit for the mouse.

## Decision

Mirror exactly these three onto the real terminal by **per-frame poll-diff**: each
frame, read the child's live screen, compare it with a record of what was last
mirrored, and emit bytes only when a mode has changed. This is the same shape ADR-012
uses for the alt screen and the renderer uses for cursor visibility (DECTCEM).

| Mode | On | Off |
|---|---|---|
| Application cursor (DECCKM) | `ESC [ ? 1 h` | `ESC [ ? 1 l` |
| Application keypad | `ESC =` | `ESC >` |
| Bracketed paste | `ESC [ ? 2004 h` | `ESC [ ? 2004 l` |

The byte forms are vt100's own, so anything gutter emits its parser would accept back.
DECKPAM is a plain escape, `ESC =`, not a DECSET — easy to get wrong.

**Not `Screen::input_mode_diff`.** vt100 ships what looks like exactly this helper, and
it is a trap: its body emits these three *and then* the mouse protocol mode and
encoding. Mirroring a mouse mode is the one thing this must never do, so the diff is
hand-rolled and covers three modes, permanently.

**Not diffed against `prev`.** The renderer already carries a `prev` parser as the
paint baseline, and it is tempting to diff modes against it. But `sync_prev` replays
`contents_formatted`, which deliberately leaves out the input modes, so `prev`'s flags
sit at their defaults for ever and a diff against it would re-emit the mirror bytes
every frame, at 60fps, invisibly. The mirrored state is instead three plain booleans in
`ModeMirror`, held on the renderer beside `cursor_visible` and `outer_alt_active`.

**The bytes go out through `OuterTerminal::relay`**, ADR-021's method. Its name says
relay, but what it does — write these opaque bytes to the outer terminal, they are not
a row paint — is what the mirror needs too, and reusing it keeps one `Call::Relay` in
the mock for the teardown and suspend ordering assertions to read.

**Placed before the paint loop.** ADR-014 requires each painted row to be a
self-contained run inside its margin-offset rectangle; mode bytes between a `move_to`
and its row bytes would break that for nothing. Sitting next to the alt-screen mirror
is for readability, not correctness — all three modes are terminal-global and are not
saved or restored across `?1049`.

**Bracketed paste also gates the scanner.** With the mode mirrored the terminal really
does send the guards, so the scanner's paste state becomes reachable, and inside it the
rule is absolute: every byte between the guards is forwarded verbatim. Not matched
against the chord, not extracted as a mouse report, not dropped for being malformed —
a pasted `ESC [ < 0 ; 10 ; 5 M` must reach the child as those bytes, not as a
margin-translated report. The guards themselves are forwarded too. A real terminal
never puts genuine mouse reports inside its own paste guards, so suspending extraction
wholesale costs nothing.

The gate reads the **mirror's** flag, not the child's live mode. The mirror flag
answers "could the terminal have produced these guard bytes?", which is what the
scanner actually needs to know; gating on the child's mode would let a literal
`ESC [ 200 ~` in ordinary input open a paste in the window between the child setting
the mode and gutter mirroring it.

**Restore to the terminal's defaults, and only what was set.** Teardown and park emit
an off form for each mode currently mirrored on, immediately before ADR-021's relay
reset — all the input-encoding restores together, coarsest last. Reading the
terminal's *initial* state would need a `DECRQM` round trip on the fd Thread 3 owns for
the whole run, the same wall ADR-019 hit, so gutter restores to off. The accepted
downside: a launching shell that had its own paste protection on has it cleared. Both
`zsh`'s ZLE and `bash`'s readline re-assert their input modes at every prompt, so the
damage lasts one prompt.

**Resume needs no replay list.** `park` clears the mirror after emitting the offs; the
child's modes are still on its screen, so the resume repaint's poll finds them
disagreeing with a cleared mirror and re-emits them. The poll-diff *is* the replay.
This is where it beats `CursorShape::rearm()`, which exists only because the DECSCUSR
watcher is event-driven and has no live state to re-read.

### What is never mirrored

**Mouse modes. Not now, not later, not behind a flag.** Mouse reports carry
coordinates, and a coordinate from the real terminal is in physical columns while the
child's grid is in band columns — translating that is the whole point of ADR-005.
Mirroring `?1000h` / `?1002h` / `?1003h` / `?1006h` outward would not change what the
terminal sends, since gutter's eager capture already enabled the set it needs, but it
would set up a second, uncoordinated authority over the terminal's mouse state, and a
child that *disabled* reporting would turn gutter's own capture off underneath it.

**Anything with a display side effect.** The rule: mirror modes that change how the
terminal encodes input, and nothing else. If a mode can move the cursor, change what is
on screen, alter wrapping, or change what the terminal reports about its own geometry,
it is not mirrored, however convenient that would be.

### The DECSETs considered and rejected

There is **no relay path for private modes** — no allowlist, no route from an
unhandled DECSET to the outer terminal. These reach `unhandled_csi` and each is a
standing decision not to act:

| Parameter | Why not |
|---|---|
| `?1047`, `?1048` | The alt screen and the saved cursor. Both belong to ADR-012's mirror and gutter's own paint; a second authority would fight it. |
| `?2048` | In-band resize reports. Would tell the child the *real* terminal's size — the exact lie the project exists to prevent. |
| `?2026` | Synchronised output. A paint-timing mode for whoever owns the paint, which is gutter. |
| `?7` | Autowrap. A display mode, and one vt100 does not implement for its own grid either. gutter sets the host's autowrap itself (ADR-014), independently of the child. |
| `?12` | Cursor blink. Display. |
| `?66` | DECNKM, the DECSET spelling of application keypad. Mirroring it as well as `ESC =` would give one piece of state two mechanisms that could disagree; every relevant child uses `ESC =`. |
| `?1004` | Focus reporting. Genuinely safe — input-only, no coordinates, no display effect — but default-deny means an entry earns its place by evidence a real child needs it, and the honest case was "nothing hard breaks without it". Adding it later is one more mirrored mode. |
| `?1036`, `?1039` | Meta-sends-escape variants. No observed child needs them, and the byte forms they change are exactly what passthrough already carries verbatim. |
| `?9001` | Win32 input mode. Windows-only; gutter is unix-only. |
| `?1015` | The urxvt mouse encoding — deliberately *not* enabled, per ADR-005. |

A future addition would have to deal with one vt100 quirk: `decset` and `decrst` call
the unhandled closure **once per unrecognised parameter**, while the closure always
hands `unhandled_csi` the **complete** parameter list. So `CSI ? 7727 ; 1004 h` fires
twice with identical arguments, and any relay would have to rebuild the list without
the recognised parameters and drop the duplicate call. This is a good part of why a
poll-diff on parsed state is the better mechanism where one exists.

## Consequences

- A child that sets DECCKM gets the arrow encoding it asked for, from the terminal it
  asked, with gutter understanding nothing about either. The in-mode resize decoder
  therefore has to accept `ESC O C` / `ESC O D` as well as the CSI forms (ADR-016),
  because mirroring DECCKM is what makes the terminal send the SS3 form.
- A pasted `0x1C` no longer drops the user into resize mode mid-paste.
- RIS needs no special case: `ESC c` replaces vt100's screen wholesale, so the next
  poll sees all three modes off and emits the off forms. A resize needs none either —
  `set_size` touches the grid, not the mode flags.
- The mirror lags the child by up to one frame (~16 ms, ADR-007), and the lag is in the
  safe direction: the terminal cannot start producing a new encoding before it has been
  told to, so gutter never receives bytes in an encoding it is not ready for.
- A truncated paste leaves interpretation suspended — the chord and mouse extraction
  stop working until the child leaves paste mode. Nothing is buffered, no input is lost,
  and it corrects itself when the mirror next turns paste mode off. Accepted rather than
  defended against.
- A child that disables paste mode mid-paste ends the suppression immediately, so a
  `0x1C` in the tail can still enter resize mode. That is the old behaviour narrowed to
  a tail, and the alternative — waiting for a closing guard the terminal may already
  have decided not to send — risks the stuck state above for nothing.

## Code anchors

- `src/modes.rs` — `ModeMirror`: the three booleans, the hand-rolled edge diff
  (`take_pending`), `reset_bytes()` and `clear()`
- `src/render.rs` — the mirror block in `render_once`; `mode_reset` in
  `ordered_restore`, shared by `run_teardown` and `park`; the `clear()` in `park`; and
  the scanner's gate in `apply_message`
- `src/scan.rs` — `Scanner::set_paste_guards` and the gated paste state
- `src/terminal.rs` — `OuterTerminal::relay`, shared with ADR-021

The mouse modes this must never touch are
[ADR-005](0005-mouse-eager-capture-poll-gate.md)'s; the edge-triggered mirror it copies
is [ADR-012](0012-screen-mode-mirroring.md). The relay it sits beside — and borrows its
outward write from — is [ADR-021](0021-child-driven-mode-relay.md), on top of
[ADR-020](0020-raw-input-passthrough.md)'s passthrough. The restore slot it joins is
[ADR-010](0010-ordered-teardown.md) and, on the suspend cycle,
[ADR-019](0019-suspend-resume-cycle-ordering.md).
