# ADR-013: Inline anchor, scroll-aware paint, and hand-back

Status: Accepted

## Context

On the primary screen, a plain command should grow downward from where gutter was
launched, not overwrite the scrollback above it, and lines that scroll off the top
should land in the terminal's own scrollback rather than a buffer of our own. When
gutter exits, the shell prompt should resume below the command's output.

## Decision

Anchor the band at `base_row`, the launch cursor row captured by a CPR query before
the input threads start. As the band grows past the bottom of the screen, scroll the
real terminal up to make room and let `base_row` fall monotonically toward 0. Once it
reaches 0 the band fills the screen, and lines that depart the top are emitted to the
terminal's scrollback. A second vt100 parser with bounded scrollback — the scroll
tracker — records which lines left the top of the `W`-window each frame, since the
live parser (scrollback 0) can't reconstruct that once a burst scrolls past a
screenful in one frame. On exit, a child still in the alt screen takes the normal
alt-leave; a child on the primary screen that ever painted inline gets a hand-back —
the cursor drops to a fresh line below the band, with a dim status line on a non-zero
exit. A TUI that went straight to alt and never painted inline leaves nothing behind.

## Consequences

- `base_row` starts at the launch row and only ever decreases (via make-room scroll).
- The scroll tracker keeps its screen for the whole session; a frame's departures are
  the growth of its scrollback since the last frame. The live parser stays at
  scrollback 0.
- The hand-back fires only when `ever_painted_inline` is set, so a straight-to-alt
  TUI leaves no stray status line.
- The status line prints only on a non-zero exit.

## Amendment — the CPR query travels over the terminal that answers it

The probe used to write `ESC [ 6 n` to stdout and read the `ESC [ row ; col R` answer
back off `/dev/tty`: a question posed on one handle and an answer expected on another.
That worked only because the two usually happened to be the same terminal. With stdout
redirected the question went into the file, nothing could ever answer, and the anchor
fell back to `rows - 1` a full `CPR_TIMEOUT` later — 100 ms of dead startup on every
run of `gutter cmd > log`.

Since [ADR-023](0023-controlling-terminal-fd-model.md) the query goes out through the
band's own sink, so query and reply are the same terminal by construction. The "no tty
at all" arm of the capture is gone with it: startup has already refused a run with no
terminal to paint on, so the only fallback left is the one that always mattered — the
terminal did not answer in time, and the anchor takes `rows - 1`. Never row 0, which is
the overpaint this record exists to prevent.

## Amendment — the tracker keeps its screen, and counts by growth

The tracker used to be re-seeded from the live grid's `contents_formatted()` at the end
of every frame, so that its scrollback was empty when the next frame began and its
length was that frame's departure count. The replay carries the grid's cells and
nothing else: not the child's scroll region, and not the alternate-screen flag. vt100
pushes a departed row into scrollback only while no scroll region is active, so from the
frame after the child set one, a tracker that had forgotten it reported a phantom
departure for every scroll inside the region — and one phantom departure is enough to
collapse `base_row` and pin the band to the top of the screen for the rest of the
session. The alt-screen half was the same fault masked: the render loop threw the
departed lines away whenever the outer screen was in alt, which hid a tracker that was
on the primary grid while the child was on the alternate one.

vt100 0.16.2 cannot clear a scrollback, read its length, or report a scroll region, so
the fix is to stop re-seeding. The tracker now lives for the session: it is the child's
own screen state, region and alt flag included, and each frame's departures are the
lines beyond the length the last frame counted to. Nothing is drained while the tracker
is in the alternate screen — vt100 gives the alt grid no scrollback, so no line ever
departs one, and the primary count waits untouched for the child to come back. The mask
in the render loop is gone with it. A baseline reset (resize, the alt→primary edge, a
resume) marks the tracker's current scrollback as counted rather than rebuilding it, so
lines that departed before the change are not re-emitted after it, and a resize resizes
the tracker's screen alongside the live parser's.

That leaves one thing the deque has to be emptied for. At its cap vt100 drops the
oldest line for each new one, the length stops growing, and a growth count reads zero
for every departure after that — the band would stop scrolling the real terminal
altogether. On a full scrollback the tracker is re-seeded once, which does forget the
scroll region until the child sets one again. The cap is 4096 departed lines, so it is
paid once in a long-scrolling session rather than sixty times a second, and only ever
in exchange for counting nothing at all.

## Code anchors

- `src/render.rs` — the anchor, make-room scroll, scroll emit, and hand-back; the
  scroll-tracker field
- `src/anchor.rs` — the hand-rolled probe and the input handle it reads the reply from
- `src/main.rs` — the CPR anchor capture and the `GUTTER_FORCE_ANCHOR_ROW` override

The teardown ordering is [ADR-010](0010-ordered-teardown.md); the screen-mode
mirroring is [ADR-012](0012-screen-mode-mirroring.md). The terminal the probe asks,
and the handle it asks on, are [ADR-023](0023-controlling-terminal-fd-model.md).
