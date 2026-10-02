#!/usr/bin/env bash
# Did any test stop catching a bug it used to catch?
#
#   compare.sh <before-label> <after-label> [renames-file]
#   compare.sh --self-test
#
# Reads target/breakage/results/<label>/<patch>.txt and <patch>.run2.txt as
# breakage.sh wrote them.
# A test counts as caught by a patch when it failed in every run recorded for that
# patch under that label (one run or two). For each patch, prints the tests caught
# before that are not caught after, and exits 1 if there are any.
#
# renames-file: lines of `old new`, both as `binary::test`, for tests that were renamed
# or moved to another file between the two labels. Blank lines and `#` comments are
# skipped.
#
# Things that are reported and also make the exit status 1:
#   - a patch with results before and none after
#   - a patch that did not apply or did not build after
#   - the unpatched control failing after (those tests fail without any bug, so their
#     "catch" proves nothing)
# A binary that died or hung after has no per-test lines, so every test of that binary
# is taken as caught and a warning is printed.
#
# RESULTS=/path  results directory (default: target/breakage/results)
set -u

HERE="$(cd "$(dirname "$0")" && pwd)"

if [ "${1:-}" = --self-test ]; then
  T="$(mktemp -d)"; trap 'rm -rf "$T"' EXIT
  mkdir -p "$T/b" "$T/a"
  printf 'x::one\nx::two\nx::flaky\n' > "$T/b/01-p.txt"
  printf 'x::one\nx::two\n'           > "$T/b/01-p.run2.txt"   # flaky dropped: not caught before
  printf 'x::one\ny::deux\nx::new\n'  > "$T/a/01-p.txt"        # two renamed, a new catch is fine
  printf 'x::a\n' > "$T/b/02-q.txt"; : > "$T/a/02-q.txt"       # x::a lost
  : > "$T/b/00-control.txt"; : > "$T/a/00-control.txt"
  printf 'x::two y::deux\n' > "$T/renames"
  out="$(RESULTS="$T" "$0" b a "$T/renames")"; rc=$?
  [ $rc -eq 1 ] && echo "$out" | grep -q '^02-q: x::a$' && ! echo "$out" | grep -q '01-p' \
    || { echo "self-test FAILED (rc $rc):"; echo "$out"; exit 1; }
  printf 'x::a\n' > "$T/a/02-q.txt"
  RESULTS="$T" "$0" b a "$T/renames" >/dev/null || { echo "self-test FAILED: clean case exited non-zero"; exit 1; }
  echo "self-test ok"; exit 0
fi

[ $# -ge 2 ] || { sed -n '2,24p' "$0"; exit 2; }
RESULTS="${RESULTS:-$(git -C "$HERE" rev-parse --show-toplevel)/target/breakage/results}"
BEFORE="$RESULTS/$1"; AFTER="$RESULTS/$2"; RENAMES="${3:-}"
[ -d "$BEFORE" ] || { echo "no results for $1" >&2; exit 2; }
[ -d "$AFTER" ]  || { echo "no results for $2" >&2; exit 2; }
[ -z "$RENAMES" ] || [ -f "$RENAMES" ] || { echo "no such renames file: $RENAMES" >&2; exit 2; }

# Tests that failed in every run recorded for a patch, one per line, sorted.
caught() { # <dir> <patch>
  if [ -f "$1/$2.run2.txt" ]; then
    comm -12 <(sort -u "$1/$2.txt") <(sort -u "$1/$2.run2.txt")
  else
    sort -u "$1/$2.txt"
  fi
}

# Rewrite old names to new ones.
renamed() {
  if [ -n "$RENAMES" ]; then
    awk 'NR == FNR { if ($0 !~ /^[ \t]*(#|$)/) map[$1] = $2; next }
         { print ($0 in map) ? map[$0] : $0 }' "$RENAMES" - | sort -u
  else
    cat
  fi
}

status=0
for f in "$BEFORE"/*.txt; do
  patch="$(basename "$f" .txt)"
  case "$patch" in *.run[0-9]*) continue ;; esac

  if [ ! -f "$AFTER/$patch.txt" ]; then
    echo "$patch: <no results after>"; status=1; continue
  fi
  if grep -hq '^<' "$AFTER/$patch.txt" "$AFTER/$patch.run2.txt" 2>/dev/null; then
    echo "$patch: $(grep -h '^<' "$AFTER/$patch.txt" "$AFTER/$patch.run2.txt" 2>/dev/null | sort -u | tr '\n' ' ')"
    status=1; continue
  fi

  after="$(caught "$AFTER" "$patch")"
  if [ "$patch" = 00-control ]; then
    # The control is the other way round: anything failing after is a problem.
    [ -z "$after" ] || { echo "$after" | sed "s/^/$patch: fails with no patch: /"; status=1; }
    continue
  fi

  dead="$(echo "$after" | sed -n 's/::<binary died or hung.*//p')"
  [ -z "$dead" ] || echo "warning: $patch: binary died or hung after, every test in it taken as caught: $(echo $dead)" >&2

  while IFS= read -r t; do
    [ -n "$t" ] || continue
    case "$t" in *'<'*) continue ;; esac
    echo "$after" | grep -qxF -- "$t" && continue
    [ -n "$dead" ] && echo "$dead" | grep -qxF -- "${t%%::*}" && continue
    echo "$patch: $t"; status=1
  done < <(caught "$BEFORE" "$patch" | renamed)
done

[ $status -eq 0 ] && echo "nothing lost: every test caught under '$1' is still caught under '$2'"
exit $status
