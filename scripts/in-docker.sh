#!/usr/bin/env bash
# Run a command in a CPU-limited Linux container against the repo (mounted read-only).
# Everything the container writes goes to target/docker/, mounted at /work, so the
# host's own build is untouched.
#
# usage: in-docker.sh <cpus> <command...>
#   in-docker.sh 2 scripts/stress.sh -n 10 -t 14 -b 4
#   in-docker.sh 1 cargo test --locked --test '*'
#   OUT=/work/runs/NAME in-docker.sh 2 scripts/stress.sh    # keep stress.sh's logs
# The image (rust + vim + procps) is built on first use.
set -eu
REPO=$(cd "$(dirname "$0")/.." && pwd)
IMAGE=${IMAGE:-gutter-test:1.98.1}
[ $# -ge 2 ] || { sed -n '2,10p' "$0"; exit 2; }
cpus=$1; shift
docker image inspect "$IMAGE" >/dev/null 2>&1 || docker build -t "$IMAGE" - <<'DOCKERFILE'
FROM rust:1.98.1
RUN apt-get update && apt-get install -y --no-install-recommends vim procps && rm -rf /var/lib/apt/lists/*
DOCKERFILE
mkdir -p "$REPO/target/docker"
exec docker run --rm --cpus "$cpus" \
  -v "$REPO":/repo:ro -v "$REPO/target/docker":/work \
  -e CARGO_TARGET_DIR=/work/target -e CARGO_HOME=/work/cargo \
  -e TERM=xterm-256color ${OUT:+-e OUT="$OUT"} ${RUST_TEST_THREADS:+-e RUST_TEST_THREADS="$RUST_TEST_THREADS"} \
  -w /repo "$IMAGE" "$@"
