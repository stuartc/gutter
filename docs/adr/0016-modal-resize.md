# ADR-016: Modal resize

Status: Accepted

## Context

gutter exists to give a comfortable reading width, and that width was fixed at
launch. PRD 0001 (Feature 2) asks for a way to change it live, without teaching the
child anything about it. Until then gutter forwarded every keystroke to the child; a
live resize means reserving at least one.

## Decision

Reserve exactly one chord, `--resize-key` (default Ctrl-\), to toggle a modal resize
state. The render thread classifies each unit the input scanner produces
([ADR-020](0020-raw-input-passthrough.md)) before anything is written to the PTY, so
the chord — and, while the mode is active, every key — never reaches the child. In
the mode:

- `h` / `l` step the width by one unit down / up; `-`, `+` / `=` and the Left / Right
  arrows do the same.
- `H` / `L` step by ten.
- `Esc`, or the chord again, leaves the mode.
- Any other key is swallowed — consumed, not forwarded — so a mistimed exit can never
  leak a stray character into the child's prompt.

Supporting choices:

- **Byte-form chord matching.** The input path is raw bytes, so the chord is matched
  on the bytes a terminal sends for it. `Ctrl-\` (also spelt `Ctrl-4`) is the byte
  `0x1C` on a legacy terminal. Once the child has asked the terminal for a keyboard
  protocol ([ADR-021](0021-child-driven-mode-relay.md)), the same physical key arrives
  as `ESC[92;5u` under kitty or `ESC[27;5;92~` under modifyOtherKeys, so the chord
  matches all three. False positives are not a concern: a bare `0x1C` cannot sit
  inside a terminal-generated escape sequence, because C0 controls abort or execute
  mid-sequence. The in-mode keys get the same treatment: a key report in either
  protocol is reduced to the byte the key would have sent before it is matched, and
  `Esc` matches its disambiguated forms too. Widening only the exit would let `Esc`
  leave the mode on those terminals while every step key was silently swallowed.
- **Presses only.** gutter never asks for key-release reports itself; they arrive only
  when the child asked for them through the relay. When they do, the chord's release
  must not count as a second press, or the mode would toggle straight back out on
  key-up. kitty carries the event type as a sub-parameter of the *modifier* field, not
  the key field, so the release is `ESC[92;5:3u`, and the matcher accepts press forms
  only. Step keys act on repeats (hold `h` to keep shrinking): an auto-repeating
  legacy key sends its byte again, and a kitty repeat report (`:2`) reduces to the
  same byte as its press, so it steps too.
- **A consumed press owes the child nothing.** gutter remembers the key of the last
  report press it consumed, so the child never gets the rest of a key it never saw.
  That key's release (`:3`) is swallowed and settles it. Its repeats are swallowed
  out of the mode and leave it owed; in the mode they reach the classifier and step
  (or are swallowed) the same way. Any other key clears it. So a key held across the
  mode's edge stays with gutter until key-up: a step key released after the idle
  exit, an `Esc` held to leave, and a chord held from outside the mode, whose
  repeats land in the mode as unrecognised keys and are swallowed.
- **Unit-preserving steps.** `step_width` nudges a `Width` in its own unit — columns
  for `Cols`, percent for `Percent` — and never converts between them. A `Cols` step
  starts from the effective width and is clamped to `[MIN_W, real_cols]`, but the
  floor never exceeds the current width, so a band already narrower than `MIN_W`
  (legal via `--width 10`) stays put on a shrink press rather than snapping up. A
  `Percent` step is clamped to 1–100, and `resolve_width` applies the column floor and
  cap ([ADR-011](0011-width-resolution.md)).

A step (`apply_resize_step`) follows `handle_resize`'s
[ADR-008](0008-resize-ordering.md) order — resize the PTY first, then
`set_size(rows, cols)` — but holds `real_cols` fixed and steps `width_config` instead
of reading a new terminal size. At a bound, where the width does not change, it skips
the PTY and parser resize and only refreshes the overlay.

The mode leaves itself after `RESIZE_IDLE` (3 s) with no resize key. The deadline
belongs to the render loop, not the `Renderer`: `ResizeCtl` holds it as an `Option`
of the injected `Clock`'s instant ([ADR-007](0007-coalescing-loop.md)), so idle
behaviour is testable on a virtual clock with no real sleeps. Phase A's wait is
bounded by whichever deadline comes first, the idle exit or the ESC-hold, so a silent
child still wakes the loop; with neither pending it blocks on a plain `recv()` and
costs no CPU while idle. A check at the top of each frame catches a child whose output
never lets Phase A block.

A swallowed key does not reset the idle deadline. Idle means "no resize key", so a
stray keypress cannot hold the mode open.

## Consequences

- Outside resize mode the chord is the only keystroke gutter withholds from the child.
- `apply_message` handles every message in the live loop (Phase A and Phase B): input
  goes through the scanner and the classifier, everything else to `dispatch`. The
  shutdown drain and the suspend drain call `dispatch` directly, since resize keys
  there are irrelevant.
- What the mode paints — rails, width readout, gutter clear — sits behind
  `enter_resize_overlay`, `refresh_resize_overlay` and `clear_resize_overlay`, and is
  [ADR-017](0017-uniform-margin-management.md)'s.
- A width-only change leaves `base_row` alone, so the inline anchor
  ([ADR-013](0013-inline-anchor-scroll-paint.md)) stays where it was, as it does on a
  width-only terminal resize.
- Inside a bracketed paste the chord is not matched
  ([ADR-022](0022-absorbed-mode-mirroring.md)). Pasted text is data, so a `0x1C` in it
  is forwarded like any other byte, and a paste containing it cannot drop the user
  into resize mode and have the rest of the paste eaten as commands. The paste guards
  are only honoured once gutter has mirrored `?2004h` outward, so a program that never
  asked for paste protection cannot disable the chord by sending the guards itself.
- The arrow aliases match both cursor-key encodings, `ESC [ D` and `ESC O D`. With
  DECCKM mirrored (ADR-022) the terminal really does send the SS3 form.

## Code anchors

- `src/chord.rs` — `Chord`, `parse_chord`, `Chord::matches`, `csi_key`
- `src/render.rs` — `classify_unit`, `walk_tokens`, `apply_key_action`,
  `apply_resize_step`, `ResizeCtl`, `RESIZE_IDLE`, `apply_message`, and the idle
  checks in `run`
- `src/geometry.rs` — `step_width`
- `src/cli.rs` — `--resize-key`
