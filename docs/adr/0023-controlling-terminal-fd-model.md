# ADR-023: One terminal, and the band never paints on stdout

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

**One terminal.** Everything gutter does on the outer terminal goes through one device
— `/dev/tty`, the controlling terminal, or, when the process has none, the terminal its
own stdio names (*the terminal gutter's stdio names* below) — and stdout is never
written.

| Side | Handle |
|---|---|
| The band's paint sink | The resolved terminal, opened write-only, wrapped in a `BufWriter`. This open is what resolves it |
| The keyboard, and the CPR probe's reply | The same device, opened read-write, **once**, shared by the probe and Thread 3 |
| The OSC-52 clipboard | The same device, its own read-write open (ADR-004) |
| The terminal's size | `TIOCGWINSZ` on the sink's own descriptor (`terminal::tty_size`), at startup and on every resize |
| Raw mode | `tcgetattr`/`tcsetattr` on the sink's own descriptor |

Nothing asks crossterm which terminal it is, which is what makes the table one answer
rather than three: `terminal::size` runs its own `/dev/tty`-then-stdout-then-`tput`
resolution, and `enable_raw_mode` its own stdin-then-`/dev/tty` one. Both are replaced by
an ioctl on the descriptor gutter already holds. The size matters most — it is the one
number that positions the band and sizes the child's PTY — and the line settings live on
the *device*, not the descriptor, so setting them through the write-only sink is what the
read side sees too.

gutter never writes stdout, and reads stdin in no code of its own — with no controlling
terminal it asks its three standard descriptors for a name and nothing more. stderr
carries gutter's own diagnostics — the startup refusal, a keyboard that would not open,
and the handful of setup and clipboard failures it reports and carries on past — never
the child's output, which goes to its PTY and reaches the screen only as band paint.

**The write open is the guard.** A terminal that opens for writing is a terminal gutter
can paint on, so there is no separate `isatty` check to write and no way for the check
and the handle to disagree — the thing gutter tests is the very thing it then uses. It
runs at the top of `run()`, before the child's PTY is spawned and before anything is
probed. Failing prints one line, `gutter: no controlling terminal: <reason>`, and returns
1, matching the existing startup-error shape in `main.rs`. Placing it first is also what
removes the stall: the probe can no longer be waiting for an answer that was never going
to arrive, because a run with nothing to answer it does not get that far.

The read open is **not** the guard. It asks for more than the sink did — read as well as
write — so it can be refused on a terminal that just opened for writing, a `/dev/pts`
node owned by another user being the realistic shape. A terminal gutter can paint on is
not a run to refuse: the keyboard is dropped, `gutter: keyboard input disabled: <reason>`
goes to stderr, the CPR probe is skipped rather than left to time out on a handle that
cannot answer, and Thread 3 is never spawned. Folding the two opens into one guard
would refuse those runs and report the wrong cause while doing it.

**One file description on the read side.** The startup probe and Thread 3 share a
single open, which the probe reads to completion and then hands over, leftover
keystrokes included. Two separate opens would be two independent readers on one
terminal, and a terminal gives each waiting byte to exactly one reader: whichever read
ran first would take the CPR reply, and the other would sit waiting for bytes already
gone. Extra *write* handles to the same device raise no such question and are expected:
the render sink and the clipboard sink are two of them, deliberately (ADR-004).

**The sink is buffered, so the flush is correctness, not tidiness.** A `BufWriter<File>`
holds bytes until something asks it not to, and the restore path is the last thing that
writes. The ordered restore's last byte-producing step, `show_cursor`, flushes,
and teardown is best-effort per step so it is reached whatever else failed; `park`
flushes before the self-stop (ADR-019). The sink's own drop-flush at the end of `run()`
is a backstop that discards its result, not the mechanism (ADR-010).

### The precedent: tmux and screen

Neither looks at stdout at all. Both work the terminal out from **stdin, by name** —
`ttyname(STDIN_FILENO)` — and render to that. If stdin is not a terminal, both refuse
straight away, one line and a non-zero exit: tmux's is
`open terminal failed: not a terminal`. A redirected stdout is simply ignored; `tmux > log`
paints on the screen and leaves the log empty. gutter now behaves the same way, and
deliberately does **not** tee the child's output into the redirect target — an empty
log is the chosen behaviour, not a gap.

### The terminal gutter's stdio names

`/dev/tty` is asked first, and answers for nearly every run. It fails for a process
that has no controlling terminal — `setsid gutter bash`, a `subprocess.Popen` handed a
PTY without `start_new_session=True` from a caller that has no terminal of its own, a
Go `exec` that attaches a PTY but omits `Setsid`+`Setctty`. In every one of those there
is a usable screen on descriptors 0/1/2, so gutter asks `ttyname` for its path and
**reopens it by name** — the tmux and screen mechanism, reached as a fallback rather than
as the primary route.

All three descriptors are asked, stdin first, where tmux asks only stdin. A launcher that
attaches a PTY normally puts it on all three and stdin answers; a supervisor that pipes
gutter's input — `setsid sh -c 'true | gutter bash'` — leaves a usable screen on 1 or 2
and nothing on 0, and refusing there would be refusing a terminal gutter can see. Asking
the other two costs two `ttyname_r` calls. It depends on raw mode being taken on the
resolved device (below): crossterm's stdin-first raw mode would find the pipe, fall back
to the `/dev/tty` open that already failed, and refuse the run anyway.

Reopening by name matters. `dup(0)` would hand back the
caller's own file description, shared offset and flags and all; a fresh `open` is an
independent one, which is what ADR-004's separate-open rule and the probe/Thread-3
sharing rule both assume. Every open carries `O_NOCTTY`, which means nothing on
`/dev/tty` and everything on the fallback route: gutter is a session leader without a
controlling terminal exactly when that route is taken, and on Linux a plain `open` of a
free terminal would silently adopt it. What widens is how gutter *finds* a screen, not
what session it is in.

Order matters more than mechanism here, and `/dev/tty` stays first for two reasons. It
is the process's own answer, so it keeps working when the stdio is redirected — tmux
refuses `tmux < input.txt`, gutter runs. And when only one of the two resolves,
`/dev/tty` is the one that cannot be pointed somewhere unrelated by a caller's plumbing.

Which route failed is in the refusal. The fallback's error names the device it could not
open — `gutter: no controlling terminal: /dev/pts/7: Permission denied` — because
reporting `/dev/tty`'s reason for a failure on a different device sends the reader at the
wrong one, with nothing in the line to say that gutter did find a screen and was turned
away reopening it.

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
  outside a terminal session, starts and paints on that PTY. It does not come back
  as the old `dup(0)` fallback in `open_input_tty`; what stands in its place is a whole terminal, resolved by
  name and used for the sink, the keyboard and the clipboard alike, so the two-handle
  disagreement this record exists to remove cannot come back through it.
- The degraded "keyboard input is disabled" branch survives, on a narrower trigger: the
  read-write open being refused on a device the write-only one accepted, rather than
  `/dev/tty` being missing altogether. The band still paints; the session is watch-only.
- The clipboard's separateness (ADR-004) survives unchanged, but its rule is now
  stated correctly: a distinct **open**, not a fd that differs from stdout.
- The size is read off the sink's own descriptor, so it cannot answer for a terminal the
  band is not on, and `80×24` is reached only when that ioctl fails or reports a zero dimension. `setsid`, a
  200×50 terminal on stdin, stdout to a file, `--width 40 --center`: the band centres for
  200 columns and the child gets 50 rows. Under crossterm's resolution the same run
  centred for 80 and gave the child 24.
- The fallback route also never sees a resize. `SIGWINCH` goes to the foreground process
  group of the terminal's session, and a process that reached this route is by definition
  in neither — so Thread 5 never fires, `handle_resize` never runs, and both the band and
  the child's PTY stay at the size the launch resolved. A `--width Npct` band silently
  stops tracking the window. This is the other half of the `O_NOCTTY` trade-off: on Linux,
  dropping it would let gutter adopt a terminal no session owns and get the signal back,
  at the cost of adopting terminals it should not.

### Raw mode on the resolved device

crossterm's `enable_raw_mode`/`disable_raw_mode` go through its `tty_fd()`, which uses
**stdin** whenever stdin is a terminal and only opens `/dev/tty` otherwise. With stdin on
one terminal and `/dev/tty` on another that set the flags on stdin while everything else
here talked to `/dev/tty`, and the cost was not cosmetic: the terminal gutter paints on
and reads the keyboard from was left cooked, so keystrokes echoed over the band and only
reached the child on Enter, while a terminal gutter otherwise never touched was left raw
for the whole run. Measured on a pair of PTYs — the band on `/dev/tty`, `ECHO` still set
there and clear on the stdin terminal. It also refused outright on the fallback route
with a pipe on stdin, where its `/dev/tty` open is the one that already failed.

So gutter owns the termios itself: `tcgetattr` on the sink's descriptor, `cfmakeraw`,
`tcsetattr(TCSANOW)`. The settings as gutter found them are saved on the first enable and
never overwritten — the park/unpark cycle (ADR-019) re-enables raw mode over a state park
itself restored, and saving again there would make teardown hand the shell back what park
left rather than what the user had. `disable_raw_mode` puts the saved settings back, or
does nothing when raw mode was never taken, keeping its ADR-010 slot and its
"undo only what was set up" rule.

`TCSANOW` rather than `TCSADRAIN`: gutter's own frames are already in flight, and waiting
on them would let the mode change lag the keystroke that caused it.

## Amendment — the pre-flight stamp is stdout, before there is a terminal

`--version` prints `gutter <stamp>` on stdout and returns 0. That is the one write
to descriptor 1 in the binary, and it does not reopen anything this record decided.
It happens at the very top of `run()`: the argument list is parsed, the stamp is
printed, and the function returns — before `open_tty_write`, before the read-side
open, before the child is spawned. There is no band, no sink and no terminal
resolved at that point, so nothing here has two answers to argue over.

The carve-out is deliberate rather than tolerated. A stamp asked for by name is the
program's output, not a frame, and `gutter --version | …` has to work the way every
other tool's does; sending it to stderr instead would keep the letter of "never
stdout" and break the thing the flag is for. The rule the rest of this record turns
on is narrower and unchanged: **once gutter is running a child, the band and everything
else it does on the terminal go to the resolved device, and stdout is never
written.** `gutter cmd > log` still paints on screen and leaves the log empty.

## Code anchors

- `src/terminal.rs` — `open_tty_write`, the two routes, the startup guard and the path
  it hands back; `stdio_tty_path`; `tty_size`; the termios save/restore behind
  `enable_raw_mode`/`disable_raw_mode`; the `BufWriter<File>` sink and the `writer()`
  accessor the probe borrows
- `src/anchor.rs` — `open_input_tty`, the single read-side open; `probe_cursor_row`,
  which writes its query through the band's own sink
- `src/clipboard.rs` — `open_tty_read_write`, the read-write open both the input
  handle and the clipboard sink are made from
- `src/main.rs` — the `--version` early return ahead of them, both opens at the top of
  `run()`, the resolved path threaded to the keyboard and clipboard opens, and the
  refusal lines
- `tests/tty_model.rs` — the refusal, the fallback, the redirect, and the
  probe-survives-a-redirect tests

The clipboard's separate open is [ADR-004](0004-osc52-clipboard-separate-tty.md); the
CPR probe whose question this moves is
[ADR-013](0013-inline-anchor-scroll-paint.md). The single-reader rule on the input
handle is [ADR-020](0020-raw-input-passthrough.md)'s, on
[ADR-009](0009-merged-unbounded-channel.md)'s exclusivity argument. The flush the
buffered sink now depends on belongs to [ADR-010](0010-ordered-teardown.md) and, on
the suspend cycle, [ADR-019](0019-suspend-resume-cycle-ordering.md).
