# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.1] - 2026-08-11

### Fixed

- Stray characters and dropped letters no longer appear in the band while output is streaming quickly
- The right edge no longer corrupts when the band fills the whole terminal width
- Your terminal's margins no longer pick up a background colour from the band as it scrolls
- A row that scrolls and loses content no longer leaves stale text standing in the band

## [0.3.0] - 2026-08-04

### Added

- Keys gutter used to swallow now reach the child: Delete, Home, End, PageUp, PageDown, Insert, Shift+Tab, F1–F12, `Alt+<key>` and modified arrows
- The child negotiates the kitty and modifyOtherKeys keyboard protocols with your terminal directly, so a program that wants key-event reporting gets it
- Application cursor keys, application keypad and bracketed paste follow the child's mode, and a paste reaches it whole — including while resize mode is up
- Whatever the child asked your terminal for is put back when it exits, and re-asserted across Ctrl-Z and `fg`

### Fixed

- Gutter paints on your terminal when its output is redirected — `gutter cmd > log` now leaves the log empty instead of the screen blank
- The terminal, its size and its line settings all come from one device, so a session whose stdin and stdout are different terminals no longer paints with the wrong geometry
- Teardown attempts every restore step, so a failure part-way can no longer hand your shell back in raw mode
- A clean exit resets colours and cursor shape, so your shell no longer comes back tinted or wearing the child's cursor
- Every resize-mode key works on a terminal using the kitty keyboard protocol, not just Escape
- A mouse report in a non-SGR encoding is dropped rather than panicking the render thread

## [0.2.2] - 2026-07-23

### Changed

- Ignore dev files

### Fixed

- Clear band interior on resize to stop stale-cell corruption

## [0.2.1] - 2026-07-08

### Changed

- Point README install section at v0.2.0
- Link install section at /releases/latest
- Merge pull request #7 from stuartc/fix/resize-cursor-artifacts
- Release gutter version 0.2.1

### Removed

- Remove flickering block artifacts at resize band edges

## [0.2.0] - 2026-07-08

### Added

- Modal resize-mode state machine
- Address review nits from stream-B follow-up
- Uniform margin management + resize rails
- Default to a 100-column band, add --width full alias
- Stop observation + Flow/Msg plumbing
- Suspend/resume cycle
- Resume polish
- Address review findings (resume repaint, abort drain, best-effort park)

### Changed

- Bump actions/checkout to v5
- Drop workstream narration from overlay-stub comments
- Dedup resize-step overlay refresh, drop stream narration
- Renumber this branch's ADRs to 0016/0017
- Drop redundant comment and duplicate blank-assert helper
- Merge pull request #4 from stuartc/feat/default-width
- Merge remote-tracking branch 'origin/main' into feat/modal-resize
- Merge pull request #5 from stuartc/feat/modal-resize
- ADRs + cross-links
- Simplify suspend cycle after review
- Merge pull request #6 from stuartc/fix/ctrl-z-suspend
- Release gutter version 0.2.0

### Fixed

- Blank vacated rail/readout cells when the band grows in mode
- Fix stale full-width-default docs and comments after review
- Suspend PTY integration

## [0.1.0] - 2026-06-30

### Added

- Add .gitignore and .tool-versions
- Skeleton-passthrough-teardown
- Virtual-grid-offset-repaint
- Wide-char-edge-of-band
- Keyboard-reencode-kitty
- Resize
- Osc52-clipboard-write
- Sgr-mouse
- Hardening-equivalence-gate
- Dim "Exited with: N" status line on non-zero exit
- Mirror child screen mode onto the outer terminal (E2 core)
- Emit scrolled-off primary lines into the real terminal's scrollback
- Add ubuntu CI job running the five cargo gates
- Add CLAUDE.md for Claude Code
- Add README and GPL-3.0 licence
- Spawn the input reader after the kitty probe
- Clip row runs to the band rectangle so reverse-video can't bleed
- Anchor the band at the launch row and grow it inline
- Answer the child's device queries from the render thread

### Changed

- Bootstrap gutter cargo project with pinned deps
- Simplify and re-scope after skeleton-passthrough-teardown
- Simplify and re-scope after virtual-grid-offset-repaint
- Simplify and re-scope after wide-char-edge-of-band
- Simplify and re-scope after keyboard-reencode-kitty
- Simplify and re-scope after resize
- Simplify and re-scope after osc52-clipboard-write
- Simplify and re-scope after sgr-mouse
- Simplify and re-scope after hardening-equivalence-gate
- Keep render_cell_walk dormant; state the A1 rows_diff-only assumption
- Ignore vim swap, undo and session files
- Update outer-screen invariant for base_row/ever_painted_inline model
- Serialise the test harness to stop PTY integration flakes
- Merge branch 'fix/vim-test-swapfiles'
- Tighten code comments, add ADRs and comment standard
- Set up changelog, versioning and binary releases
- V0.1.0

### Fixed

- Spawn child in launcher cwd, confirm env inheritance (E1)
- Grow the equivalence corpus to four fixtures (A2)
- Reconcile inline base_row with scroll-emit, attribute rows, and teardown gate
- Serialize PTY integration tests to fix parallel-load flakiness
- Drain the PTY path before teardown reads the alt-screen state
- Stop vim integration test leaking swap files
- Fix loose ADR cites and width-default wording in comments

[0.3.1]: https://github.com/stuartc/gutter/compare/v0.3.0..v0.3.1
[0.3.0]: https://github.com/stuartc/gutter/compare/v0.2.2..v0.3.0
[0.2.2]: https://github.com/stuartc/gutter/compare/v0.2.1..v0.2.2
[0.2.1]: https://github.com/stuartc/gutter/compare/v0.2.0..v0.2.1
[0.2.0]: https://github.com/stuartc/gutter/compare/v0.1.0..v0.2.0
[0.1.0]: https://github.com/stuartc/gutter/tree/v0.1.0

<!-- generated by git-cliff -->
