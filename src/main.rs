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
//! Slice 02 (virtual grid + offset repaint): the PTY is sized `W × real_rows`
//! so the child believes it owns a `W`-wide terminal; its output is parsed into
//! a `W`-column vt100 grid on the render thread and repainted to the real
//! terminal at a left-margin column offset (left-aligned, fixed `W`). Keyboard
//! stays the slice-01 passthrough placeholder (real re-encoding is slice 04);
//! OSC 52 (slice 06), mouse (slice 07), resize and proportional width (slice 05)
//! land later, their modules declared by the slices that implement them.

mod callbacks;
mod cli;
mod clock;
mod geometry;
mod input;
mod keyboard;
mod msg;
mod pty;
mod render;
mod terminal;
mod waiter;

#[cfg(feature = "oracle")]
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
    let width = geometry::resolve_width(config.width, real_cols);
    // Left-aligned, fixed margin 0 in this slice; the centred formula
    // (`geometry::centred_margin`) is implemented and property-tested but the
    // `--center` flag that selects it lands in slice 05.
    let left_margin = 0u16;

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

    // --- Thread 3: input reader — DETACHED (un-interruptible read()) ---
    // Spawned but never joined; reaped by process::exit on teardown (ADR-010).
    thread::spawn(move || input::run(merged_tx));

    // --- Outer terminal setup: raw mode then alt screen ---
    let mut terminal = CrosstermTerminal::new();
    if let Err(e) = terminal.enable_raw_mode() {
        eprintln!("gutter: failed to enable raw mode: {e}");
        return 1;
    }
    if let Err(e) = terminal.enter_alt_screen() {
        let _ = terminal.disable_raw_mode();
        eprintln!("gutter: failed to enter alt screen: {e}");
        return 1;
    }

    // --- Thread 2: the render loop, on the main thread ---
    let mut renderer = Renderer::new(width, rows, left_margin);
    let mut clock = RealClock::new(merged_rx);
    let code = render::run(&mut clock, &mut renderer, &mut terminal, &mut pty_writer);

    // The render loop already ran the ordered restore before returning. Any
    // None (channel disconnected without ChildExited) is treated as success.
    code.unwrap_or(0)
}
