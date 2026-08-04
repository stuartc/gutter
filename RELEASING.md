# Releasing gutter

gutter is a solo, pre-1.0 CLI. Releases are local-drive: nothing happens that you
didn't trigger. `cargo release` bumps the version, regenerates the changelog, commits,
tags, and pushes; pushing the tag fires the release CI that builds and uploads binaries.

## One-time tool installs

```bash
cargo install cargo-release
cargo install git-cliff        # or: brew install git-cliff
```

The changelog in this repo was generated with **git-cliff 2.13.1**. cargo-release reads
its config from `[package.metadata.release]` in `Cargo.toml`; git-cliff reads `cliff.toml`.

## What ends up in the changelog

Slice subjects name the slice, not the change a user sees, so the release notes are not
just the log. Three things shape them:

- **`docs`, `test`, `refactor`, `chore`, `style`, `ci`, `build` and `perf` commits are
  skipped**, along with merge commits and the release commit itself. Only `feat` and `fix`
  reach the changelog at all.
- **A `Changelog:` trailer overrides the subject** — one bullet per trailer, so a slice
  that fixed three things says three things, in the user's terms rather than the codebase's.
  `Changelog: skip` as the only trailer drops the commit. No trailer falls back to the
  subject.
- **An effort spanning many slices squash-merges** under one `feat`/`fix` subject, with the
  trailers on the squash commit. Twelve slice commits are twelve changelog lines otherwise.

Write the trailers while the work is fresh — at the end of the slice, or on the squash
commit at merge time. Nothing downstream can recover wording that was never written.

Released sections are **never regenerated**. `scripts/changelog.sh` prepends the new one,
so a later change to `cliff.toml` cannot go back and reword what a published GitHub Release
already says, and a section tidied by hand stays tidy.

## Versioning

SemVer in `0.x`: while we're pre-1.0, **breaking changes ride the minor slot**. Tags are
`v0.1.0`, `v0.2.0`, … The release level you pass maps to the bump:

- `patch` — `0.1.0` → `0.1.1` (fixes, no behaviour change)
- `minor` — `0.1.0` → `0.2.0` (features, *and* breaking changes while in 0.x)
- `major` — `0.x` → `1.0.0` (a deliberate, separate decision — not a routine release)

The first release is **`v0.1.0`**, cut from current `main` as-is.

## Release flow

1. **Dry-run first.** Dry-run is the default — leave off `--execute`:

   ```bash
   cargo release minor
   ```

   This resolves the new version, runs the pre-release hook (`scripts/changelog.sh` writes
   and stages the new `CHANGELOG.md` section), and reports the planned commit, tag, and push
   **without changing anything else**. `publish = false` means it never attempts crates.io.

2. **Review** the planned bump, the new `CHANGELOG.md` section, and the tag name. The section
   is yours to edit — the real run sees it is already there and leaves it alone, so anything
   you fix now is what ships.

3. **Execute** once you're happy:

   ```bash
   cargo release minor --execute
   ```

### What `--execute` does, in order

1. Bump the version in `Cargo.toml`.
2. Run the pre-release hook — `scripts/changelog.sh` prepends the `v${NEW_VERSION}` section
   and its compare link, then `git add`s the file. If the dry run already wrote that section
   (or you wrote it yourself), it is left as it stands.
3. Make a **single commit** containing the version bump and the changelog.
4. Tag that commit `v0.x.0`.
5. Push the branch and the tag.

The pushed tag triggers the release workflow (`.github/workflows/release.yml`), which
cross-builds the Linux x86_64 and universal macOS binaries, attaches them with SHA-256
checksums and the `LICENSE`, and creates the GitHub Release with notes from the changelog.

Releases are refused from anywhere but `main` (`allow-branch = ["main"]`).

## First release (`v0.1.0`)

`cargo release` only ever *bumps* — there is no version to bump *to* `0.1.0` from, since
`Cargo.toml` already declares `0.1.0`. So the very first tag is cut by hand; cargo-release
takes over from the second release onward.

```bash
git cliff -o CHANGELOG.md --tag v0.1.0   # turn [Unreleased] into the [0.1.0] section
git add CHANGELOG.md
git commit -m "chore(release): v0.1.0"
git tag v0.1.0
git push origin main v0.1.0              # the tag push triggers release.yml
```

Every release after this one uses the `cargo release <level> --execute` flow below.

## Verification checklist

Run these by hand before the first real tag — this feature is config + CI, so the checks
assert on observable artefacts (the generated changelog, the dry-run plan, a compiled binary):

- [ ] `cargo release patch` (dry-run, no `--execute`) reports a clean plan, runs the hook,
      and shows the regenerated changelog **without attempting a publish**.
- [ ] `NEW_VERSION=x.y.z scripts/changelog.sh` against real history produces valid Keep a
      Changelog output — slice scopes (`feat(04): …`) stripped, `Changelog:` trailers used
      where present — and is **idempotent** (re-running yields no diff).
- [ ] `cargo build --release` is green and the resulting `target/release/gutter` runs.
- [ ] `release.yml` passes `actionlint` (syntax + action-input check).

Be honest about the limit: **the first `v0.1.0` tag push is the real integration test** for
the cross-build and the Release upload. There's no way to fully prove the workflow before a
real tag exists — the checklist above gets you as far as is possible beforehand.

## Installing from source (for users)

Users with a Rust toolchain can install without a published crate (this is also in the README):

```bash
cargo install --git https://github.com/stuartc/gutter --locked
```

macOS binaries are unsigned (no Apple Developer account), so Gatekeeper warns on first run.
Clear the quarantine flag with `xattr -c ./gutter`, or right-click the binary and choose Open.
