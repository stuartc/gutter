# ADR-023: One terminal, never stdout

Status: Accepted

## Context

gutter had three open file descriptors and two different answers to "which terminal
am I talking to". It painted the band on **stdout**, but read the keyboard from
`/dev/tty`, took the terminal's size from `/dev/tty` (crossterm's `terminal::size`
opens it itself), and opened a second `/dev/tty` for the OSC-52 clipboard sink
(ADR-004). The startup CPR probe behind the inline anchor (ADR-013) straddled the
split outright: it wrote its question, `ESC [ 6 n`, to stdout, and read the answer
back off `/dev/tty`.

`/dev/tty` is the kernel's per-process name for "the terminal this process is
attached to" — its *controlling* terminal. It is not a fixed device: the same path
opens a different device in a process attached to a different terminal, and it fails
to open at all in a process attached to none. stdout is unrelated to any of that; it
is wherever the invoking shell happened to point descriptor 1.

Three consequences, all measured rather than theorised:

- `gutter cmd > log` left the screen completely blank and put the entire band —
  escape sequences, repaints and all — into the file. It also stalled about 104 ms at
  startup, because the probe's question went into the file, so no terminal ever saw
  it and no reply could come; the anchor fell back only after `CPR_TIMEOUT` (100 ms)
  expired.
- With stdin on a 24×80 terminal and stdout on a 50×120 one, gutter painted into the
  120-column terminal using the 80-column terminal's geometry. Two answers, both
  believed at once.
- Nothing anywhere in the binary ever checked that a handle was a terminal at all —
  `isatty`/`IsTerminal` appeared nowhere. There was no point at which gutter could
  say "there is no screen here" and stop.

## Decision

**One terminal.** Every side gutter has on the outer terminal goes through one device
— `/dev/tty`, the controlling terminal, or, when the process has none, the terminal
`ttyname(STDIN_FILENO)` names (*the terminal stdin names* below) — and stdout is not
touched, bar one thing, the termios state, which crossterm still owns and which *The
one thing still not on the controlling terminal* below records in full.

| Side | Handle |
|---|---|
| The band's paint sink | The resolved terminal, opened write-only, wrapped in a `BufWriter`. This open is what resolves it |
| The keyboard, and the CPR probe's reply | The same device, opened read-write, **once**, shared by the probe and Thread 3 |
| The OSC-52 clipboard | The same device, its own read-write open (ADR-004) |
| The terminal's size | `/dev/tty`, opened by crossterm inside `terminal::size`, falling through to stdout where there is none |
| Raw mode | **stdin** whenever stdin is a terminal — crossterm's `tty_fd()`, the exception |

gutter never writes stdout, and reads stdin in no code of its own — with no controlling
terminal it asks descriptor 0 for its name and nothing more, and crossterm's raw-mode
helpers are the only other thing that touches it. stderr carries gutter's own
diagnostics — the startup refusals, and a failed clipboard write — never the child's
output, which goes to its PTY and reaches the screen only as band paint.

**The open is the guard.** A terminal that opens is a terminal gutter can paint on, so
there is no separate `isatty` check to write and no way for the check and the handle to
disagree — the thing gutter tests is the very thing it then uses. Both opens run at the
top of `run()`, before the child's PTY is spawned and before anything is probed. Either
failure prints one line,
`gutter: no controlling terminal: <reason>`, and returns 1, matching the existing
startup-error shape in `main.rs`. Placing them first is also what removes the stall:
the probe can no longer be waiting for an answer that was never going to arrive,
because a run with nothing to answer it does not get that far.

**One file description on the read side.** The startup probe and Thread 3 share a
single open, which the probe reads to completion and then hands over, leftover
keystrokes included. Two separate opens would be two independent readers on one
terminal, and a terminal gives each waiting byte to exactly one reader: whichever read
ran first would take the CPR reply, and the other would sit waiting for bytes already
gone. Extra *write* handles to the same device raise no such question and are expected:
the render sink and the clipboard sink are two of them, deliberately (ADR-004).

**The sink is buffered, so the flush is correctness, not tidiness.** `io::Stdout`
flushed itself as the process ended; a `BufWriter<File>` does not, and `process::exit`
runs no destructors (ADR-010). The ordered restore's last byte-producing step,
`show_cursor`, flushes, and teardown is best-effort per step so it is reached whatever
else failed; `park` flushes before the self-stop (ADR-019).

### The precedent: tmux and screen

Neither looks at stdout at all. Both work the terminal out from **stdin, by name** —
`ttyname(STDIN_FILENO)` — and render to that. If stdin is not a terminal, both refuse
straight away, one line and a non-zero exit: tmux's is
`open terminal failed: not a terminal`. A redirected stdout is simply ignored; `tmux > log`
paints on the screen and leaves the log empty. gutter now behaves the same way, and
deliberately does **not** tee the child's output into the redirect target — an empty
log is the chosen behaviour, not a gap.

### The terminal stdin names

`/dev/tty` is asked first, and answers for nearly every run. It fails for a process
that has no controlling terminal — `setsid gutter bash`, a `subprocess.Popen` handed a
PTY without `start_new_session=True` from a caller that has no terminal of its own, a
Go `exec` that attaches a PTY but omits `Setsid`+`Setctty`. In every one of those there
is a usable screen on descriptors 0/1/2, so gutter asks `ttyname(STDIN_FILENO)` for its
path and **reopens it by name** — the tmux and screen mechanism, reached as a fallback
rather than as the primary route.

Reopening by name is load-bearing, not incidental. `dup(0)` would hand back the
caller's own file description, shared offset and flags and all; a fresh `open` is an
independent one, which is what ADR-004's separate-open rule and the probe/Thread-3
sharing rule both assume. Every open carries `O_NOCTTY`, which means nothing on
`/dev/tty` and everything on the fallback route: gutter is a session leader without a
controlling terminal exactly when that route is taken, and on Linux a plain `open` of a
free terminal would silently adopt it. What widens is how gutter *finds* a screen, not
what session it is in.

Order matters more than mechanism here, and `/dev/tty` stays first for two reasons. It
is the process's own answer, so it keeps working when stdin is redirected — tmux refuses
`tmux < input.txt`, gutter runs. And when only one of the two resolves, `/dev/tty` is
the one that cannot be pointed somewhere unrelated by a caller's plumbing.

tmux's own reason for avoiding `/dev/tty` does not apply to gutter. tmux is a client and
a long-lived server: the client works out the terminal's *path* and hands that name to
the server, which opens it. `/dev/tty` is meaningless as a name to pass around, because
it resolves per process — in the server it would mean the server's terminal, or nothing.
gutter is a single process wrapping a single child, so the descriptor it opens is the
descriptor it uses, and no name ever leaves the process.

One difference from tmux remains. When gutter *does* have a controlling terminal and is
handed a **different** terminal on its stdio, `/dev/tty` answers first and gutter paints
there — a caller that allocates a PTY without `TIOCSCTTY` from inside a terminal session
gets nothing on the PTY it is reading.
Closing that means preferring stdin over `/dev/tty`, which costs the redirected-stdin
case above; it is left as tmux's behaviour, not gutter's.

## Consequences

- `gutter cmd > log` paints on screen and leaves the log empty, with no startup stall.
- With no terminal on either route — a cron job, a pipeline with no terminal anywhere —
  gutter prints one line and exits 1 **before** the child is spawned. There is no
  half-started run to clean up and no child left behind.
- `setsid gutter bash`, and any launcher that hands over a PTY without `TIOCSCTTY` from
  outside a terminal session, starts and paints on that PTY. The old `dup(0)` fallback
  in `open_input_tty` is still gone; what replaces it is a whole terminal, resolved by
  name and used for the sink, the keyboard and the clipboard alike, so the two-handle
  disagreement this record exists to remove cannot come back through it.
- The degraded "keyboard input is disabled" branch is deleted with it. A run without a
  keyboard is no longer reachable: the same device must open for the paint sink first.
- The clipboard's separateness (ADR-004) survives unchanged, but its rule is now
  stated correctly: a distinct **open**, not a fd that differs from stdout.
- Both guard failures print the same line, so the message does not say which open
  failed. The second can realistically only fail if the controlling terminal is
  revoked in the microseconds between the two calls; distinguishing them is not worth
  the prose.
- The size still comes from crossterm, which opens `/dev/tty` itself and falls back to
  stdout, then to `tput`, then to gutter's own `80×24` default. On the `/dev/tty` route
  the stdout fallback is unreachable, the guard having already proved `/dev/tty` opens.
  On the fallback route it is what answers, and it lands on the right terminal because
  the shape that gets there is a terminal on 0/1/2. A run that reaches the fallback with
  stdout redirected elsewhere gets `tput`, or the `80×24` default: the band paints
  correctly and is sized for a terminal that may not be this one.

### The one thing still not on the controlling terminal

Raw mode. crossterm's `enable_raw_mode`/`disable_raw_mode` go through its `tty_fd()`,
which uses **stdin** whenever stdin is a terminal and only opens `/dev/tty` otherwise.
So with stdin on one terminal and `/dev/tty` on another, the termios flags are set on
stdin while everything else in this record talks to `/dev/tty` — the same shape of bug,
in the one corner crossterm still owns.

On the fallback route the exception disappears of its own accord: the device gutter
resolved is the terminal on stdin, so crossterm sets the flags on the same one.

It is left open deliberately. Closing it means dropping crossterm's raw-mode helpers
for a direct `tcgetattr`/`tcsetattr` on gutter's own `/dev/tty` descriptor, and
carrying the saved termios through the ordered teardown (ADR-010) and the park/unpark
cycle (ADR-019) — which reshapes `OuterTerminal` and its mock. That is its own slice.
The exposure is narrow: it needs two genuinely different terminals, and the ordinary
redirect cases that used to trigger the stdout half of this bug no longer reach it.

## Code anchors

- `src/terminal.rs` — `open_tty_write`, the two routes, the startup guard and the path
  it hands back; `stdin_tty_path`; the `BufWriter<File>` sink and the `writer()`
  accessor the probe borrows
- `src/anchor.rs` — `open_input_tty`, the single read-side open; `probe_cursor_row`,
  which writes its query through the band's own sink
- `src/clipboard.rs` — `open_tty_read_write`, the read-write open both the input
  handle and the clipboard sink are made from
- `src/main.rs` — both opens at the top of `run()`, the resolved path threaded to the
  keyboard and clipboard opens, and the refusal lines
- `tests/tty_model.rs` — the refusal, the fallback, the redirect, and the
  probe-survives-a-redirect tests

The clipboard's separate open is [ADR-004](0004-osc52-clipboard-separate-tty.md); the
CPR probe whose question this moves is
[ADR-013](0013-inline-anchor-scroll-paint.md). The single-reader rule on the input
handle is [ADR-020](0020-raw-input-passthrough.md)'s, on
[ADR-009](0009-merged-unbounded-channel.md)'s exclusivity argument. The flush the
buffered sink now depends on belongs to [ADR-010](0010-ordered-teardown.md) and, on
the suspend cycle, [ADR-019](0019-suspend-resume-cycle-ordering.md).
