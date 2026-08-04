#!/usr/bin/env bash
# Add the release section for $NEW_VERSION to the top of CHANGELOG.md and stage
# it. cargo-release runs this as its pre-release hook; run it by hand and it does
# the same thing.
#
# It PREPENDS rather than regenerating the whole file. A released section is a
# published artefact — it is what the GitHub Release for that tag says — so a
# later change to cliff.toml's parsers must not go back and reword it, and a
# section someone tidied by hand has to survive the next release.
#
# git-cliff's footer only writes the compare links when it generates the whole
# file, so prepending means adding the new one here.
set -euo pipefail

new="${NEW_VERSION:?NEW_VERSION is set by cargo-release; pass it yourself otherwise}"
cd "$(git rev-parse --show-toplevel)"

# The dry run writes the section too (that is what you review), so a real run
# straight after it would prepend a second copy.
if grep -q "^## \[${new}\]" CHANGELOG.md; then
    echo "changelog.sh: CHANGELOG.md already has a ${new} section, leaving it alone"
    git add CHANGELOG.md
    exit 0
fi

prev="${PREV_VERSION:-$(git describe --tags --abbrev=0 | sed 's/^v//')}"

git cliff --unreleased --tag "v${new}" --prepend CHANGELOG.md

repo="https://github.com/stuartc/gutter"
awk -v link="[${new}]: ${repo}/compare/v${prev}..v${new}" '
    !added && /^\[[0-9]+\.[0-9]+\.[0-9]+\]: / { print link; added = 1 }
    { print }
' CHANGELOG.md >CHANGELOG.md.new && mv CHANGELOG.md.new CHANGELOG.md

git add CHANGELOG.md
