#!/usr/bin/env python3
"""Capture tests/fixtures/synthetic-modes.rec for the replay check.

Runs the debug build of gutter on a real PTY with GUTTER_RECORD set, around
tests/fixtures/synthetic-modes.sh, and plays the terminal and the person at it:
answers the startup cursor query so the band starts below some shell history,
resizes the window, presses the resize-mode keys, and releases each of the
child's stages with Enter after more than a second of quiet (the replay check
snapshots wherever a recording is quiet for a second).

Usage, from the repo root:

    cargo build && scripts/capture-rec.py [out.rec]

Timestamps differ on every capture, so a recapture replaces the snapshots that
go with the recording. Stdlib only.
"""
import fcntl
import os
import pty
import select
import struct
import sys
import termios
import time

OUT = sys.argv[1] if len(sys.argv) > 1 else "tests/fixtures/synthetic-modes.rec"
CMD = ["target/debug/gutter", "--width", "80%", "sh", "tests/fixtures/synthetic-modes.sh"]
BAND_ROW = 7  # 1-based row the fake terminal reports: six history rows above the band
QUIET = 1.5  # comfortably over the replay check's one second


def set_size(fd, cols, rows):
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))


pid, master = pty.fork()
if pid == 0:
    set_size(0, 100, 30)
    env = {"TERM": "xterm-256color", "PATH": os.environ["PATH"], "GUTTER_RECORD": OUT}
    os.execvpe(CMD[0], CMD, env)


def drain(seconds):
    """Read what gutter paints for `seconds`, answering its cursor query."""
    end = time.monotonic() + seconds
    while (left := end - time.monotonic()) > 0:
        if not select.select([master], [], [], left)[0]:
            return
        try:
            data = os.read(master, 65536)
        except OSError:  # gutter has exited
            return
        if not data:
            return
        if b"\x1b[6n" in data:
            os.write(master, b"\x1b[%d;1R" % BAND_ROW)


def key(data, then=QUIET):
    os.write(master, data)
    drain(then)


drain(QUIET)  # stage 1 prints
set_size(master, 84, 30)  # narrower, with history still above the band
drain(QUIET)
key(b"\r")  # a few more lines
key(b"\r")  # the synchronized repaint
key(b"\x1c", 0.3)  # Ctrl-\ enters resize mode
key(b"H", 0.3)  # -10
key(b"l", 0.3)  # +1
key(b"l")  # +1, then quiet with the rails up (the mode leaves by itself after 3 s)
key(b"\x1b")  # Escape leaves resize mode
key(b"\r")  # the alternate screen
set_size(master, 110, 34)
drain(QUIET)
key(b"\r")  # back to the primary screen
key(b"\r", 5)  # the child exits
os.waitpid(pid, 0)

rec = open(OUT).read().splitlines()
kinds = [line.split(" ", 2)[1] + " " + line.split(" ", 2)[2][:8] for line in rec[1:] if " " in line]
assert rec[1] == "0 start 100 30 %d 80%% center" % (BAND_ROW - 1), rec[1]
for want in ["mode on", "step -10", "step 1", "mode off"]:
    assert any(k.startswith(want) for k in kinds), "no %r in the recording" % want
assert sum(k.startswith("resize") for k in kinds) == 2, "expected two resizes"
print("wrote %s: %d events" % (OUT, len(rec) - 1))
