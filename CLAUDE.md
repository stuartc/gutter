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

# Run it: gutter [--width <N|Npct|full>] [--center|--left] <cmd> [args...]
cargo run -- --width 80 --center bash      # absolute 80-col band, centred
cargo run -- --width 50pct --left vim      # proportional (recomputed on resize); 50% also works
cargo run -- bash                          # no --width → 100-column centred band (clamped to narrower terminals)
cargo run -- --width full bash             # full width, transparent passthrough
```

There is no `--help`; an unknown leading token is treated as the command. There is no `cargo fmt`/coverage step in CI — don't invent one.

**Single test** (`cargo test <name-substring>` filters across all targets):

```bash
cargo test passthrough_echo                       # by name, any target
cargo test --test passthrough                      # one integration file
cargo test --test equivalence_pty --features oracle   # the gate (needs the feature)
cargo test --bin gutter                            # only the in-crate unit tests (there is no lib target)
```

Integration tests drive a **real PTY** via `expectrl` and assert on what the outer terminal actually sees, not on gutter internals. They need a valid `TERM` (CI sets `xterm-256color`). The `--features oracle` build is the only one that compiles `src/oracle/` and the equivalence gate; the default build never pulls the heavy wezterm dependency tree.

## Architecture

**Five threads, one merged unbounded channel (`std::sync::mpsc`), no async.** Messages are a single enum (`src/msg.rs`): `Pty(Vec<u8>)`, `Input(Vec<u8>)`, `Resize` (payload-free), `ChildExited(ExitStatus)`, `ChildStopped`, `ChildContinued`, `PtyEof`.

- **Thread 1 — PTY reader (`src/pty.rs`):** dumb byte pump. Reads the child's output in bounded chunks through a backpressure seam (`sync_channel(STAGING_DEPTH=64)`), forwards as `Msg::Pty`. Never writes the PTY, never scans bytes, stops on EOF.
- **Thread 2 — render loop (`src/render.rs`):** owns the `vt100::Parser`, the outer terminal handle, the PTY master writer and the input scanner (`src/scan.rs`) — **exclusively, no mutex**. The scanner sits here because the ESC-hold deadline runs on the loop's own injected clock and the token walk feeds the chord and resize-mode state the loop already owns. It is the only thread that writes output. Dispatches every message; runs the coalescing loop (see *Invariants to respect*).
- **Thread 3 — input reader (`src/input.rs`):** owns the outer tty read fd exclusively — two readers on one fd steal bytes from each other. A dumb `read()` pump: it forwards raw chunks as `Msg::Input(Vec<u8>)` and interprets nothing — the scanner lives on Thread 2 (ADR-020). Detached at spawn (`read()` is un-interruptible), reaped by `process::exit`.
- **Thread 4 — waiter (`src/waiter.rs`):** on unix, loops on raw `waitpid(pid, …, WUNTRACED|WCONTINUED)`, surviving the child's stop/continue events and only ending the thread on real death. This — not PTY EOF — is the authoritative shutdown trigger; it sends `Msg::ChildExited` on death, and `Msg::ChildStopped`/`Msg::ChildContinued` on stop/continue (see *Invariants to respect*).
- **Thread 5 — SIGWINCH (`src/sigwinch.rs`):** signal-hook's iterator turns each `SIGWINCH` into a payload-free `Msg::Resize`; Thread 2 reads the real size when it handles it.

**Channel topology:** four sources feed the one channel — the PTY reader, the input reader, the waiter and the SIGWINCH thread. The PTY path is throttled upstream (bounded `sync_channel`) but the merged channel is unbounded, so a keystroke `send()` never blocks under a multi-MB PTY flood. Because Thread 2 alone owns the parser, there is no shared mutable state and no parser mutex.

**Data flow (output):** child PTY → Thread 1 bounded staging → merged channel → Thread 2 `parser.process()` → `W`-column vt100 grid → `rows_diff` per-row byte runs → Thread 2 emits `MoveTo(left_margin, row)` + row bytes → outer terminal.

**Data flow (input):** outer tty → Thread 3 `read()` → merged channel → Thread 2 `Scanner::feed` → tokens → `write_all` to the PTY master, except mouse reports (translated by the gate) and the reserved chord / in-mode resize keys (consumed). Between bracketed-paste guards nothing is extracted at all.

**Data flow (mode negotiation):** the child's keyboard-mode requests reach `unhandled_csi` → matched against the relay's allowlist (`src/relay.rs`) → emitted outward as canonical bytes; the three modes vt100 absorbs instead reach the outer terminal through `src/modes.rs`'s per-frame poll-diff. The terminal's reply comes back on the input fd and reaches the child by ordinary passthrough.

**Render model.** Thread 2 holds three parsers (`src/render.rs`):
- `parser` — live, carries callbacks (cursor-shape watcher, OSC-52 clipboard sink, device-query replies, keyboard-mode relay).
- `prev` — diff baseline, no callbacks; `rows_diff(prev, 0, W)` yields the per-row byte runs to repaint.
- `scroll_tracker` — a band-sized mirror with bounded scrollback (4096), fed the same bytes, used to count lines that scrolled off the top so they reach the real terminal's scrollback.

`render_cell_walk()` is **tested-but-dormant** (the "A1 rows_diff-only assumption"): gutter renders via `rows_diff` only, and right-edge safety rests on vt100's margin rule (a wide glyph never has its lead cell at column `W-1`; it wraps). The cell-walk is insurance against a corrupting cell the design believes cannot occur — there is no runtime chooser. It only goes live if a corrupting cell is actually observed on the `rows_diff` path.

### Module map

Most files map one-to-one onto a concern; the non-obvious split:

| File | Responsibility |
|------|----------------|
| `src/main.rs` | Orchestration: open the controlling terminal (the open is the startup guard), spawn PTY, eager mouse capture, open the `/dev/tty` clipboard sink, start threads, run Thread 2. |
| `src/anchor.rs` | The input tty, and the startup CPR probe behind the inline anchor: hand-rolled so keystrokes typed during startup survive as leftover rather than vanishing into crossterm's event queue. The query goes out through the band's own sink, so the terminal that is asked is the one that answers. |
| `src/cli.rs` | Hand-rolled arg parse (no clap). `--width N\|Npct\|N%`, `--center`/`--left`. |
| `src/geometry.rs` | Pure layout maths: `margin()`, `resolve_width()` (absolute vs proportional), `physical_col()`. No I/O; property-tested. |
| `src/terminal.rs` | `OuterTerminal` trait abstracting every outer side effect; crossterm impl over a buffered `/dev/tty` handle (`open_tty_write`, also the startup guard) + a recording mock for restore-order / column assertions. |
| `src/callbacks.rs` | `vt100::Callbacks` impl holding the DECSCUSR cursor-shape watcher, the device-query replies, the keyboard-mode relay hook and the OSC-52 hook. |
| `src/input.rs` | Thread 3 — the dumb outer-tty read pump. Owns the read fd, interprets nothing. |
| `src/scan.rs` | The input scanner, on the render thread: raw bytes → tokens (forwarded runs, whole escape sequences, SGR-1006 mouse reports, paste spans) plus the `ESC_HOLD` constant. Pure — no I/O, no clock, no terminal. Everything it does not extract is forwarded verbatim. |
| `src/sigwinch.rs` | signal-hook's `SIGWINCH` → `Msg::Resize`. |
| `src/chord.rs` | The reserved `--resize-key` chord and the byte forms it matches. |
| `src/relay.rs` | The child's keyboard-mode relay and its undo log: a closed allowlist of sequences forwarded outward as canonical bytes. |
| `src/modes.rs` | The three modes vt100 absorbs into screen state (DECCKM, application keypad, bracketed paste), mirrored onto the outer terminal by poll-diff. |
| `src/mouse.rs` | Pure forwarding gate: live `(mode, encoding)` in, translated/down-filtered SGR-1006 out. Also owns the eager-capture wire bundle, which has to stay inside what the scanner can extract. |
| `src/clipboard.rs` | OSC-52 wire reconstruction → its own open of `/dev/tty` (so clipboard write and frame repaint don't fight over fd state). Also the read-write open the input tty is made from. |
| `src/cursor.rs` | DECSCUSR cursor-shape mirroring to the outer terminal. |
| `src/clock.rs` | Injectable clock + receiver, so the coalescing loop is unit-testable with a virtual clock and scripted messages — no real PTY, no threads. |
| `src/oracle/` | **Feature-gated** equivalence gate (`gate.rs`, `cellview.rs`): replays bytes through both vt100 and wezterm-term at the same width and diffs cells. |

### Invariants to respect

These are load-bearing and easy to break. Each has a full record under `docs/adr/`;
the one-liners below are the quick reference.

- **Coalescing loop.** Fixed-deadline ~60fps coalescer; the explicit `now >= deadline` burst-exit check is mandatory. See [ADR-007](docs/adr/0007-coalescing-loop.md).
- **Resize order (on the render thread).** Recompute `W` → `resizer.resize(W, rows)` (TIOCSWINSZ first) → `parser.set_size(rows, W)` (mind the `(rows, cols)` order) → recompute margin → baseline reset. See [ADR-008](docs/adr/0008-resize-ordering.md).
- **One terminal, and opening it is the guard.** The band's sink, the keyboard, the clipboard and the size all come from `/dev/tty`, the controlling terminal; stdout is never written, so `gutter cmd > log` paints on screen and leaves the log empty. No controlling terminal means one line to stderr and exit 1, before the child is spawned. The read side is opened once — the startup probe and Thread 3 share it, or they race for the CPR reply. See [ADR-023](docs/adr/0023-controlling-terminal-fd-model.md).
- **Teardown is explicit and ordered.** No destructors after `process::exit`; restore by hand, each step conditional on what was set up. The sink is a `BufWriter`, which `process::exit` will not land for you, so the flush behind the last restore step is correctness. See [ADR-010](docs/adr/0010-ordered-teardown.md) and [ADR-023](docs/adr/0023-controlling-terminal-fd-model.md).
- **Keyboard is never interpreted.** Bytes from the outer tty go to the child untouched, except SGR mouse reports (translated), the reserved chord, the in-mode resize keys and the paste guards. The child's mode requests are relayed out so the child and the terminal negotiate directly. See [ADR-020](docs/adr/0020-raw-input-passthrough.md) and [ADR-021](docs/adr/0021-child-driven-mode-relay.md).
- **The ESC-hold flushes whole, never split.** An ambiguous `ESC` prefix is withheld until the sequence completes or the timeout fires; on timeout the entire pending buffer is flushed verbatim and in order. Splitting it leaks escape-sequence fragments into the child's prompt. Nothing overtakes the hold buffer. See [ADR-020](docs/adr/0020-raw-input-passthrough.md).
- **Relayed bytes are canonical, never re-serialised from parameters.** vte cannot tell an omitted CSI parameter from an explicit `0`, so joining parameters back together turns the kitty query `CSI ? u` into `CSI ? 0 u` and a bare pop into a zero-level pop. Each matched shape emits a fixed string. See [ADR-021](docs/adr/0021-child-driven-mode-relay.md).
- **No DECSET is ever relayed.** The three absorbed modes are mirrored by poll-diff; nothing else crosses. Modes with a display side effect, and modes ADR-005 or ADR-012 already own, are never touched. See [ADR-022](docs/adr/0022-absorbed-mode-mirroring.md).
- **Mouse: eager capture.** One enable at startup, one disable at teardown, both written by gutter itself; a per-frame poll-diff gate translates and re-encodes the SGR-1006 reports the scanner extracts. See [ADR-005](docs/adr/0005-mouse-eager-capture-poll-gate.md).
- **Width.** `--width N` is absolute, `--width Npct`/`N%` proportional; the child is always told it owns `W` columns. See [ADR-011](docs/adr/0011-width-resolution.md).
- **Outer screen mirrors the child.** Mirror the child's alt-screen on its edges; the band anchors at `base_row` and flows inline on the primary screen. See [ADR-012](docs/adr/0012-screen-mode-mirroring.md), [ADR-013](docs/adr/0013-inline-anchor-scroll-paint.md) and [ADR-022](docs/adr/0022-absorbed-mode-mirroring.md).
- **Child stop is detected via `waitpid(WUNTRACED)`, never a signal handler.** Raw mode strips `ISIG` on the outer tty and the child's SIGTSTP is scoped to its own session — gutter's process can never receive it directly. See [ADR-018](docs/adr/0018-stop-aware-waiter.md).
- **Suspend/resume is straight-line code on the render thread.** Park the outer terminal (ADR-010 order) → `kill(0, SIGTSTP)` → the process freezes until `fg` → unpark (raw mode first) → continue the child's group. No SIGCONT handler. See [ADR-019](docs/adr/0019-suspend-resume-cycle-ordering.md).

The full set (including the equivalence gate, clipboard fd, margin rule, channel
topology, and row clipping) is indexed in [docs/adr/README.md](docs/adr/README.md).

## Equivalence gate & fixtures

The fixtures under `tests/fixtures/*.cast` are **not asciinema recordings** — they are raw, timing-less, *settled* VT byte streams recorded at a declared width and consumed via `include_bytes!`. Current corpus: `claude-code-flow.cast` (the Claude Code baseline), `plain-scroll.cast` (primary-screen scrolling past a screenful), `wide-edge.cast` (CJK/emoji at the band edge). `*.allowlist` files record benign vt100↔wezterm divergences that have been reviewed and blessed; the gate passes when there are **zero corrupting** cells.

The declared width a fixture is captured at **must** equal the `W` the gate replays it at, or the cell-by-cell diff silently misaligns. `scripts/capture-fixture.sh <cols> <rows> <out.cast> -- <cmd>` is the helper to record a new one at an exact PTY size (review the bytes before checking in).

## Working conventions

Work proceeds in thin **vertical slices**, each cutting through every layer it touches and leaving the binary runnable and green. Commits carry an effort-scoped slice tag — `feat(kbd-1): …`, `test(kbd-3): …`, `docs(kbd-4): …` — where the word names the effort and the number the slice within it. (The first iteration used bare slice numbers, `feat(04): …`; those are still in the log.)

## Agent skills

### Issue tracker

Issues and PRDs are tracked as local markdown files under `.scratch/<feature>/` (no GitHub Issues; PRs are not a triage surface). See `docs/agents/issue-tracker.md`.

### Triage labels

Five canonical roles, used verbatim: `needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: one `CONTEXT.md` + `docs/adr/` at the repo root (created lazily when needed). See `docs/agents/domain.md`.
