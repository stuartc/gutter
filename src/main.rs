//! gutter — a one-pane terminal multiplexer that runs a command inside a
//! narrower column band, transparent to keyboard/mouse/clipboard/resize.
//!
//! Architecture: three live threads + a waiter, one merged unbounded channel,
//! no async runtime. See `.context/design/CONTEXT.md` for the full design brief.
//!
//! - Thread 1 (`pty::reader`)    — dumb PTY byte pump, self-throttled via a
//!   bounded staging `sync_channel(N)` (the backpressure seam, ADR-007/009).
//! - Thread 2 (`render::run`)     — owns the `vt100::Parser`, the outer
//!   terminal handle and the sole PTY-master writer; runs the fixed-deadline
//!   60fps coalescing loop and the offset repaint.
//! - Thread 3 (`input::run`)      — owns crossterm's event source exclusively.
//! - Thread 4 (`waiter::run`)     — blocks on `child.wait()`, the authoritative
//!   child-death signal (ADR-010).
//!
//! Virtual grid + offset repaint (slice 02): the PTY is sized `W × real_rows`
//! so the child believes it owns a `W`-wide terminal; its output is parsed into
//! a `W`-column vt100 grid on the render thread and repainted to the real
//! terminal at a left-margin column offset (left-aligned, fixed `W`).
//!
//! Keyboard re-encode + kitty negotiation (slice 04): gutter ALWAYS re-encodes
//! each `KeyEvent` (crossterm gives no raw bytes) at the child's negotiated
//! kitty level, tracking the child's `CSI > N u` push/pop stack via the shared
//! callbacks struct and clamping it to the outer terminal's
//! `supports_keyboard_enhancement()` capability (ADR-002/003).
//!
//! Resize + alignment + proportional width (slice 05): `Event::Resize` arrives as
//! `Msg::Input` and runs on Thread 2 — the only owner of the parser. The handler
//! recomputes `W` (for a proportional `--width Npct`), resizes the PTY then the
//! parser screen in that order (ADR-008), recomputes the centred/left margin, and
//! clears the gutter before the full repaint (ADR-011). OSC 52 (slice 06) and
//! mouse (slice 07) land later, their modules declared by the slices that
//! implement them.

mod callbacks;
mod cli;
mod clipboard;
mod clock;
mod cursor;
mod geometry;
mod input;
mod keyboard;
mod mouse;
mod msg;
mod pty;
mod render;
mod rowclip;
mod terminal;
mod waiter;

// The wezterm-term comparison oracle and the equivalence gate (ADR-001) are
// test-only infrastructure — no shipping call site (the gate trips a flag flip,
// it doesn't run in the binary). Gated on `test` AND the `oracle` feature so it
// compiles for `cargo test --features oracle` but is excluded from a plain
// `--features oracle` build, leaving no dead-code surface in the binary.
#[cfg(all(test, feature = "oracle"))]
mod oracle;

use std::process;
use std::sync::mpsc::{channel, sync_channel};
use std::thread;

use clock::RealClock;
use render::Renderer;
use terminal::{CrosstermTerminal, OuterTerminal};

fn main() {
    let code = run();
    process::exit(code);
}

/// The orchestration, factored out of `main` so `main` is just the
/// `process::exit` shell. Returns the exit code to propagate.
fn run() -> i32 {
    // --- Arg parse: gutter [--width <N>] <cmd> [args...] ---
    let config = match cli::parse(std::env::args().skip(1)) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("{msg}");
            return 2;
        }
    };

    // --- Resolve the band width `W` against the real terminal ---
    // `W` is the ONE width the child is ever told about; it is sized into the
    // PTY below so the child lays out as if it owned a `W`-wide terminal. The
    // real terminal's own width is only used to position the band (the margin).
    let (real_cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    // `--width` omitted → a full-width band that tracks the terminal (100%);
    // otherwise the parsed absolute/proportional Width. One Width value, resolved
    // by the single `geometry::resolve_width` (startup) and recomputed on resize.
    let width_config = config.width.unwrap_or(geometry::Width::Percent(100));
    let width = geometry::resolve_width(width_config, real_cols);

    // --- Spawn the child in a PTY sized `W × real_rows` ---
    let spawned = match pty::spawn(&config.cmd, &config.args, width, rows) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("gutter: failed to spawn '{}': {e}", config.cmd);
            return 1;
        }
    };
    let pty::Pty {
        master,
        reader,
        child,
    } = spawned;

    let mut pty_writer = match master.take_writer() {
        Ok(w) => w,
        Err(e) => {
            eprintln!("gutter: failed to open PTY writer: {e}");
            return 1;
        }
    };
    // The master moves into the resizer (the render thread's `Event::Resize`
    // handler is the only caller of `master.resize` — ADR-008). `resize` takes
    // `&self`, so the resizer holds the master and hands out nothing else.
    let resizer = pty::MasterResizer::new(master);

    // --- The merged unbounded channel into Thread 2 (ADR-009) ---
    let (merged_tx, merged_rx) = channel::<msg::Msg>();

    // --- Thread 1: PTY reader → bounded staging → merge forwarder ---
    // The bounded sync_channel is the backpressure seam (ADR-007); the merge
    // into the unbounded channel keeps input always-admissible (ADR-009).
    let (staging_tx, staging_rx) = sync_channel::<Vec<u8>>(pty::STAGING_DEPTH);
    thread::spawn(move || pty::reader(reader, staging_tx));
    {
        let merged_tx = merged_tx.clone();
        thread::spawn(move || pty::forward_to_merge(staging_rx, merged_tx));
    }

    // --- Thread 4: waiter — the authoritative child-death signal ---
    {
        let merged_tx = merged_tx.clone();
        thread::spawn(move || waiter::run(child, merged_tx));
    }

    // --- Outer terminal setup: raw mode, kitty probe, eager mouse capture ---
    // No forced alt screen (ADR-012): the render thread mirrors the child's mode.
    let mut terminal = CrosstermTerminal::new();
    if let Err(e) = terminal.enable_raw_mode() {
        eprintln!("gutter: failed to enable raw mode: {e}");
        return 1;
    }

    // Kitty keyboard capability (ADR-003): probe AFTER raw mode (the probe does a
    // `CSI ? u` round-trip on the real terminal). If the outer terminal can
    // source kitty, push the disambiguation flags so crossterm thereafter
    // distinguishes Shift+Enter from Enter; pair the pop in teardown. The result
    // is the clamp fed to the child's kitty state — when false, the child's
    // `CSI > N u` enable is neutralised and Shift+Enter degrades predictably.
    // The probe queries the real terminal; a test harness cannot make a dumb PTY
    // answer the `CSI ? u` round-trip, so `GUTTER_FORCE_KITTY` overrides the
    // result (`1` → forced true for case A, `0` → forced false for case B). This
    // is the injectable seam the PRD's Testing Decisions call for — the
    // `outer_supports` bool is a plain value the harness can set. Absent the env
    // var, the real probe decides.
    let outer_supports_kitty = match std::env::var("GUTTER_FORCE_KITTY").ok().as_deref() {
        Some("1") => true,
        Some("0") => false,
        _ => terminal.supports_keyboard_enhancement().unwrap_or(false),
    };
    if outer_supports_kitty {
        if let Err(e) = terminal.push_keyboard_flags() {
            eprintln!("gutter: failed to push keyboard enhancement flags: {e}");
        }
    }

    // Capture the launch cursor row (ADR-013, the inline anchor) in the SAME
    // exclusive pre-input-thread window the kitty probe just used (slice 08). The
    // input thread, once spawned, owns crossterm's event source and would consume
    // the `ESC[6n` CPR reply, so the query must run while nothing else is draining
    // the terminal — the mirror operation of the kitty probe, sharing one window.
    // The row anchors grid row 0 so a plain command grows downward from where
    // gutter was launched instead of overpainting the scrollback above it.
    // `GUTTER_FORCE_ANCHOR_ROW` mirrors `GUTTER_FORCE_KITTY`: a test harness's
    // kernel PTY never answers CPR, so the tests inject the launch row directly.
    // When the query fails — a non-tty (`Err` returns at once) or the rare tty that
    // ignores the CPR request — fall back to the bottom line (`real_rows - 1`), the
    // overwhelmingly common launch point; falling back to 0 would reproduce the
    // overpaint this anchor exists to prevent. `position()` has no timeout: a real
    // tty that answers CPR (effectively all of them) returns its reply here.
    let anchor_row = match std::env::var("GUTTER_FORCE_ANCHOR_ROW").ok() {
        Some(v) => v.parse::<u16>().unwrap_or(rows.saturating_sub(1)),
        None => crossterm::cursor::position()
            .map(|(_col, row)| row)
            .unwrap_or(rows.saturating_sub(1)),
    };

    // --- Thread 3: input reader — DETACHED (un-interruptible read()) ---
    // Spawned but never joined; reaped by process::exit on teardown (ADR-010).
    //
    // ORDERING (load-bearing): this spawn MUST stay after the kitty probe above.
    // Thread 3 drains crossterm's event source, and the probe's `CSI ? u` reply
    // returns through that same source — a Thread 3 started first would consume
    // the reply, forcing the probe to its full ~2 s timeout and a permanent
    // `false` (kitty silently clamped off even on capable terminals). The span
    // from `enable_raw_mode()` down to this spawn is the exclusive
    // pre-input-thread window: the only place an outer round-trip query can read
    // its own reply uncontended.
    thread::spawn(move || input::run(merged_tx));

    // Eager outer mouse capture (ADR-005): enable ONCE here, before the alt
    // screen, so the outer terminal is already in SGR-any-motion reporting at the
    // first click after the child negotiates — the dropped-first-click race is
    // removed by construction, not by timing. `DisableMouseCapture` fires once in
    // the render thread's ordered teardown. Eager, not reactive: the outer mouse
    // state never tracks the child's; the gate (slice 07) does the narrowing.
    if let Err(e) = terminal.enable_mouse() {
        eprintln!("gutter: failed to enable mouse capture: {e}");
    }

    // No forced alt screen (ADR-012). gutter mirrors the child's screen mode: the
    // render thread enters the outer alt screen lazily, on the child's `?1049h`
    // edge, and a plain command stays on the primary screen so its output
    // survives on exit. Kitty push and eager mouse capture above both work on the
    // primary screen, so they stay.

    // --- The OSC-52 clipboard sink: a separately-opened /dev/tty (ADR-004) ---
    // Opened read-write at this ONE call site, distinct from crossterm's stdout
    // repaint sink, and handed into the render thread's callbacks. If there is no
    // controlling tty (the clipboard fd can't open), degrade to a discarding sink
    // so gutter still runs — a missing clipboard fd must not abort startup.
    let clipboard_out: Box<dyn std::io::Write + Send> = match clipboard::open_tty_read_write() {
        Ok(tty) => Box::new(tty),
        Err(e) => {
            eprintln!("gutter: /dev/tty unavailable, clipboard disabled: {e}");
            Box::new(std::io::sink())
        }
    };

    // --- Thread 2: the render loop, on the main thread ---
    let mut renderer = Renderer::new(
        width,
        rows,
        real_cols,
        config.layout,
        width_config,
        outer_supports_kitty,
        clipboard_out,
        anchor_row,
    );
    let mut clock = RealClock::new(merged_rx);
    let code = render::run(
        &mut clock,
        &mut renderer,
        &mut terminal,
        &mut pty_writer,
        &resizer,
    );

    // The render loop already ran the ordered restore before returning. Any
    // None (channel disconnected without ChildExited) is treated as success.
    code.unwrap_or(0)
}
