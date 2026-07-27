# ADR-018: Stop-aware waiter (`waitpid` + `WUNTRACED`/`WCONTINUED`)

Status: Accepted

## Context

Ctrl-Z inside a child (e.g. `nvim :suspend`) stops the child, but gutter's waiter
thread (Thread 4) blocked on `portable_pty::Child::wait()`, which wraps a plain
`waitpid` with no `WUNTRACED`. A stopped child is invisible to it: `wait()` blocks
straight through the stop, gutter never learns the child stopped, and it keeps
forwarding keystrokes into a process that can't read them. `fg` does nothing —
there is no job-control shell holding *gutter's* job, only the child's — and only
killing the terminal recovers.

**Why gutter can never observe the stop any other way.** When the user presses
Ctrl-Z, gutter's own process never receives SIGTSTP, for two independent reasons:

1. **gutter's side (the real terminal).** gutter runs `enable_raw_mode()` on the
   outer tty, which clears `ISIG`. The real terminal's line discipline never turns
   `0x1A` into a signal — Ctrl-Z arrives at gutter as an ordinary `0x1A` byte and
   is forwarded into the PTY master like any other byte. gutter's own process
   group gets nothing.
2. **the child's side (the PTY).** portable-pty's unix spawn does `setsid()` +
   `TIOCSCTTY` in `pre_exec`, so the child is a session leader in its own session,
   the foreground process group of the PTY slave (pid == pgid == sid). SIGTSTP is
   generated *inside that session only*: a cooked-mode child (a shell) has its PTY's
   line discipline turn `0x1A` into SIGTSTP for its own foreground group; a
   raw-mode child (nvim, less) reads the raw byte and self-raises (`nvim :suspend`
   does `kill(0, SIGTSTP)` on its own group). Either way the signal is scoped to the
   child's session on the child's controlling terminal — it cannot cross into
   gutter's session.

A signal handler on gutter can therefore never fire from this repro path. The only
observation point left is asking the kernel directly: `waitpid(child_pid, …,
WUNTRACED)`.

## Decision

Rewrite the waiter (`src/waiter.rs`) to reap via raw `libc::waitpid(pid, &mut
status, WUNTRACED | WCONTINUED)` in a loop, bypassing `portable_pty::Child::wait()`
entirely on unix:

- `WIFSTOPPED(status)` → send `Msg::ChildStopped { sig: WSTOPSIG(status) }` and loop
  back; `waitpid` then blocks until the next state change. `sig` rides along for
  tests/logging only — v1 reacts to every stop signal identically (SIGTSTP,
  SIGSTOP, SIGTTIN, SIGTTOU all drive the same suspend cycle, ADR-019).
- `WIFCONTINUED(status)` → send `Msg::ChildContinued` and loop back.
- `WIFEXITED`/`WIFSIGNALED` → send `Msg::ChildExited` and return, ending the
  thread. A signalled death maps to exit code `128 + WTERMSIG(status)`, the
  conventional signal-death mapping (this is a behaviour change from the previous
  plain `child.wait()`, which only ever reported `WIFEXITED`).
- `waitpid` returning `-1` with `EINTR` retries; any other error synthesises
  `Msg::ChildExited(1)` so the render loop still tears down rather than hangs.

**Reap-ownership rule.** Once raw `waitpid` owns reaping a pid, nothing else may
wait on it: no `child.wait()`, no `try_wait()`, no "kill then wait" teardown
helper. One reaper, forever. The `portable_pty` `Child` handle is kept alive in
scope (so the PTY master fd stays open) but its `wait()` is never called;
portable-pty's `Drop` swallows the resulting `ECHILD` at teardown.

**`#[cfg(unix)]` split.** The raw-`waitpid` loop is unix-only. Off-unix, `run` keeps
today's behaviour verbatim: one blocking `child.wait()`, no stop observation, no
suspend support. On unix, if `child.process_id()` returns `None` (no pid to target),
the waiter also falls back to the legacy single-`wait()` path — no suspend support,
same as before.

**`WCONTINUED` is a non-load-bearing repaint hint.** `Msg::ChildContinued`'s only
effect is `renderer.reset_prev_baseline()` (force a full repaint next frame). The
primary suspend/resume path (ADR-019) does not depend on it — resume is
straight-line code after gutter's own `suspender.suspend_self()` call returns, not
triggered by observing the child. `ChildContinued` exists to repaint correctly if
some *other* process continues the child out from under gutter, and it also fires
harmlessly on gutter's own `continue_child` SIGCONT (one redundant baseline reset
per resume). **As built, `WIFCONTINUED` does not fire at all on macOS** — Darwin's
`waitpid` does not wake on SIGCONT the way Linux's does — so this hint is inert on
macOS and only earns its keep on Linux. The unit test
(`wait_loop_reports_stop_continue_exit`) tolerates its absence for exactly this
reason.

**Orphaned-group note.** The child's process group is orphaned (its parent, gutter,
is in a different session). POSIX permits discarding *terminal-generated*
job-control stops for an orphaned group, but a `kill(2)`-raised SIGTSTP (nvim's
route) stops regardless, and the issue's repro proves the stop happens in practice.
If a stop is ever discarded, the child simply keeps running — no bug, no code
needed.

## Consequences

- The waiter thread never terminates on a stop or a continue — only on real child
  death — so `Msg::ChildExited` remains the sole shutdown trigger (unchanged from
  before this ADR).
- Signal-death exit codes changed from "unreported" to `128 + sig`, visible to
  anything reading gutter's own exit code.
- `child_pid` must be captured (`child.process_id()`) *before* `child` moves into
  the waiter thread, since the raw `waitpid` loop needs the pid directly rather than
  going through the `Child` trait object.
- The suspend/resume cycle (ADR-019) that reacts to `Msg::ChildStopped` depends
  entirely on this ADR's detection mechanism; get the reap-ownership rule wrong
  (e.g. adding a second waiter) and the two reapers race or fail with `ECHILD`.

## Code anchors

- `src/waiter.rs` — `wait_loop`, the reap-ownership comment, the `#[cfg(unix)]` /
  `#[cfg(not(unix))]` split, `legacy_wait`
- `src/msg.rs` — `Msg::ChildStopped`, `Msg::ChildContinued`
- `src/main.rs` — capturing `child_pid` before the waiter spawn

The suspend/resume cycle this detection feeds is
[ADR-019](0019-suspend-resume-cycle-ordering.md).
