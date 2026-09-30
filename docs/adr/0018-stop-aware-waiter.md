# ADR-018: Stop-aware waiter (`waitpid` + `WUNTRACED`/`WCONTINUED`)

Status: Accepted

## Context

Ctrl-Z inside a child (e.g. `nvim :suspend`) stops the child. gutter's waiter thread
(Thread 4) used to block on `portable_pty::Child::wait()`, which wraps a plain
`waitpid` with no `WUNTRACED`, so a stopped child was invisible to it: `wait()`
blocked straight through the stop, gutter never learned the child had stopped, and it
kept forwarding keystrokes into a process that could not read them. `fg` did nothing —
no job-control shell holds *gutter's* job, only the child's — and only killing the
terminal recovered.

**Why gutter cannot observe the stop any other way.** When the user presses Ctrl-Z,
gutter's own process never receives SIGTSTP, for two independent reasons:

1. **gutter's side (the real terminal).** Raw mode (`cfmakeraw`, ADR-023) clears
   `ISIG` on the outer terminal, so its line discipline never turns `0x1A` into a
   signal. Ctrl-Z arrives at gutter as an ordinary `0x1A` byte and is forwarded into
   the PTY master like any other. gutter's own process group gets nothing.
2. **The child's side (the PTY).** portable-pty's unix spawn does `setsid()` +
   `TIOCSCTTY` in `pre_exec`, so the child leads its own session and is the
   foreground process group of the PTY (pid == pgid == sid). SIGTSTP is generated
   *inside that session only*: a cooked-mode child (a shell) has its PTY's line
   discipline turn `0x1A` into SIGTSTP for its own foreground group; a raw-mode child
   (nvim, less) reads the byte and raises the signal itself (`nvim :suspend` does
   `kill(0, SIGTSTP)` on its own group). Either way the signal stays in the child's
   session and cannot reach gutter's.

A signal handler in gutter can therefore never fire on this path. The only place left
to observe the stop is the kernel, via `waitpid(child_pid, …, WUNTRACED)`.

## Decision

The waiter (`src/waiter.rs`) reaps with raw `libc::waitpid(pid, &mut status,
WUNTRACED | WCONTINUED)` in a loop, bypassing `portable_pty::Child::wait()` entirely
on unix:

- `WIFSTOPPED(status)` → send `Msg::ChildStopped { sig: WSTOPSIG(status) }` and loop;
  `waitpid` then blocks until the next state change. `sig` is carried for tests and
  logging only — every stop signal (SIGTSTP, SIGSTOP, SIGTTIN, SIGTTOU) drives the
  same suspend cycle (ADR-019).
- `WIFCONTINUED(status)` → send `Msg::ChildContinued` and loop.
- `WIFEXITED`/`WIFSIGNALED` → send `Msg::ChildExited` and return, ending the thread.
  A death by signal maps to exit code `128 + WTERMSIG(status)`, the usual convention.
- `waitpid` returning `-1` with `EINTR` retries; any other error sends
  `Msg::ChildExited` with exit code 1, so the render loop still tears down rather than
  hangs.

**One reaper, forever.** Once raw `waitpid` owns reaping a pid, nothing else may wait
on it: no `child.wait()`, no `try_wait()`, no "kill then wait" teardown helper. The
`portable_pty` `Child` handle is kept alive (so the PTY master stays open) but its
`wait()` is never called; portable-pty's `Drop` swallows the resulting `ECHILD` at
teardown.

**Unix only.** Off unix, `run` does one blocking `child.wait()`: no stop observation,
no suspend support. On unix, if `child.process_id()` returns `None` there is no pid to
target, and the waiter falls back to the same single wait (`legacy_wait`).

**`WCONTINUED` is only a repaint hint.** `Msg::ChildContinued`'s sole effect is
`renderer.reset_prev_baseline()`, forcing a full repaint on the next frame. The
suspend/resume cycle (ADR-019) does not depend on it: resume is straight-line code
that runs when gutter's own `suspender.suspend_self()` returns, not a reaction to the
child. `ChildContinued` exists to repaint correctly if some *other* process continues
the child behind gutter's back, and it also fires harmlessly after gutter's own
`continue_child` (one redundant baseline reset per resume). Darwin's `waitpid` does not
report `WIFCONTINUED` the way Linux's does, so on macOS the hint never fires; the unit
test `wait_loop_reports_stop_continue_exit` tolerates its absence for that reason.

**Orphaned process group.** The child's process group is orphaned (its parent, gutter,
is in a different session). POSIX lets the system discard *terminal-generated*
job-control stops for an orphaned group, but a SIGTSTP sent with `kill(2)` (nvim's
route) stops it regardless, and the stop does happen in practice. If a stop is ever
discarded, the child simply keeps running — no bug, and no code needed.

## Consequences

- The waiter thread never ends on a stop or a continue, only on real child death, so
  `Msg::ChildExited` remains the one shutdown trigger.
- A child killed by a signal now gives gutter an exit code of `128 + sig`, where the
  old `child.wait()` path only ever reported a normal exit (`WIFEXITED`).
- `child_pid` has to be captured (`child.process_id()`) *before* `child` moves into the
  waiter thread, because the loop needs the pid itself rather than the `Child` trait
  object.
- The suspend/resume cycle (ADR-019) depends entirely on this detection. Add a second
  waiter and the two reapers race or fail with `ECHILD`.

## Code anchors

- `src/waiter.rs` — `run`, `wait_loop`, `legacy_wait`, the unix / non-unix split
- `src/msg.rs` — `Msg::ChildStopped`, `Msg::ChildContinued`
- `src/main.rs` — capturing `child_pid` before the waiter is spawned

The suspend/resume cycle this detection feeds is
[ADR-019](0019-suspend-resume-cycle-ordering.md).
