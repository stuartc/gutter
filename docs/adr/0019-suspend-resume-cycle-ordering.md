# ADR-019: Suspend/resume cycle ordering

Status: Accepted

## Context

Once the waiter can observe a stopped child (ADR-018), gutter has to act on it: hand
the real terminal back to the launching shell in a sane state, get out of the child's
way, and put everything back correctly when the shell `fg`s it. This has to happen
without breaking the thread model — Thread 2 is the sole owner of the parser, the
outer terminal and the PTY-master writer, and that exclusivity (ADR-009) must survive
suspend/resume intact.

## Decision

**Straight-line code on Thread 2, no SIGCONT handler.** `Msg::ChildStopped` maps to
`Flow::Suspend`, which runs `suspend_cycle` — one function, entirely on the render
thread. It parks the outer terminal and calls `suspender.suspend_self()`
(`kill(0, SIGTSTP)`), and **the whole process stops at that call** — every thread
freezes mid-stack, with nothing to clean up, because nothing runs until the shell's
`fg` sends SIGCONT. Then `suspend_self` returns and execution carries on at the next
statement: same thread, same stack, still the terminal's only owner. No signal handler,
self-pipe or extra thread is needed to resume. A handler-based design would raise
async-signal-safety questions, and questions about which thread may touch the terminal
(only Thread 2 may), for no gain.

**The two syscalls.** `kill(0, SIGTSTP)` stops gutter's own process group — the
conventional self-suspend (vim does the same), and what a job-control shell's job table
expects. `kill(-child_pgid, SIGCONT)` continues the *child's* whole group (pgid == pid,
since portable-pty `setsid()`s the child), so a child that forked its own children
resumes with them. `raise(SIGTSTP)` was rejected: it signals only gutter's own process,
not the group the shell's job control watches.

**Restore before the self-stop.** The shell that gets the terminal back after
`[1]+ Stopped` must inherit a sane state: cooked mode, cursor shown and default-shaped,
mouse off, autowrap on, the child's keyboard modes undone, and the alt screen left (or
an inline hand-back newline emitted). `park()` runs ADR-010's `ordered_restore` for
this — raw mode dropped **last** — and then flushes, so every byte lands before
`suspend_self()` freezes the process.

**Raw mode dropped last, re-taken first.** `unpark()` re-takes the terminal with raw
mode *first* — before autowrap, the mode replay, mouse or the alt screen — to shorten
the window in which the input thread (Thread 3, which keeps the tty read fd across the
whole suspend) could see canonical rather than raw input. The raw-mode re-take retries
on `EINTR` a bounded number of times (`retry_enable_raw`), since the SIGCONT that woke
gutter can interrupt the `tcsetattr`. Autowrap is turned off again straight after:
park turned it on for the shell, and nothing else would notice it stayed on for the
rest of the run (ADR-014).

Every unpark step is attempted whatever an earlier one returned, the same `BestEffort`
shape as the restore (ADR-010) and for the mirrored reason. A raw-mode re-take that
runs out of `EINTR` retries must not skip the alt re-entry: `outer_alt_active` would be
set back to `true` while the terminal sat on the primary screen park handed back, and
teardown would then emit a `?1049l` for an alt screen the terminal never entered —
restoring a buffer from before the run over what the user was looking at.

**No CPR at resume.** A cursor-position query needs to read the reply off the tty, and
Thread 3 owns that fd throughout the suspend (it freezes with the process but is never
joined or restarted), so it would eat the reply. There is no keyboard capability to
re-probe either — gutter asks the outer terminal for no keyboard mode of its own
(ADR-020).

**What unpark re-asserts.** The keyboard modes the *child* asked for, replayed from
the relay's log (ADR-021), oldest first and before mouse. A set changes whichever level
of the kitty stack was on top when it was issued, so the replay order matters. `park`
leaves that log alone — it is a park, not a teardown, and the same double-meaning trap
as `outer_alt_active` (below) applies. The three modes ADR-022 mirrors are the opposite
case and need no replay: `park` clears the mirror after emitting their off forms, and
the step-9 repaint's per-frame poll finds the child's live modes disagreeing with it
and turns them back on. That lands after `continue_child`, so the child could in
principle paint in the gap. It cannot matter: these modes change only what the
terminal sends, and the user is not typing in those microseconds. Beyond raw mode,
autowrap, the replay, mouse and the alt screen, nothing else is re-taken.

**The inline anchor is reseeded, not re-queried.** For a child on the primary screen,
`base_row` is reset to the bottom row (`rows.saturating_sub(1)`), on the assumption that
the shell scrolled the screen while gutter slept and the old `base_row` means nothing.
`render_once`'s make-room scroll then lays the band out again at the bottom, pushing the
shell's suspend-time output into scrollback above the copy of the band from before the
suspend — the same duplication a plain terminal shows after `fg`. Accepted, not fixed.

**Reseed before the missed-resize catch-up.** The outer terminal may have been resized
while gutter was stopped, and a pending SIGWINCH can merge a resize-and-back into a
stale or missing event. So `unpark` is followed by an explicit `term.terminal_size()`
query and, if it differs from the current geometry, the full ADR-008 resize handler.
The reseed has to come first: `handle_resize` clears the band's interior (ADR-017), and
on the primary screen the old `base_row` still points into the shell output the child
left on screen, so clearing from it would wipe that history. With the anchor already at
the bottom, the clear only touches the band's own rows. The reseed takes its row count
from the same `terminal_size()` query, falling back to the parser's size if the query
fails.

**TIOCSWINSZ before the child's SIGCONT.** Because the catch-up runs *before*
`continue_child()`, its `TIOCSWINSZ` queues one SIGWINCH on the still-stopped child.
The child wakes to a single pending resize and repaints once at the right size, instead
of once at the stale size and again a moment later.

**`continue_child` last.** `suspender.continue_child()` — `kill(-child_pgid, SIGCONT)`,
ignoring `ESRCH` because the child may have died while gutter was stopped — runs only
once the outer terminal is raw, has the child's keyboard modes replayed, has mouse on
and is on the right screen. The child's repaint after continuing must never race a
terminal that is still cooked or in the wrong mode.

**`outer_alt_active` has two meanings.** Park does *not* clear the flag after leaving
the alt screen: through the frozen interval it keeps meaning "the child's screen is
alt", so the abort path (step 1 below) cannot misread it. At resume, `unpark()`
re-derives it from `renderer.parser.screen().alternate_screen()`. Anything that clears
it at park time instead of re-deriving it at resume will emit a spurious alt-leave on
the next teardown.

**Silent suspend.** No "Suspended (fg to resume)" banner. This matches plain job
control — the launching shell already prints `[1]+ Stopped`, and a banner would be
noise gutter has no business adding.

**`bg` and SIGTTOU.** If the shell backgrounds gutter instead of foregrounding it,
unpark's raw-mode re-take (a `tcsetattr` from a background process group) raises
SIGTTOU under the default disposition. gutter stops again before touching the terminal,
and the child was never continued. A later `fg` restarts the call in the foreground and
the cycle completes normally. This comes free from the ordering rather than from any
code; it should be confirmed by hand against both `zsh` and `bash`.

## Sequence

`suspend_cycle`, run entirely on Thread 2:

0. **Resize-mode teardown**, if the overlay is active — `leave_resize_mode`, the same
   as `run`'s shutdown path.
1. **Pre-stop drain.** A bounded drain that stops at a quiet gap
   (`SUSPEND_QUIET_GAP` = 20 ms) or a cap (`SUSPEND_DRAIN_CAP` = 100 ms), for the
   child's terminal-restore bytes from before it stopped, which may arrive after
   `Msg::ChildStopped`. It cannot reuse `drain_pty_path`, which waits for
   `Msg::PtyEof` — the PTY is not at EOF, the child is only stopped. If a `Flow::Exit`
   comes through during the drain (the child was killed right after stopping), return
   `SuspendOutcome::ChildExited` straight away, without parking.
2. **`render_once`** — paint the drained state. The child's alt→primary edge, if any,
   fires here, keeping `outer_alt_active` accurate for park.
3. **Park** (`park()`).
4. **`suspender.suspend_self()`** — the process stops here until `fg`.
5. **Unpark** (`unpark()`) — raw mode first, then autowrap off, the mode replay, mouse
   and alt; the cursor-shape watcher is `rearm()`ed so the next repaint re-sends the
   child's shape (park reset the outer cursor to the default, so the watcher's record
   of what it last sent is stale).
6. **Inline anchor reseed** — for a primary-screen child, `base_row` goes to the bottom
   row reported by `term.terminal_size()`.
7. **Missed-resize catch-up** — if that same size differs from the current geometry,
   run `handle_resize` (ADR-008).
8. **`suspender.continue_child()`**.
9. **Repaint** — `reset_prev_baseline()`, `render_once`, then `clear_gutter` to blank
   any shell text the suspend left in the gutter columns, then an explicit flush, since
   the loop next blocks on the channel and a quiet child would otherwise leave those
   bytes unsent. It is `clear_gutter` rather than `repaint_margins(.., false)` because
   the latter also blanks the width readout's in-band fallback span, which after this
   `render_once` would erase band cells with nothing to repaint them (on
   `--width full` the readout falls inside the band).

There is no `teardown_done` flag. The abort in step 1 returns before park ever runs,
so it falls through to `run`'s single call to `run_teardown`; with only one call site,
there is no double teardown to guard against.

## Consequences

- Steps 3–9 run on Thread 2 with no locks and no new shared state; ADR-009's
  exclusivity is untouched.
- A `Suspender` trait (`src/suspend.rs`) makes the two syscalls injectable:
  `RealSuspender` in production, and a recording `MockSuspender` in tests that shares
  an ordered call log with `MockTerminal`, so the order of park, self-stop, unpark and
  continue-child is asserted as one sequence. The filter that log is read through has
  to let `Call::Relay` through, or the two keyboard-mode steps are invisible to the
  assertion.
- `drain_pty_path` ignores `Flow::Suspend`, so the shutdown drain never starts a fresh
  suspend.
- On resume, `run`'s loop does `continue 'frames`, so the next frame takes a fresh
  deadline — no deadline from before the stop survives an arbitrarily long suspend.
- A `SIGTSTP` sent straight to gutter (not via a child's Ctrl-Z) is outside this
  cycle: gutter has no handler, so it stops in raw mode and leaves the outer terminal
  unusable. Handling it would mean adding `SIGTSTP` to the signal thread that already
  turns `SIGWINCH` into `Msg::Resize` (Thread 5), rather than adding a thread. Not done.

## Code anchors

- `src/render.rs` — `suspend_cycle`, `park`, `unpark`, `retry_enable_raw`,
  `SuspendOutcome`, `Flow::Suspend`
- `src/relay.rs` — the mode log park resets and unpark replays
- `src/suspend.rs` — the `Suspender` trait, `RealSuspender`, `MockSuspender`
- `src/cursor.rs` — the cursor-shape watcher's `rearm()`
- `src/terminal.rs` — `OuterTerminal::terminal_size()`

The stop detection this cycle reacts to is [ADR-018](0018-stop-aware-waiter.md). Park
reuses [ADR-010](0010-ordered-teardown.md)'s restore. The missed-resize catch-up reuses
[ADR-008](0008-resize-ordering.md)'s resize ordering. The inline anchor it reseeds is
[ADR-013](0013-inline-anchor-scroll-paint.md).
