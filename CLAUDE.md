# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is gutter?

**gutter** spawns a child command in a PTY and renders it inside a narrower column band (centred or left-aligned) while the child behaves as if it owned the full terminal. Keyboard, mouse, clipboard (OSC 52), and resize all pass through transparently. It is not a prefix-with-spaces hack: gutter embeds a terminal emulator — it sizes the child's PTY at `W × real_rows` (where `W` is the band width), parses the child's output into a `W`-column virtual grid (`vt100`), and repaints that grid at a left-margin column offset on the real terminal. The original target is wrapping Claude Code in a constrained width.

## Build, test, run

Toolchain is pinned to **Rust 1.98.1** (`.tool-versions`; CI and `release.yml` pin the same version).

```bash
cargo build                         # debug build → target/debug/gutter
cargo test                          # unit + integration tests (no oracle)
cargo test --features oracle        # also builds & runs the equivalence gate and the painted-band check
cargo clippy --all-targets
cargo clippy --all-targets --features oracle

# Run it: gutter [--width <N|Npct|full>] [--center|--left] [--resize-key <chord>] <cmd> [args...]
cargo run -- --width 80 --center bash      # absolute 80-col band, centred
cargo run -- --width 50pct --left vim      # proportional (recomputed on resize); 50% also works
cargo run -- bash                          # no --width → 100-column centred band (clamped to narrower terminals)
cargo run -- --width full bash             # alias for 100%: a full-width band that tracks the terminal
cargo run -- --version                     # print the build stamp on stdout and exit
```

Flags are only recognised before the command; the first token that is not a flag is the command, and everything after it belongs to the child. There is no `--help` — it would be run as the command. There is no `cargo fmt`/coverage step in CI — don't invent one.

**Single test** (`cargo test <name-substring>` filters across all targets):

```bash
cargo test passthrough_echo                        # by name, any target
cargo test --test passthrough                      # one integration file
cargo test --bin gutter                            # only the in-crate unit tests (there is no lib target)
cargo test --bin gutter --features oracle oracle::gate::equivalence_gate   # the equivalence gate
cargo test --bin gutter --features oracle oracle::band::painted_band       # the painted-band check
```

Integration tests drive a **real PTY** via `expectrl` and assert on what the outer terminal actually sees, not on gutter internals. They need a valid `TERM` (CI sets `xterm-256color`). The `--features oracle` build is the only one that compiles `src/oracle/` — the equivalence gate and the painted-band check both, each an in-crate `#[cfg(test)]` module; the default build never pulls the heavy wezterm dependency tree.

**Know what a green suite does not prove.** The render-thread unit tests read gutter's output back through `RecordingGrid` (`src/terminal.rs`), a mock that is itself a `vt100::Parser` — the same emulator gutter reasons with. It checks the band arithmetic and cannot, by construction, catch gutter's model disagreeing with a real terminal: vt100 has no deferred wrap, real terminals do. The equivalence gate does not cover this either — it diffs two grids built from the child's bytes and never sees what gutter writes. Anything about *where* bytes land on the real screen has to be proved against wezterm or a real PTY — in this repo that means the painted-band check in `src/oracle/band.rs`, which replays gutter's own bytes (`paint_frames_to_tape` in `src/render.rs` → a `Tape`) through wezterm-term at the physical screen size. See [ADR-001](docs/adr/0001-two-emulator-equivalence-gate.md).

## Architecture

**Five threads, one merged unbounded channel (`std::sync::mpsc`), no async.** Messages are a single enum (`src/msg.rs`): `Pty(Vec<u8>)`, `Input(Vec<u8>)`, `Resize` (payload-free), `ChildExited(ExitStatus)`, `ChildStopped { sig }`, `ChildContinued`, `PtyEof`.

- **Thread 1 — PTY reader (`src/pty.rs`):** dumb byte pump. Reads the child's output in bounded chunks through a bounded staging channel (`sync_channel(STAGING_DEPTH=64)`) that blocks it when full, and forwards them as `Msg::Pty`. Never writes the PTY, never scans bytes, stops on EOF.
- **Thread 2 — render loop (`src/render.rs`):** owns the parsers, the outer terminal handle, the PTY master writer and the input scanner (`src/scan.rs`) — **exclusively, no mutex**. The scanner sits here because the ESC-hold deadline runs on the loop's own injected clock and the token walk feeds the chord and resize-mode state the loop already owns. It is the only thread that writes output. Dispatches every message; runs the coalescing loop (see *Invariants to respect*).
- **Thread 3 — input reader (`src/input.rs`):** owns the outer tty read fd exclusively — two readers on one fd steal bytes from each other. A dumb `read()` pump: it forwards raw chunks as `Msg::Input(Vec<u8>)` and interprets nothing. Detached at spawn (`read()` is un-interruptible), reaped by `process::exit`. Not spawned at all when the keyboard will not open (see ADR-023).
- **Thread 4 — waiter (`src/waiter.rs`):** on unix, loops on raw `waitpid(pid, …, WUNTRACED|WCONTINUED)`, surviving the child's stop/continue events and only ending the thread on real death. This — not PTY EOF — is the authoritative shutdown trigger; it sends `Msg::ChildExited` on death, and `Msg::ChildStopped`/`Msg::ChildContinued` on stop/continue.
- **Thread 5 — SIGWINCH (`src/sigwinch.rs`):** signal-hook's iterator turns each `SIGWINCH` into a payload-free `Msg::Resize`; Thread 2 reads the real size when it handles it.

The PTY path is throttled upstream but the merged channel is unbounded, so a keystroke `send()` never blocks under a multi-MB PTY flood ([ADR-009](docs/adr/0009-merged-unbounded-channel.md)).

**Data flow (output):** child PTY → Thread 1 bounded staging → merged channel → Thread 2 `parser.process()` → `W`-column vt100 grid → `rows_diff` per-row byte runs → clipped for the row's placement (`src/rowclip.rs`) → Thread 2 emits `MoveTo(left_margin, offset + row)` + the row's bytes → outer terminal.

**Data flow (input):** outer tty → Thread 3 `read()` → merged channel → Thread 2 `Scanner::feed` → tokens → `write_all` to the PTY master, except mouse reports (translated by the mouse gate) and the reserved chord / in-mode resize keys (consumed). Between bracketed-paste guards nothing is extracted at all.

**Data flow (mode negotiation):** the child's keyboard-mode requests reach `unhandled_csi` → matched against the relay's allowlist (`src/relay.rs`) → emitted outward as canonical bytes; the three modes vt100 absorbs instead reach the outer terminal through `src/modes.rs`'s per-frame poll-diff. The terminal's reply comes back on the input fd and reaches the child by ordinary passthrough.

**Render model.** Thread 2 holds three parsers (`src/render.rs`):
- `parser` — live, carries callbacks (cursor-shape watcher, OSC-52 clipboard sink, device-query replies, keyboard-mode relay).
- `prev` — the diff baseline; `rows_diff(prev, 0, W)` yields the per-row byte runs to repaint.
- `scroll_tracker` — a band-sized mirror with bounded scrollback (`SCROLL_TRACKER_SCROLLBACK` = 4096), fed the same bytes and keeping its screen (scroll region and alt-screen flag included) until the scrollback runs out of room. The growth of its scrollback each frame is the count of lines that left the top, which gutter then sends to the real terminal's scrollback ([ADR-013](docs/adr/0013-inline-anchor-scroll-paint.md)).

`render_cell_walk()` is **tested but dormant**: gutter renders via `rows_diff` only, and right-edge safety rests on vt100's margin rule (a wide glyph never has its lead cell at column `W-1`; it wraps). The cell walk has no production caller and no runtime switch; it only goes live if a corrupting cell is actually observed on the `rows_diff` path. See [ADR-006](docs/adr/0006-band-fit-margin-rule.md).

### Module map

Most files map one-to-one onto a concern; the non-obvious split:

| File | Responsibility |
|------|----------------|
| `src/main.rs` | Orchestration: open the terminal (the open is the startup guard) and thread its path to the keyboard and clipboard opens, spawn PTY, eager mouse capture, start threads, run Thread 2. |
| `src/msg.rs` | The `Msg` enum every thread sends on the merged channel. |
| `src/pty.rs` | PTY spawn at `W × real_rows`, the `PtyResizer` trait (`TIOCSWINSZ`), and Thread 1. |
| `src/waiter.rs` | Thread 4, the stop-aware reaper. |
| `src/suspend.rs` | The `Suspender` trait (`kill(0, SIGTSTP)`, continue the child's group) so the suspend cycle's tests drive a recorder instead of stopping the test process. |
| `src/anchor.rs` | The input tty, and the startup CPR probe behind the inline anchor: hand-rolled so keystrokes typed during startup survive as leftover rather than vanishing into crossterm's event queue. The query goes out through the band's own sink, so the terminal that is asked is the one that answers. |
| `src/cli.rs` | Hand-rolled arg parse (no clap). `--width N\|Npct\|N%\|full`, `--center`/`--left`, `--resize-key`, `--version`. |
| `src/geometry.rs` | Pure layout maths: `margin()`, `resolve_width()` (absolute vs proportional), `physical_col()`, the resize-mode rail layout. No I/O; property-tested. |
| `src/rowclip.rs` | Makes a `rows_diff` row run safe to paint at one `Placement { left_margin, phys_row, grid_row }`: bounds its erases (`ESC[K`, `ESC[1K`, `ESC[2K`) to `W` and re-expresses the run's absolute `CUP`/`CHA` moves as absolute moves in the band's physical coordinates, tracking a virtual in-band column across the run. Pure — bytes in, bytes out. |
| `src/terminal.rs` | `OuterTerminal` trait abstracting every outer side effect; crossterm impl over a buffered tty handle (`open_tty_write` — `/dev/tty`, else the terminal gutter's own stdio names; also the startup guard), the size (`tty_size`) and raw-mode ioctls on that handle, `clear_gutter_bytes`, and the test mocks (`MockTerminal` for restore order, `RecordingGrid` for column assertions). |
| `src/callbacks.rs` | `vt100::Callbacks` impl holding the DECSCUSR cursor-shape watcher, the device-query replies, the keyboard-mode relay hook and the OSC-52 hook. |
| `src/input.rs` | Thread 3 — the dumb outer-tty read pump. Owns the read fd, interprets nothing. |
| `src/scan.rs` | The input scanner, on the render thread: raw bytes → tokens (forwarded runs, whole escape sequences, SGR-1006 mouse reports, paste spans) plus the `ESC_HOLD` constant (25 ms). Pure — no I/O, no clock, no terminal. Everything it does not extract is forwarded verbatim. |
| `src/sigwinch.rs` | signal-hook's `SIGWINCH` → `Msg::Resize`. |
| `src/chord.rs` | The reserved `--resize-key` chord and the byte forms it matches. |
| `src/relay.rs` | The child's keyboard-mode relay and its undo log: a closed allowlist of sequences forwarded outward as canonical bytes. |
| `src/modes.rs` | The three modes vt100 absorbs into screen state (DECCKM, application keypad, bracketed paste), mirrored onto the outer terminal by poll-diff. |
| `src/mouse.rs` | Pure forwarding gate: the child's live `(mode, encoding)` and one report in, translated/down-filtered SGR-1006 out, or a drop. Also owns the eager-capture bytes (`MOUSE_ENABLE`/`MOUSE_DISABLE`), which have to stay inside what the scanner can extract. |
| `src/clipboard.rs` | OSC-52 wire reconstruction → its own open of the resolved terminal (so clipboard write and frame repaint don't fight over fd state). Also the read-write open the input tty is made from. |
| `src/cursor.rs` | DECSCUSR cursor-shape mirroring to the outer terminal. |
| `src/clock.rs` | The `Clock` trait the render loop is generic over (now, deadline, blocking `recv`, bounded `recv_until`) and `RealClock`. The `VirtualClock` that drives the loop's tests on scripted time lives in `src/render.rs`'s tests. |
| `src/oracle/` | **Feature-gated**, two checks over wezterm-term (see below). `mod.rs` wires wezterm-term; `gate.rs` + `cellview.rs` are the equivalence gate; `band.rs` is the painted-band check (`Tape`, `BandRect`). |

`src/snapshots/` holds the `insta` snapshots for the render tests.

### Invariants to respect

These are easy to break. Each has a full record under `docs/adr/`; the one-liners below are the quick reference.

- **Coalescing loop.** Fixed-deadline ~60fps coalescer; the explicit `now >= deadline` burst-exit check is mandatory. See [ADR-007](docs/adr/0007-coalescing-loop.md).
- **A painted row stays inside the band.** Each changed row goes out after one `move_to(left_margin, phys_row)`, prefixed with `ESC[m`. The clipper rewrites every erase in the run into a fill of the band's own columns and every absolute move (and `\r`) into an absolute `CUP` onto the same cell in the band's physical coordinates. It stays an *absolute* `CUP`, never a relative hop such as `CUB`: after a glyph lands in the screen's last column, deferred wrap leaves the real cursor a column behind the clipper's tracker — and the band's right edge is the screen's on a default `gutter claude` and on every `--width full` run. A clipped run is therefore correct at one placement only. The same rule covers scrolling: `newline` resets the SGR before it scrolls, and a row repainted in place during a scrolling frame blanks the band's columns first. Host autowrap is off for the whole run and turned back on only by teardown and park. See [ADR-014](docs/adr/0014-row-run-self-containment.md).
- **A gutter clear erases to the terminal's real edge, and only across the band's rows.** Outside the band, clears are `ESC[K` from `band_end` and `ESC[1K` at `margin - 1` — never runs of spaces sized from the width gutter last read, which overrun a terminal that has since shrunk and leave the row soft-wrapped. All three `OuterTerminal` implementations share one emitter, `clear_gutter_bytes`. The clear spans `0..rows` on the alt screen and `base_row..rows` on the primary screen, so shell history above the band is never touched. See [ADR-024](docs/adr/0024-terminal-relative-gutter-erases.md) and [ADR-017](docs/adr/0017-uniform-margin-management.md).
- **Resize order (on the render thread).** Recompute `W` → `resizer.resize(W, rows)` (`TIOCSWINSZ` first) → `parser.set_size(rows, W)` (mind the `(rows, cols)` order; the scroll tracker too) → recompute margin → baseline reset and gutter clear. A resize-mode step follows the same order with the real width held fixed. See [ADR-008](docs/adr/0008-resize-ordering.md) and [ADR-016](docs/adr/0016-modal-resize.md).
- **One terminal, and opening it is the guard.** The band's sink, the keyboard, the clipboard, the size and the termios all come from one device: `/dev/tty`, or — with no controlling terminal — the terminal `ttyname` names on stdin, stdout or stderr, reopened by name. The size is a `TIOCGWINSZ` and raw mode a `tcgetattr`/`tcsetattr` on the sink's own fd; nothing asks crossterm. stdout is never written (`--version` excepted), so `gutter cmd > log` paints on screen and leaves the log empty. If the write open fails gutter prints one line to stderr and exits 1 before spawning the child; a read-write open that fails only drops the keyboard (watch-only run). The read side is opened once and shared by the startup probe and Thread 3, or they race for the CPR reply. The clipboard gets its own open, never a `dup` of the sink. See [ADR-023](docs/adr/0023-controlling-terminal-fd-model.md) and [ADR-004](docs/adr/0004-osc52-clipboard-separate-tty.md).
- **Teardown is explicit and ordered, and best-effort per step.** No destructors after `process::exit`; restore by hand in ADR-010's fixed order with raw mode disabled **last**, each step only if what it undoes was set up, and every step attempted even after an earlier one failed. The suspend path's `park()` runs the same list. The sink is a `BufWriter`, so `show_cursor`'s flush is what puts the restore on screen. See [ADR-010](docs/adr/0010-ordered-teardown.md).
- **Keyboard is never interpreted.** Bytes from the outer tty go to the child untouched, except SGR mouse reports (translated), the reserved chord, the in-mode resize keys and the paste guards. The child's mode requests are relayed out so the child and the terminal negotiate directly. See [ADR-020](docs/adr/0020-raw-input-passthrough.md), [ADR-016](docs/adr/0016-modal-resize.md) and [ADR-021](docs/adr/0021-child-driven-mode-relay.md).
- **The ESC-hold flushes whole, never split.** An ambiguous `ESC` prefix is withheld until the sequence completes or `ESC_HOLD` fires; on timeout the entire pending buffer is flushed verbatim and in order. Splitting it leaks escape-sequence fragments into the child's prompt. Nothing overtakes the hold buffer. See [ADR-020](docs/adr/0020-raw-input-passthrough.md).
- **Relayed bytes are canonical, never re-serialised from parameters.** vte cannot tell an omitted CSI parameter from an explicit `0`, so joining parameters back together turns the kitty query `CSI ? u` into `CSI ? 0 u` and a bare pop into a zero-level pop. Each matched shape emits a fixed string. See [ADR-021](docs/adr/0021-child-driven-mode-relay.md).
- **No DECSET is ever relayed.** The three absorbed modes are mirrored by poll-diff against their own record (not against `prev`); nothing else crosses. Modes with a display side effect, and modes ADR-005 or ADR-012 already own, are never touched — mouse modes above all. See [ADR-022](docs/adr/0022-absorbed-mode-mirroring.md).
- **Mouse: eager capture and a per-report gate.** One enable at startup, one disable at teardown, both written by gutter itself. For each SGR-1006 report the scanner extracts, the render thread reads the child's live mouse mode and encoding and runs the report through the gate, which translates it into band coordinates or drops it. Only SGR ever reaches the child; a report the child could only take in another encoding is dropped, never fatal. See [ADR-005](docs/adr/0005-mouse-eager-capture-poll-gate.md).
- **Width.** `--width N` is absolute, `--width Npct`/`N%` proportional; the child is always told it owns `W` columns. See [ADR-011](docs/adr/0011-width-resolution.md).
- **Outer screen mirrors the child.** Mirror the child's alt-screen on its edges; the band anchors at `base_row` and flows inline on the primary screen. See [ADR-012](docs/adr/0012-screen-mode-mirroring.md) and [ADR-013](docs/adr/0013-inline-anchor-scroll-paint.md).
- **Child stop is detected via `waitpid(WUNTRACED)`, never a signal handler, and the waiter is the only reaper.** Raw mode strips `ISIG` on the outer tty and the child's SIGTSTP is scoped to its own session — gutter's process can never receive it directly. Once raw `waitpid` owns the pid nothing else may wait on it: no `child.wait()`, no `try_wait()`. See [ADR-018](docs/adr/0018-stop-aware-waiter.md).
- **Suspend/resume is straight-line code on the render thread.** Park the outer terminal (ADR-010 order) → `kill(0, SIGTSTP)` → the process freezes until `fg` → unpark (raw mode first) → reseed the inline anchor → catch up any missed resize → continue the child's group, last. No SIGCONT handler. See [ADR-019](docs/adr/0019-suspend-resume-cycle-ordering.md).

The full set is indexed in [docs/adr/README.md](docs/adr/README.md).

## The two oracle checks & the fixtures

The `oracle` feature carries two separate checks, both diffing against wezterm-term but asking different questions. Both run under `cargo test --bin gutter --features oracle`.

**The equivalence gate** (`src/oracle/gate.rs`, tests `recorded_target_equivalence_gate_passes`, `wide_edge_equivalence_gate_passes` and `gate_uses_two_different_emulators`) asks whether vt100 made the same grid of the child's bytes that an independent emulator did. It replays one byte stream through both at the same width and diffs cells.

**The painted-band check** (`src/oracle/band.rs`) asks whether gutter's own output lands where gutter thinks it does. `paint_frames_to_tape` paints real frames through the production render path into a `Tape` — the production `CrosstermTerminal` writing into a `Vec<u8>` — and the check replays that tape through wezterm-term at the *physical* screen size, diffs the band's rectangle against the child's `W`-column vt100 grid, and checks that nothing landed outside the band. It covers the case the rest of the suite cannot: a band whose right edge is the screen's, where deferred wrap is live and vt100 and a real terminal disagree about where the cursor is.

`tests/equivalence_pty.rs` is **not** the gate: it runs the same fixtures end to end through the real binary over a real PTY, needs no feature, and checks that gutter survives them.

The fixtures under `tests/fixtures/*.cast` are **not asciinema recordings** — they are raw, timing-less, *settled* VT byte streams recorded at a declared width and consumed via `include_bytes!`. Current corpus: `claude-code-flow.cast` (the Claude Code baseline), `plain-scroll.cast` (primary-screen scrolling past a screenful), `wide-edge.cast` (CJK/emoji at the band edge). `*.allowlist` files record benign vt100↔wezterm divergences that have been reviewed and blessed; the gate passes when there are **zero corrupting** cells.

The declared width a fixture is captured at **must** equal the `W` it is replayed at, by either check, or the cell-by-cell diff silently misaligns. `scripts/capture-fixture.sh <cols> <rows> <out.cast> -- <cmd>` is the helper to record a new one at an exact PTY size (review the bytes before checking in).

## Working conventions

Work proceeds in thin **vertical slices**, each cutting through every layer it touches and leaving the binary runnable and green. Commits carry an effort-scoped slice tag — `feat(kbd-1): …`, `test(kbd-3): …`, `docs(kbd-4): …` — where the word names the effort and the number the slice within it. (The first iteration used bare slice numbers, `feat(04): …`; those are still in the log.)

**Changelog wording lives in a trailer.** `git cliff` builds `CHANGELOG.md` from the log, and a slice subject names the slice, not the change a user sees. So a `feat`/`fix` commit worth a release note carries one `Changelog:` trailer per bullet, written in the user's terms — several are fine when one slice fixed several things. `Changelog: skip` as the only trailer drops the commit; a commit with no trailer falls back to its subject. Every other commit type (`docs`, `test`, `refactor`, `chore` and the rest) is skipped wholesale, so it needs nothing. Efforts spanning many slices squash-merge under one subject with the trailers on the squash commit. Write the trailers while the work is fresh — nothing downstream can recover wording that was never written.

## Releasing

[RELEASING.md](RELEASING.md) is the authority; this is the shape of it.

Releases are local-drive — nothing happens that someone didn't trigger. The dry run writes the changelog section, that section is reviewed and committed on its own, then `cargo release <patch|minor> --execute` bumps `Cargo.toml`, runs the pre-release hook, commits, tags `v0.x.y` and pushes — two commits per release, because cargo-release refuses to run with a dirty tree; **the tag push is what fires `release.yml`**, which cross-builds the Linux x86_64 and universal macOS binaries and creates the GitHub Release with notes lifted from the matching `CHANGELOG.md` section. Dry-run is the default (leave off `--execute`). Pre-1.0, breaking changes ride the minor slot. Releases are refused from anywhere but `main`.

The hook is `scripts/changelog.sh`, and it **prepends** the new section rather than regenerating the file. A released section is what a published GitHub Release says, so a later change to `cliff.toml`'s parsers must not go back and reword it, and a section edited by hand has to survive the next release. The script adds the compare link too, which git-cliff's footer only writes when it generates the whole file. It is idempotent — the dry run writes the section you review, and the real run then leaves it alone, which is also what makes a hand edit safe. That edit has to be committed before `--execute`, or cargo-release stops on the dirty tree.

`cliff.toml` decides what reaches the changelog at all: only `feat` and `fix`, everything else skipped, with `Changelog:` trailers preferred over subjects (see *Working conventions* above). Entries dated before v0.3.0 predate that and read like slice subjects; the v0.3.0 section was rewritten by hand for the same reason.

## Agent skills

### Issue tracker

Issues and PRDs are tracked as local markdown files under `.scratch/<feature>/` (no GitHub Issues; PRs are not a triage surface). See `docs/agents/issue-tracker.md`.

### Triage labels

Five canonical roles, used verbatim: `needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: one `CONTEXT.md` + `docs/adr/` at the repo root (created lazily when needed). See `docs/agents/domain.md`.
