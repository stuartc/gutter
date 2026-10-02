#!/usr/bin/env bash
# Stress the PTY integration tests: run the test binaries over and over, several at
# once, with CPU burners alongside, and count which tests fail.
#
# usage: stress.sh [-n iterations] [-t test-threads] [-b burners] [-j binaries-at-once]
#                  [-f test-name-filter] [binary ...]
#   binary   names under tests/ without .rs (default: all of them)
#   -j       default: every selected binary at once. -j 1 runs them one after another.
#   env      REPO (the checkout, default: the one this script is in), OUT (log dir,
#            default: a fresh temp dir), CARGO_TARGET_DIR (passed to cargo)
#
# No per-binary timeout: a hung test hangs the run (macOS has no timeout(1)).
set -u
n=10 t=8 b=0 j=0 f=
while getopts n:t:b:j:f: o; do
  case $o in
    n) n=$OPTARG ;; t) t=$OPTARG ;; b) b=$OPTARG ;; j) j=$OPTARG ;; f) f=$OPTARG ;;
    *) sed -n '2,12p' "$0"; exit 2 ;;
  esac
done
shift $((OPTIND - 1))

cd "${REPO:-$(dirname "$0")/..}" || exit 2
OUT=${OUT:-$(mktemp -d "${TMPDIR:-/tmp}/stress.XXXXXX")}
export TERM=xterm-256color T=$t F=$f

# "name path" per line, from cargo's own list of what it built.
bins=$(cargo test --no-run 2>&1 | sed -n 's|^ *Executable tests/\(.*\)\.rs (\(.*\))$|\1 \2|p')
[ $# -gt 0 ] && bins=$(echo "$bins" | grep -E "^($(IFS='|'; echo "$*")) ")
[ -n "$bins" ] || { echo "no test binaries matched" >&2; exit 2; }
[ "$j" -gt 0 ] || j=$(echo "$bins" | wc -l | tr -d ' ')

burners=
cleanup() { [ -n "$burners" ] && kill $burners 2>/dev/null; pkill -P $$ 2>/dev/null; }
trap cleanup EXIT
trap 'exit 130' INT TERM HUP
for _ in $(seq 1 "$b"); do yes >/dev/null & burners="$burners $!"; done

start=$(date +%s)
for i in $(seq 1 "$n"); do
  mkdir -p "$OUT/$i"
  echo "$bins" | D="$OUT/$i" xargs -P "$j" -n 2 sh -c \
    '"$2" ${F:+"$F"} --test-threads "$T" >"$D/$1.log" 2>&1; echo "exit $?" >>"$D/$1.log"' sh
done
wall=$(($(date +%s) - start))

echo "== $n iterations, --test-threads $t, $b burners, $j binaries at once, $(echo "$bins" | wc -l | tr -d ' ') binaries${f:+, filter $f}"
echo "== failures per test (count  binary::test):"
grep -rE '^test .* \.\.\. FAILED$' "$OUT" |
  sed -E 's|.*/([^/]+)\.log:test (.*) \.\.\. FAILED$|\1::\2|' | sort | uniq -c | sort -rn
echo "== binaries that ended without a test result (crash or abort):"
grep -rL '^test result:' "$OUT" | sed -E 's|.*/([^/]+)\.log$|\1|' | sort | uniq -c
echo "== failing binary runs: $(grep -rlE '^exit [^0]' "$OUT" | wc -l | tr -d ' ') of $((n * $(echo "$bins" | wc -l)))"
echo "== wall time: ${wall}s   logs: $OUT"
