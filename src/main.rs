//! gutter — a one-pane terminal multiplexer that runs a command inside a
//! narrower column band, transparent to keyboard/mouse/clipboard/resize.
//!
//! Architecture: three live threads + a waiter, one merged unbounded channel,
//! no async runtime. See `.context/design/CONTEXT.md` for the full design brief.
//!
//! - Thread 1 (`pty::reader`)   — dumb PTY byte pump, self-throttled.
//! - Thread 2 (`render::run`)    — owns the outer terminal; the only writer of
//!   the PTY master and the only thread touching outer output/teardown.
//! - Thread 3 (`input::run`)     — owns crossterm's event source exclusively.
//! - Thread 4 (`waiter::run`)    — blocks on `child.wait()`, the authoritative
//!   child-death signal.
//!
//! Slice 01 (skeleton + passthrough + teardown): the render thread does a
//! verbatim byte blit instead of a real grid repaint, the PTY is the real
//! terminal width (the offset arrives in slice 02), and keyboard re-encoding is
//! a throwaway placeholder. The vt100 parser, the 60fps coalescing loop, the
//! margin maths, OSC 52, mouse, resize and proportional width all land later;
//! their modules (`callbacks`, `clipboard`, `clock`, `mouse`, `resize`,
//! `width`) are declared by the slices that implement them.

mod cli;
mod input;
mod keyboard;
mod msg;
mod pty;
mod render;
mod terminal;
mod waiter;

use std::process;
use std::sync::mpsc::{channel, sync_channel};
use std::thread;

use terminal::{CrosstermTerminal, OuterTerminal};

fn main() {
    let code = run();
    process::exit(code);
}

/// The orchestration, factored out of `main` so `main` is just the
/// `process::exit` shell. Returns the exit code to propagate.
fn run() -> i32 {
    // --- Arg parse: gutter <cmd> [args...] ---
    let config = match cli::parse(std::env::args().skip(1)) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("{msg}");
            return 2;
        }
    };

    // --- Size the PTY to the real terminal's current cols × rows ---
    // This is the one slice where the PTY is the real width; from slice 02 the
    // `cols` argument becomes `W`. The width flows from this single place.
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));

    // --- Spawn the child in the PTY ---
    let spawned = match pty::spawn(&config.cmd, &config.args, cols, rows) {
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
    let mut stdout = std::io::stdout();
    let code = render::run(merged_rx, &mut terminal, &mut stdout, &mut pty_writer);

    // The render loop already ran the ordered restore before returning. Any
    // None (channel disconnected without ChildExited) is treated as success.
    code.unwrap_or(0)
}
