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

   This resolves the new version, runs the pre-release hook (git-cliff regenerates and
   stages `CHANGELOG.md`), and reports the planned commit, tag, and push **without changing
   anything**. `publish = false` means it never attempts crates.io.

2. **Review** the planned bump, the regenerated `CHANGELOG.md`, and the tag name. Make sure
   the new version section reads cleanly and the merge / non-conventional commits are present.

3. **Execute** once you're happy:

   ```bash
   cargo release minor --execute
   ```

### What `--execute` does, in order

1. Bump the version in `Cargo.toml`.
2. Run the pre-release hook — git-cliff regenerates `CHANGELOG.md` (full regeneration with
   `--tag v${NEW_VERSION}`, not a prepend) and `git add`s it.
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
- [ ] `git cliff -o CHANGELOG.md` against real history produces valid Keep a Changelog output
      — slice scopes (`feat(04): …`) stripped, merge / non-conventional commits retained — and
      is **idempotent** (re-running yields no diff).
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
