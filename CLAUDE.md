# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is gutter?

**gutter** spawns a child command in a PTY and renders it inside a narrower column band (centred or left-aligned) while the child behaves as if it owned the full terminal. Keyboard, mouse, clipboard (OSC 52), and resize all pass through transparently. It is not a prefix-with-spaces hack: gutter embeds a terminal emulator — it sizes the child's PTY at `W × real_rows` (where `W` is the band width), parses the child's output into a `W`-column virtual grid (`vt100`), and repaints that grid at a left-margin column offset on the real terminal. The original target is wrapping Claude Code in a constrained width.

## Build, test, run

Toolchain is pinned to **Rust 1.96.0** (`.tool-versions`).

```bash
cargo build                         # debug build → target/debug/gutter
cargo test                          # unit + integration tests (no oracle)
cargo test --features oracle        # also builds & runs the equivalence gate
cargo clippy --all-targets
cargo clippy --all-targets --features oracle

# Run it: gutter [--width <N|Npct>] [--center|--left] <cmd> [args...]
cargo run -- --width 80 --center bash      # absolute 80-col band, centred
cargo run -- --width 50pct --left vim      # proportional (recomputed on resize); 50% also works
cargo run -- bash                          # no --width → full width, transparent passthrough
```

There is no `--help`; an unknown leading token is treated as the command. There is no `cargo fmt`/coverage step in CI — don't invent one.

**Single test** (`cargo test <name-substring>` filters across all targets):

```bash
cargo test passthrough_echo                       # by name, any target
cargo test --test passthrough                      # one integration file
cargo test --test equivalence_pty --features oracle   # the gate (needs the feature)
cargo test --lib                                   # only the in-crate unit tests
```

Integration tests drive a **real PTY** via `expectrl` and assert on what the outer terminal actually sees, not on gutter internals. They need a valid `TERM` (CI sets `xterm-256color`). The `--features oracle` build is the only one that compiles `src/oracle/` and the equivalence gate; the default build never pulls the heavy wezterm dependency tree.

## Architecture

**Four threads, one merged unbounded channel (`std::sync::mpsc`), no async.** Messages are a single enum (`src/msg.rs`): `Pty(Vec<u8>)`, `Input(Event)`, `ChildExited(ExitStatus)`.

- **Thread 1 — PTY reader (`src/pty.rs`):** dumb byte pump. Reads the child's output in bounded chunks through a backpressure seam (`sync_channel(STAGING_DEPTH=64)`), forwards as `Msg::Pty`. Never writes the PTY, never scans bytes, stops on EOF.
- **Thread 2 — render loop (`src/render.rs`):** owns the `vt100::Parser`, the outer terminal handle, and the PTY master writer — **exclusively, no mutex**. It is the only thread that writes output. Dispatches every message; runs the coalescing loop (see *Invariants to respect*).
- **Thread 3 — input reader (`src/input.rs`):** owns crossterm's event source exclusively (crossterm 0.29 requires same-thread reads). Forwards each decoded `Event` as `Msg::Input`. Detached at spawn (`event::read()` is un-interruptible), reaped by `process::exit`.
- **Thread 4 — waiter (`src/waiter.rs`):** blocks on `child.wait()`. This — not PTY EOF — is the authoritative shutdown trigger; it sends `Msg::ChildExited`.

**Channel topology:** the PTY path is throttled upstream (bounded `sync_channel`) but the merged channel is unbounded, so a keystroke `send()` never blocks under a multi-MB PTY flood. Because Thread 2 alone owns the parser, there is no shared mutable state and no parser mutex.

**Data flow:** child PTY → Thread 1 bounded staging → merged channel → Thread 2 `parser.process()` → `W`-column vt100 grid → `rows_diff` per-row byte runs → Thread 2 emits `MoveTo(left_margin, row)` + row bytes → outer terminal.

**Render model.** Thread 2 holds three parsers (`src/render.rs`):
- `parser` — live, carries callbacks (kitty watcher, cursor-shape watcher, OSC-52 clipboard sink).
- `prev` — diff baseline, no callbacks; `rows_diff(prev, 0, W)` yields the per-row byte runs to repaint.
- `scroll_tracker` — a band-sized mirror with bounded scrollback (4096), fed the same bytes, used to count lines that scrolled off the top so they reach the real terminal's scrollback.

`render_cell_walk()` is **tested-but-dormant** (the "A1 rows_diff-only assumption"): gutter renders via `rows_diff` only, and right-edge safety rests on vt100's margin rule (a wide glyph never has its lead cell at column `W-1`; it wraps). The cell-walk is insurance against a corrupting cell the design believes cannot occur — there is no runtime chooser. It only goes live if a corrupting cell is actually observed on the `rows_diff` path.

### Module map

Most files map one-to-one onto a concern; the non-obvious split:

| File | Responsibility |
|------|----------------|
| `src/main.rs` | Orchestration: spawn PTY, probe kitty capability, eager mouse capture, open `/dev/tty` clipboard sink, start threads, run Thread 2. |
| `src/cli.rs` | Hand-rolled arg parse (no clap). `--width N\|Npct\|N%`, `--center`/`--left`. |
| `src/geometry.rs` | Pure layout maths: `margin()`, `resolve_width()` (absolute vs proportional), `physical_col()`. No I/O; property-tested. |
| `src/terminal.rs` | `OuterTerminal` trait abstracting every outer side effect; crossterm impl + a recording mock for restore-order / column assertions. |
| `src/callbacks.rs` | `vt100::Callbacks` impl holding the kitty-level watcher and the DECSCUSR cursor-shape watcher. |
| `src/keyboard/` | `kitty_state.rs` (push/pop level stack, clamped to outer capability), `encode.rs` (`KeyEvent` → legacy or kitty `CSI…u` bytes). |
| `src/mouse.rs` | Pure forwarding gate: live `(mode, encoding)` in, translated/down-filtered SGR-1006 out. |
| `src/clipboard.rs` | OSC-52 wire reconstruction → separate `/dev/tty` (so clipboard write and frame repaint don't fight over fd state). |
| `src/cursor.rs` | DECSCUSR cursor-shape mirroring to the outer terminal. |
| `src/clock.rs` | Injectable clock + receiver, so the coalescing loop is unit-testable with a virtual clock and scripted messages — no real PTY, no threads. |
| `src/oracle/` | **Feature-gated** equivalence gate (`gate.rs`, `cellview.rs`): replays bytes through both vt100 and wezterm-term at the same width and diffs cells. |

### Invariants to respect

These are load-bearing and easy to break:

- **Coalescing loop.** The render loop is a fixed-deadline ~60fps coalescer with an explicit `now >= deadline` burst-exit check — this is **mandatory**, because `recv_timeout` never times out under a saturating burst, so a naive loop would starve rendering. One render per frame; zero idle CPU (single blocking `recv()`).
- **Resize order (same turn, on Thread 2).** Recompute `W` → `resizer.resize(W, rows)` (TIOCSWINSZ to the child *first*) → `parser.set_size(rows, W)` (note the `(rows, cols)` argument order) → recompute margin → gutter clear (alt-screen only) + diff-baseline reset.
- **Teardown is explicit and ordered.** No destructors run after `process::exit`, so restore by hand, in order: leave alt screen → pop kitty flags → disable mouse → show cursor → disable raw mode — each step conditional on what was actually set up.
- **Keyboard is always re-encoded.** crossterm yields a decoded `KeyEvent` only, so there is no verbatim passthrough; re-encode at the child's negotiated level. Two independent kitty states: the outer terminal's capability (probed once, clamps the child) and the child's live level (a stack driven by the `unhandled_csi` watcher).
- **Mouse: eager capture.** One enable at startup, one disable at teardown (kills the dropped-first-click race). Each frame, a poll-diff gate reads the child's live `(mode, encoding)`, translates coordinates (subtract margin, drop the gutter), down-filters motion, and re-encodes SGR-1006.
- **Width.** `--width N` is absolute (fixed for the session); `--width Npct`/`N%` is proportional (recomputed on every resize). The child is *always* told it has `W` columns, never the real terminal width.
- **Outer screen mirrors the child.** Read `screen.alternate_screen()` each frame and toggle the outer alt-screen on its edges. Plain commands stay on the primary screen so their output survives exit; an `ever_entered_alt` latch guards the on-exit replay.

## Equivalence gate & fixtures

The fixtures under `tests/fixtures/*.cast` are **not asciinema recordings** — they are raw, timing-less, *settled* VT byte streams recorded at a declared width and consumed via `include_bytes!`. Current corpus: `claude-code-flow.cast` (the Claude Code baseline), `plain-scroll.cast` (primary-screen scrolling past a screenful), `wide-edge.cast` (CJK/emoji at the band edge). `*.allowlist` files record benign vt100↔wezterm divergences that have been reviewed and blessed; the gate passes when there are **zero corrupting** cells.

The declared width a fixture is captured at **must** equal the `W` the gate replays it at, or the cell-by-cell diff silently misaligns. `scripts/capture-fixture.sh <cols> <rows> <out.cast> -- <cmd>` is the helper to record a new one at an exact PTY size (review the bytes before checking in).

## Working conventions

Work proceeds in thin **vertical slices**, each cutting through every layer it touches and leaving the binary runnable and green. Commits are prefixed with the slice number: `feat(04): …`, `test(06): … (A2)`, `docs(05): …`.
