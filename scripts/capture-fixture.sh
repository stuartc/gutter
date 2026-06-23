#!/usr/bin/env bash
# Capture a settled VT byte stream from a real target into a gutter equivalence
# fixture (slice 06, A2 — the sustainability helper).
#
# The gate fixtures under tests/fixtures/*.cast are NOT asciinema recordings:
# they are raw, timing-less, SETTLED VT byte streams recorded at a DECLARED
# width, consumed via include_bytes!. There is no recording tool in-repo; the
# existing fixtures were hand-authored. This is the tiny tee that lets a future
# fixture be captured from a real target instead of hand-curated — it is a
# helper, not a framework.
#
# It runs the target command inside a PTY of an EXACT declared size (so the
# stream is recorded at the width the gate will replay at), tees the raw master
# read-stream to the output file, and strips nothing — the captured bytes are
# exactly what the target emitted. Let the target settle, then exit it; the file
# holds the settled frame's byte stream.
#
# Usage:
#   scripts/capture-fixture.sh <cols> <rows> <out.cast> -- <command...>
#
# Example (capture a settled 80x24 frame from a command):
#   scripts/capture-fixture.sh 80 24 tests/fixtures/new.cast -- some-tui-command
#
# Notes:
# - The DECLARED width (<cols>) MUST match the W the gate test replays at, or the
#   cell-by-cell diff silently misaligns (the per-fixture width trap).
# - `script` is the portable PTY tee available on macOS and Linux; the BSD/macOS
#   and util-linux invocations differ, so both are handled below.
# - Review the captured bytes before checking in: confirm it opens as expected
#   (ESC[?1049h for an alt-screen fixture; never for a plain-command fixture) and
#   is SGR-dense, mirroring the fixture-coverage guards in the gate tests.

set -euo pipefail

if [ "$#" -lt 5 ] || [ "$4" != "--" ]; then
  echo "usage: $0 <cols> <rows> <out.cast> -- <command...>" >&2
  exit 2
fi

cols="$1"
rows="$2"
out="$3"
shift 4 # drop cols rows out --

cmd="$*"

# Run the command at the declared size and tee the PTY master stream to <out>.
# `stty` inside the captured shell fixes the size before the target reads it.
inner="stty cols ${cols} rows ${rows}; ${cmd}"

uname_s="$(uname -s)"
if [ "${uname_s}" = "Darwin" ]; then
  # BSD/macOS script: `script -q <file> <command...>`.
  script -q "${out}" /bin/sh -c "${inner}"
else
  # util-linux script: `script -q -c <command> <file>`.
  script -q -c "/bin/sh -c '${inner}'" "${out}"
fi

echo "captured ${out} at ${cols}x${rows}; review it before checking in (declared width MUST match the gate's W)." >&2
