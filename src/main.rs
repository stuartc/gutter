//! gutter — a one-pane terminal multiplexer that runs a command inside a
//! narrower column band, transparent to keyboard, mouse, clipboard and resize.
//!
//! The PTY is sized `W × real_rows` so the child lays out as if it owned a
//! `W`-wide terminal. Its output is parsed into a `W`-column vt100 grid on the
//! render thread and repainted to the real terminal at a left-margin column
//! offset.
//!
//! Three live threads plus a waiter feed one merged unbounded channel; no async
//! runtime. The design decisions behind all this live in `docs/adr/`.
//!
//! - Thread 1 (`pty::reader`)  — PTY byte pump, self-throttled via a bounded
//!   staging `sync_channel` (the backpressure seam, ADR-007/009).
//! - Thread 2 (`render::run`)  — owns the `vt100::Parser`, the outer terminal
//!   handle and the sole PTY-master writer; runs the coalescing loop and the
//!   offset repaint.
//! - Thread 3 (`input::run`)   — owns crossterm's event source exclusively.
//! - Thread 4 (`waiter::run`)  — on unix, loops on raw `waitpid(WUNTRACED|
//!   WCONTINUED)`, the authoritative child-state signal (exit AND stop/continue,
//!   ADR-0018).

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
mod suspend;
mod terminal;
mod waiter;

// The wezterm comparison oracle and the equivalence gate (ADR-001) are test-only;
// the gate trips a flag, it never runs in the binary. Gated on `test` AND the
// `oracle` feature so a plain `--features oracle` build excludes it, leaving no
// dead code in the binary.
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

/// Orchestration, factored out of `main` so `main` is only the `process::exit`
/// shell. Returns the exit code to propagate.
fn run() -> i32 {
    let config = match cli::parse(std::env::args().skip(1)) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("{msg}");
            return 2;
        }
    };

    // `W` is the one width the child is ever told about; it is sized into the PTY
    // below so the child lays out as if it owned a `W`-wide terminal. The real
    // terminal width only positions the band (the margin).
    let (real_cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    // `--width` omitted → a fixed 100-column band, clamped to a narrower terminal
    // by resolve_width and centred by the default Layout. `--width full` (or 100%)
    // is the escape hatch back to a terminal-tracking full-width passthrough
    // (ADR-011).
    let width_config = config.width.unwrap_or(geometry::Width::Cols(100));
    let width = geometry::resolve_width(width_config, real_cols);

    // Spawn the child in a PTY sized `W × real_rows`.
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
    // The master moves into the resizer; the render thread's resize handler is the
    // only caller of `master.resize` (ADR-008). `resize` takes `&self`, so the
    // resizer holds the master and hands out nothing else.
    let resizer = pty::MasterResizer::new(master);

    // The merged unbounded channel into Thread 2 (ADR-009).
    let (merged_tx, merged_rx) = channel::<msg::Msg>();

    // Thread 1: PTY reader → bounded staging → merge forwarder. The bounded
    // sync_channel is the backpressure seam (ADR-007); merging into the unbounded
    // channel keeps input always-admissible (ADR-009).
    let (staging_tx, staging_rx) = sync_channel::<Vec<u8>>(pty::STAGING_DEPTH);
    thread::spawn(move || pty::reader(reader, staging_tx));
    {
        let merged_tx = merged_tx.clone();
        thread::spawn(move || pty::forward_to_merge(staging_rx, merged_tx));
    }

    // Capture the child's pid BEFORE moving it into the waiter: the waiter reaps by
    // raw `waitpid` on unix (ADR-0018), and the suspender continues the child's group
    // by pid (ADR-0019). `None` → the waiter falls back to legacy `child.wait()`.
    let child_pid = child.process_id();

    // Thread 4: the waiter, the authoritative child-state signal (exit AND stop).
    {
        let merged_tx = merged_tx.clone();
        thread::spawn(move || waiter::run(child_pid, child, merged_tx));
    }

    // Outer terminal setup: raw mode, kitty probe, eager mouse capture. No forced
    // alt screen (ADR-012): the render thread mirrors the child's mode.
    let mut terminal = CrosstermTerminal::new();
    if let Err(e) = terminal.enable_raw_mode() {
        eprintln!("gutter: failed to enable raw mode: {e}");
        return 1;
    }

    // Probe kitty keyboard capability AFTER raw mode — the probe does a `CSI ? u`
    // round-trip on the real terminal (ADR-003). If the outer terminal supports
    // kitty, push the disambiguation flags (paired with a pop in teardown) so
    // crossterm distinguishes Shift+Enter from Enter; the result clamps the
    // child's kitty state.
    //
    // A test harness can't make a dumb PTY answer the round-trip, so
    // `GUTTER_FORCE_KITTY` overrides the probe: `1` forces true, `0` forces false.
    // Absent the env var, the real probe decides.
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

    // Capture the launch cursor row, the inline anchor (ADR-013), in the same
    // exclusive window the kitty probe used: once the input thread spawns it owns
    // crossterm's event source and would consume the `ESC[6n` CPR reply, so the
    // query must run while nothing else drains the terminal.
    //
    // `GUTTER_FORCE_ANCHOR_ROW` injects the row for tests, whose kernel PTY never
    // answers CPR. On a failed query, fall back to the bottom line (`rows - 1`),
    // the common launch point — NOT row 0, which would reproduce the overpaint the
    // anchor exists to prevent. `position()` has no timeout, but a real tty answers.
    let anchor_row = match std::env::var("GUTTER_FORCE_ANCHOR_ROW").ok() {
        Some(v) => v.parse::<u16>().unwrap_or(rows.saturating_sub(1)),
        None => crossterm::cursor::position()
            .map(|(_col, row)| row)
            .unwrap_or(rows.saturating_sub(1)),
    };

    // Thread 3: input reader, DETACHED. Spawned but never joined; the
    // un-interruptible `read()` is reaped by process::exit on teardown (ADR-010).
    //
    // ORDERING (load-bearing): this spawn MUST stay after the kitty probe. Thread 3
    // drains crossterm's event source, and the probe's `CSI ? u` reply returns
    // through that same source — a Thread 3 started first would consume the reply,
    // forcing the probe to its full ~2 s timeout and a permanent `false` (kitty
    // silently clamped off even on capable terminals). The span from
    // `enable_raw_mode()` to this spawn is the only window an outer round-trip
    // query can read its own reply uncontended.
    thread::spawn(move || input::run(merged_tx));

    // Eager outer mouse capture (ADR-005): enable ONCE here, before the alt
    // screen, so the outer terminal is already reporting SGR motion at the first
    // click after the child negotiates — the dropped-first-click race is removed
    // by construction. Disable fires once in the render thread's teardown.
    if let Err(e) = terminal.enable_mouse() {
        eprintln!("gutter: failed to enable mouse capture: {e}");
    }

    // The OSC-52 clipboard sink: a separately-opened /dev/tty (ADR-004), distinct
    // from crossterm's stdout repaint sink. With no controlling tty, degrade to a
    // discarding sink rather than aborting startup.
    let clipboard_out: Box<dyn std::io::Write + Send> = match clipboard::open_tty_read_write() {
        Ok(tty) => Box::new(tty),
        Err(e) => {
            eprintln!("gutter: /dev/tty unavailable, clipboard disabled: {e}");
            Box::new(std::io::sink())
        }
    };

    // Thread 2: the render loop, on the main thread.
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
    renderer.set_resize_key(config.resize_key);

    // The job-control seam for the suspend/resume cycle (ADR-0019): continues the
    // child's group by pid on resume, stops gutter's own group on suspend.
    let suspender = suspend::RealSuspender { child_pid };

    let mut clock = RealClock::new(merged_rx);
    let code = render::run(
        &mut clock,
        &mut renderer,
        &mut terminal,
        &mut pty_writer,
        &resizer,
        &suspender,
    );

    // The render loop already ran the ordered restore before returning. A None
    // (channel disconnected without ChildExited) counts as success.
    code.unwrap_or(0)
}
