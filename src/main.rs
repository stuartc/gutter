//! gutter — a one-pane terminal multiplexer that runs a command inside a
//! narrower column band, transparent to keyboard, mouse, clipboard and resize.
//!
//! The PTY is sized `W × real_rows` so the child lays out as if it owned a
//! `W`-wide terminal. Its output is parsed into a `W`-column vt100 grid on the
//! render thread and repainted to the real terminal at a left-margin column
//! offset.
//!
//! Four live threads plus a waiter feed one merged unbounded channel; no async
//! runtime. The design decisions behind all this live in `docs/adr/`.
//!
//! - Thread 1 (`pty::reader`)  — PTY byte pump, self-throttled via a bounded
//!   staging `sync_channel` (the backpressure seam, ADR-007/009).
//! - Thread 2 (`render::run`)  — owns the `vt100::Parser`, the outer terminal
//!   handle and the sole PTY-master writer; runs the coalescing loop, the input
//!   scanner and the offset repaint.
//! - Thread 3 (`input::run`)   — owns the outer tty read fd exclusively; a dumb
//!   `read()` pump that interprets nothing (ADR-020).
//! - Thread 4 (`waiter::run`)  — on unix, loops on raw `waitpid(WUNTRACED|
//!   WCONTINUED)`, the authoritative child-state signal (exit AND stop/continue,
//!   ADR-0018).
//! - Thread 5 (`sigwinch::run`) — `SIGWINCH` → `Msg::Resize`.

mod anchor;
mod callbacks;
mod chord;
mod cli;
mod clipboard;
mod clock;
mod cursor;
mod geometry;
mod input;
mod modes;
mod mouse;
mod msg;
mod pty;
mod relay;
mod render;
mod rowclip;
mod scan;
mod sigwinch;
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

use anchor::{open_input_tty, probe_cursor_row, CPR_TIMEOUT};
use clock::RealClock;
use render::Renderer;
use terminal::{open_tty_write, CrosstermTerminal, OuterTerminal};

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

    // gutter talks to exactly one terminal: the band's sink, the keyboard and the
    // clipboard are all opens of the one device `open_tty_write` resolved, so
    // `gutter cmd > log` paints on screen and leaves the log empty. The open is also
    // the guard — no terminal resolves, no run — and it runs here, ahead of the CPR
    // probe that would otherwise stall waiting for a reply no one is going to send.
    let (tty_out, tty_path) = match open_tty_write() {
        Ok(resolved) => resolved,
        Err(e) => {
            eprintln!("gutter: no controlling terminal: {e}");
            return 1;
        }
    };
    // The tty gutter reads input from. Opened ONCE here: the CPR probe below and
    // Thread 3 must share one file description, or they would race for the reply.
    let input_tty = match open_input_tty(&tty_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("gutter: no controlling terminal: {e}");
            return 1;
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

    // Outer terminal setup: raw mode, then eager mouse capture. No forced alt
    // screen (ADR-012): the render thread mirrors the child's mode. gutter asks
    // the outer terminal for no keyboard mode of its own — that is the child's to
    // negotiate (ADR-020).
    let mut terminal = CrosstermTerminal::new(tty_out);
    if let Err(e) = terminal.enable_raw_mode() {
        eprintln!("gutter: failed to enable raw mode: {e}");
        return 1;
    }

    // Capture the launch cursor row, the inline anchor (ADR-013), before Thread 3
    // starts draining the same fd.
    //
    // `GUTTER_FORCE_ANCHOR_ROW` injects the row for tests, whose kernel PTY never
    // answers CPR; when it is set the probe is skipped entirely. On a failed query,
    // fall back to the bottom line (`rows - 1`), the common launch point — NOT row
    // 0, which would reproduce the overpaint the anchor exists to prevent.
    let (anchor_row, leftover) = match std::env::var("GUTTER_FORCE_ANCHOR_ROW").ok() {
        Some(v) => (v.parse::<u16>().unwrap_or(rows.saturating_sub(1)), Vec::new()),
        None => {
            let (row, leftover) = probe_cursor_row(terminal.writer(), &input_tty, CPR_TIMEOUT);
            (row.unwrap_or(rows.saturating_sub(1)), leftover)
        }
    };

    // Anything the probe read that was not the reply is a keystroke typed during
    // startup. Seed it into the merged channel BEFORE Thread 3 exists, so mpsc's
    // per-sender ordering guarantees the render thread scans it ahead of the first
    // byte Thread 3 reads.
    if !leftover.is_empty() {
        let _ = merged_tx.send(msg::Msg::Input(leftover));
    }

    // Thread 3: input reader, DETACHED. Spawned but never joined; the
    // un-interruptible `read()` is reaped by process::exit on teardown (ADR-010).
    {
        let merged_tx = merged_tx.clone();
        thread::spawn(move || input::run(input_tty, merged_tx));
    }

    // Thread 5: SIGWINCH → Msg::Resize. Detached like Thread 3, and the only
    // resize source there is (ADR-020).
    thread::spawn(move || sigwinch::run(merged_tx));

    // Eager outer mouse capture (ADR-005): enable ONCE here, before the alt
    // screen, so the outer terminal is already reporting SGR motion at the first
    // click after the child negotiates — the dropped-first-click race is removed
    // by construction. Disable fires once in the render thread's teardown.
    if let Err(e) = terminal.enable_mouse() {
        eprintln!("gutter: failed to enable mouse capture: {e}");
    }

    // The OSC-52 clipboard sink: a third open of the same device (ADR-004), so a
    // clipboard write and a frame repaint never share fd state. Degrades to a
    // discarding sink rather than aborting startup.
    let clipboard_out: Box<dyn std::io::Write + Send> =
        match clipboard::open_tty_read_write(&tty_path) {
            Ok(tty) => Box::new(tty),
            Err(e) => {
                eprintln!("gutter: terminal unavailable, clipboard disabled: {e}");
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

    // A None (channel disconnected without ChildExited) counts as success.
    code.unwrap_or(0)
}
