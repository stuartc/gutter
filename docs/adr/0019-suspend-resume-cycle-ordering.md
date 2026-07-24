# ADR-019: Suspend/resume cycle ordering

Status: Accepted

## Context

Once the waiter can observe a stopped child (ADR-018), gutter needs to actually do
something about it: hand the real terminal back to the launching shell in a sane
state, get out of the child's way, and put everything back correctly when the shell
`fg`s it. This has to happen without breaking the thread model — Thread 2 is the
sole owner of the parser, the outer terminal, and the PTY-master writer, and that
exclusivity (ADR-009) must survive suspend/resume intact.

## Decision

**Straight-line on Thread 2, no SIGCONT handler.** `Msg::ChildStopped` maps to
`Flow::Suspend`, which drives `suspend_cycle` — a single function run entirely on
the render thread. It parks the outer terminal, calls `suspender.suspend_self()`
(`kill(0, SIGTSTP)`), and **the whole process stops at that call site** — all four
threads freeze together, mid-stack, with no cleanup required because nothing more
runs until the shell's `fg` sends SIGCONT. When SIGCONT arrives, `suspend_self`
simply returns and execution continues on the very next statement: same thread,
same stack, still the exclusive terminal owner. No signal handler, no self-pipe, no
extra thread is needed for resume — a handler-based design would add
async-signal-safety and terminal-ownership questions (only Thread 2 may touch the
terminal) for zero benefit here.

**Self-stop and continue-child syscalls.** `kill(0, SIGTSTP)` stops gutter's own
process group — the vim-conventional self-suspend, and what a job-control shell's
job table expects; it needs no installed signal handler for the core path.
`kill(-child_pgid, SIGCONT)` continues the *child's* entire group (pgid == pid,
since portable-pty `setsid()`s the child), so a child that itself forked a group
resumes wholesale. `raise(SIGTSTP)` was rejected: it only signals the calling
thread's process, not the group a shell's job control watches.

**Restore before self-stop.** The shell that gets the terminal back after `[1]+
Stopped` must inherit a sane state: cooked mode, cursor shown and default-shaped,
mouse off, the child's keyboard modes undone, alt screen left (or an inline
hand-back newline emitted). `park()` runs this in ADR-010's order —
leave-alt-or-hand-back, reset attributes, default cursor shape, reset the mirrored
input modes, reset the relayed keyboard modes, disable mouse, show cursor, disable
raw mode **last** — then
flushes, so every byte lands before `suspend_self()` freezes the
process. This is the entire point of the fix: today's bug is exactly *not* doing
this.

**Raw mode dropped last, re-taken first.** `unpark()` mirrors `park()` in reverse,
with raw mode re-enabled *first* — before mouse or alt screen — to shrink the
cooked-mode window during which the already-running input thread (Thread 3, which
owns the tty read fd across the whole suspend) could see canonical rather than raw
input.

**No CPR at resume.** It would need a round-trip read of the tty, which Thread 3
already owns and never releases during suspend (it freezes with the process but
never joins or restarts), so the reply would be eaten silently. There is no
keyboard capability to re-probe either — gutter asks the outer terminal for no
keyboard mode of its own (ADR-020). What `unpark()` does re-assert is what the
*child* asked for, replayed from the relay's log (ADR-021), oldest first and
before mouse: a set mutates whichever level was on top when it was issued, so the
order is load-bearing. `park` deliberately leaves that log alone — it is a park,
not a teardown, and the same double-meaning trap as `outer_alt_active` applies.
The three modes ADR-022 mirrors are the opposite case and need no replay at all:
`park` clears the mirror after emitting their off forms, and the step-9 repaint's
per-frame poll finds the child's live modes disagreeing with it and re-asserts them.
The poll-diff is the replay mechanism, which is why there is no `rearm` counterpart
here for it. That lands after `continue_child`, so the child could in principle paint
in the gap — it cannot matter, since the mode affects only what the terminal sends and
the user is not typing during those microseconds.
Beyond raw mode, the replay, mouse and the alt screen, nothing else is re-taken.
The inline anchor is not re-queried at all: **`base_row` is reseeded to the bottom
(`rows.saturating_sub(1)`)** for primary-screen (non-alt) children, on the
assumption that the shell scrolled the screen while gutter slept, making the old
`base_row` meaningless. `render_once`'s make-room scroll then re-lays the band at
the new bottom, pushing the shell's suspend-era output into scrollback above the
pre-suspend band copy — the same duplication a plain terminal shows after `fg`.
Accepted, not fixed.

**TIOCSWINSZ before child SIGCONT.** The outer terminal may have been resized while
gutter was stopped; a pending SIGWINCH can coalesce a resize-and-back into a stale
or missing event, so `unpark` queries `term.terminal_size()` explicitly
and, if it differs from the parser's current size, runs the full ADR-008 resize
handler. Because this runs *before* `continue_child()`, the `TIOCSWINSZ` queues one
SIGWINCH on the still-stopped child; the child wakes to a single pending resize and
repaints once at the right size instead of repainting once at the stale size and
again a moment later.

**`continue_child` last.** `suspender.continue_child()` — `kill(-child_pgid,
SIGCONT)`, `ESRCH` ignored (the child may have died while gutter was stopped) —
only runs after the outer terminal is fully raw, keyboard-modes-replayed,
mouse-enabled and alt-screen-correct. The child's post-continue repaint bytes must
never race an un-raw or wrong-mode terminal.

**`outer_alt_active`'s double meaning.** During park, the flag is *not* cleared
after `leave_alt_screen()` — it deliberately keeps meaning "the child's screen is
alt" through the frozen interval, so the abort path (see Edge cases) can't
misinterpret it. At resume, `unpark()` re-derives it fresh from
`renderer.parser.screen().alternate_screen()`. Anything that naively clears it at
park time instead of re-deriving it at resume will emit a spurious `leave_alt` on
the next teardown.

**Silent suspend.** No "Suspended (fg to resume)" banner. This matches plain
job-control behaviour — the launching shell already prints `[1]+ Stopped`, and a
gutter-printed banner would just be noise gutter has no business adding to the
child's terminal.

**`bg`/SIGTTOU.** If the shell backgrounds gutter instead of foregrounding it,
`unpark`'s `enable_raw_mode()` (a `tcsetattr` from a background process group)
raises SIGTTOU under default disposition — gutter re-stops before touching the
terminal, and the child was never continued. A later `fg` restarts the call in the
foreground and the cycle completes normally. This falls out of the ordering for
free rather than being handled explicitly; expected, and on the manual checklist to
confirm against both `zsh` and `bash`.

## Sequence (as built)

`suspend_cycle`, run entirely on Thread 2:

0. **Resize-mode teardown**, if active — same as `run`'s shutdown tail.
1. **Pre-stop drain.** A bounded quiet-gap drain (`SUSPEND_QUIET_GAP` = 20ms,
   capped at `SUSPEND_DRAIN_CAP` = 100ms) for the child's pre-stop terminal-restore
   bytes, which have no ordering guarantee against `Msg::ChildStopped` arriving
   first. There is no `Msg::PtyEof` here — the PTY is not at EOF, the child is
   merely stopped — so this cannot reuse `drain_pty_path`. If a `Flow::Exit`
   surfaces during the drain (the child was SIGKILLed immediately after stopping),
   return `SuspendOutcome::ChildExited` immediately without parking.
2. **`render_once`** — flush the drained state; the child's alt→primary edge (if
   any) fires here, keeping `outer_alt_active` truthful for park.
3. **Park** (`park()`).
4. **`suspender.suspend_self()`** — the process stops here until `fg`.
5. **Unpark** (`unpark()`) — raw mode first, then the mode replay, mouse and alt,
   cursor shape `rearm()`ed for the next repaint (park reset the outer cursor to
   default, staling the cursor-shape watcher's dedup state).
6. **Inline anchor reseed** — `base_row` reset to the bottom for primary-screen
   children, seeded from the post-resize row count read via `term.terminal_size()`.
7. **Missed-resize catch-up** — `term.terminal_size()` vs. the parser's current
   size; if different, run `handle_resize` (ADR-008), reusing the step-6 size query.
8. **`suspender.continue_child()`**.
9. **Repaint** — `reset_prev_baseline()`, `render_once`, then
   `repaint_margins(.., false)` to clear any shell text the suspension left in the
   gutter columns.

**Deviations from the original plan, as built:** the inline anchor reseed (step 6
above) runs *before* the missed-resize catch-up (step 7), sourcing its row count from
`term.terminal_size()` directly rather than from the parser's post-resize size. This
ordering is load-bearing: `handle_resize` now physically clears the band interior
(ADR-017), and on the primary screen the pre-resize `base_row` still points into the
shell output the child left on screen — clearing from a stale mid-screen anchor would
wipe that history. Reseeding to the bottom row first means the catch-up's interior
clear only touches the band's own rows. `OuterTerminal` gained a `terminal_size()`
seam specifically for these two steps (query the real outer size without going through
`Event::Resize`); the single query is reused by both. There is no
explicit `teardown_done` flag: the abort path (step 1) returns
`SuspendOutcome::ChildExited` *before* park ever runs, so it falls straight through
to `run`'s own single call to `run_teardown` — there is only ever one call site, so
no guard against a double call was needed.

## Consequences

- All of steps 3–9 run on Thread 2 with no locks and no new shared state; ADR-009's
  exclusivity is untouched.
- A `Suspender` trait (`src/suspend.rs`) makes the two syscalls injectable —
  `RealSuspender` in production, a recording `MockSuspender` in tests that shares an
  ordered call log with `MockTerminal` so park/self-stop/unpark/continue-child
  interleaving is asserted as one sequence. The filter that log is read through has
  to admit `Call::Relay`, or the two mode steps are invisible to the assertion.
- `Flow::Suspend` is ignored inside `drain_pty_path` (the shutdown drain never
  recurses into a fresh suspend).
- On resume, `continue 'frames` in `run`'s loop ensures the next coalescing frame
  captures a fresh deadline — no stale pre-stop deadline survives an arbitrarily
  long suspension.
- Direct `SIGTSTP` sent straight to gutter (not via a child's Ctrl-Z) is
  unaffected by this ADR — gutter has no handler, so it stops in raw mode,
  corrupting the outer terminal exactly as before. Deferred hardening (handling
  `SIGTSTP` on gutter's own signal thread) is out of scope for this fix.
  `signal-hook` is no longer unused — ADR-020 gives it `SIGWINCH` — so the
  hardening would extend that thread rather than add one.

## Code anchors

- `src/render.rs` — `suspend_cycle`, `park`, `unpark`, `retry_enable_raw`,
  `SuspendOutcome`, `Flow::Suspend`
- `src/relay.rs` — the mode log park resets and unpark replays
- `src/suspend.rs` — the `Suspender` trait, `RealSuspender`, `MockSuspender`
- `src/cursor.rs` — the cursor-shape watcher's `rearm()`
- `src/terminal.rs` — `OuterTerminal::terminal_size()`

The stop-detection mechanism this cycle reacts to is
[ADR-018](0018-stop-aware-waiter.md). The park/unpark ordering reuses
[ADR-010](0010-ordered-teardown.md)'s teardown order, reversibly. The resize
ordering the missed-resize catch-up reuses is
[ADR-008](0008-resize-ordering.md). The inline anchor concept it reseeds is
[ADR-013](0013-inline-anchor-scroll-paint.md).
