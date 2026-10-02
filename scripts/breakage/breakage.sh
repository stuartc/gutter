#!/usr/bin/env bash
# Which integration tests notice when gutter is broken in a known way?
#
#   breakage.sh <git-ref> [patch ...]
#
# For each patch: make a throwaway worktree of <git-ref>, apply the patch (src/ only),
# run the integration tests, and write the failing tests, one `binary::test` per line,
# to target/breakage/results/<label>/<patch>.txt. The full cargo output goes to
# results/<label>/logs/ beside it. compare.sh reads two labels and says what was lost.
#
# With no patch arguments it runs `control` (no patch) and then every patches/*.patch,
# which takes the best part of an hour.
# With arguments it runs exactly those: a patch name with or without `.patch`, a path
# to a patch file, or the word `control`.
#
#   LABEL=after-phase3   results directory name (default: the ref's short sha)
#   RUN=2                write <patch>.run2.txt instead of <patch>.txt (a re-run)
#   TEST_TIMEOUT=900     seconds before a hung test run is killed
#   RUST_TEST_THREADS    passed through to cargo test if set
#
# A patch that breaks most of the suite makes every waiting test run out its 30 s
# deadline; with RUST_TEST_THREADS=1 that needs TEST_TIMEOUT=3600.
#
# Everything is written under target/breakage/; the main working tree and index are
# never touched. One run at a time: the worktree path is fixed so its target dir
# stays warm.
set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(git -C "$HERE" rev-parse --show-toplevel)" || exit 2
WORK="$REPO/target/breakage"
WT="$WORK/wt/run"
export CARGO_TARGET_DIR="$WORK/target"
export TERM=xterm-256color

[ $# -ge 1 ] || { sed -n '2,26p' "$0"; exit 2; }
REF="$1"; shift
SHA="$(git -C "$REPO" rev-parse --short "$REF^{commit}")" || exit 2
LABEL="${LABEL:-$SHA}"
OUT="$WORK/results/$LABEL"
SUFFIX="${RUN:+.run$RUN}"
mkdir -p "$OUT/logs" "$WORK/wt"

if [ $# -eq 0 ]; then
  set -- control "$HERE"/patches/*.patch
fi

cleanup() { git -C "$REPO" worktree remove --force "$WT" >/dev/null 2>&1; git -C "$REPO" worktree prune; }
trap cleanup EXIT

# `Running tests/foo.rs (...)` names the binary; `test name ... FAILED` names the test.
# A binary cargo reports as failed without any FAILED line died or hung.
failing_tests() {
  awk '
    /^ +Running tests\// { sub(/^ +Running tests\//, ""); sub(/\.rs .*/, ""); bin = $0; next }
    /^test .* \.\.\. FAILED$/ { print bin "::" $2; seen[bin] = 1; next }
    /^error: test failed, to rerun pass `--test / {
      b = $0; sub(/.*--test /, "", b); sub(/`.*/, "", b); died[b] = 1 }
    END { for (b in died) if (!(b in seen)) print b "::<binary died or hung, see log>" }
  ' "$1" | sort -u
}

status=0
for arg in "$@"; do
  if [ "$arg" = control ]; then
    name=00-control; patch=
  else
    patch="$arg"
    [ -f "$patch" ] || patch="$HERE/patches/${arg%.patch}.patch"
    [ -f "$patch" ] || { echo "no such patch: $arg" >&2; status=2; continue; }
    name="$(basename "$patch" .patch)"
    patch="$(cd "$(dirname "$patch")" && pwd)/$name.patch" # git apply runs inside the worktree
  fi
  result="$OUT/$name$SUFFIX.txt"
  log="$OUT/logs/$name$SUFFIX.log"
  echo "== $name ($REF @ $SHA) -> $result"

  cleanup
  git -C "$REPO" worktree add --quiet --detach "$WT" "$REF" || { status=2; continue; }

  if [ -n "$patch" ] && ! git -C "$WT" apply --include='src/*' "$patch" 2>"$log"; then
    echo "<patch did not apply>" > "$result"; cat "$log" >&2; status=1; cleanup; continue
  fi
  if ! (cd "$WT" && cargo test --no-run --test '*') >"$log" 2>&1; then
    echo "<build failed>" > "$result"; tail -20 "$log" >&2; status=1; cleanup; continue
  fi

  # perl's alarm is the watchdog; macOS has no timeout(1).
  (cd "$WT" && perl -e 'alarm shift; exec @ARGV' "${TEST_TIMEOUT:-900}" \
     cargo test --no-fail-fast --test '*') >>"$log" 2>&1
  rc=$?
  failing_tests "$log" > "$result"
  [ $rc -ge 128 ] && echo "<run killed after ${TEST_TIMEOUT:-900}s, results incomplete>" >> "$result"
  echo "   $(grep -c . "$result") failing (cargo exit $rc)"
  cleanup
done
exit $status
