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

mod callbacks;
mod chord;
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

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::process;
use std::sync::mpsc::{channel, sync_channel};
use std::thread;
use std::time::{Duration, Instant};

use clock::RealClock;
use render::Renderer;
use terminal::{CrosstermTerminal, OuterTerminal};

/// How long the startup CPR probe waits for the terminal's reply. Generous: a
/// real terminal answers in a millisecond or two.
const CPR_TIMEOUT: Duration = Duration::from_millis(100);

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

    // The tty gutter reads input from. Opened ONCE here: the CPR probe below and
    // Thread 3 must share one file description, or they would race for the reply.
    // A separate open from the clipboard's (ADR-004); nothing ever reads that one.
    let input_tty = open_input_tty();

    // Capture the launch cursor row, the inline anchor (ADR-013), before Thread 3
    // starts draining the same fd.
    //
    // `GUTTER_FORCE_ANCHOR_ROW` injects the row for tests, whose kernel PTY never
    // answers CPR; when it is set the probe is skipped entirely. On a failed query,
    // fall back to the bottom line (`rows - 1`), the common launch point — NOT row
    // 0, which would reproduce the overpaint the anchor exists to prevent.
    let (anchor_row, leftover) = match std::env::var("GUTTER_FORCE_ANCHOR_ROW").ok() {
        Some(v) => (v.parse::<u16>().unwrap_or(rows.saturating_sub(1)), Vec::new()),
        None => match input_tty.as_ref() {
            Some(tty) => {
                let (row, leftover) = probe_cursor_row(tty, CPR_TIMEOUT);
                (row.unwrap_or(rows.saturating_sub(1)), leftover)
            }
            None => (rows.saturating_sub(1), Vec::new()),
        },
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
    if let Some(tty) = input_tty {
        let merged_tx = merged_tx.clone();
        thread::spawn(move || input::run(tty, merged_tx));
    } else {
        eprintln!("gutter: /dev/tty unavailable, keyboard input is disabled");
    }

    // Thread 5: SIGWINCH → Msg::Resize. Detached like Thread 3; crossterm no longer
    // installs a handler, so this is the only resize source (ADR-020).
    thread::spawn(move || sigwinch::run(merged_tx));

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

/// The tty gutter reads input from: `/dev/tty` (a separate open from the
/// clipboard's, ADR-004), falling back to a `dup` of stdin.
fn open_input_tty() -> Option<File> {
    if let Ok(tty) = clipboard::open_tty_read_write() {
        return Some(tty);
    }
    // SAFETY: `dup` returns a fresh descriptor this process owns outright, so
    // handing it to `File` transfers a genuinely exclusive ownership.
    let fd = unsafe { libc::dup(0) };
    (fd >= 0).then(|| unsafe { File::from_raw_fd(fd) })
}

/// Ask the terminal where the cursor is (DSR-CPR, `ESC [ 6 n`) and read the
/// `ESC [ row ; col R` answer back off the input tty. Returns the 0-based row and
/// **everything else that was read** — bytes the user typed while gutter was
/// starting, which the caller must not drop.
///
/// Hand-rolled rather than `crossterm::cursor::position()`: that runs through
/// crossterm's event machinery, which pushes every non-reply event it meets into
/// an internal queue that nothing drains once crossterm is out of the input path
/// (ADR-020).
fn probe_cursor_row(tty: &File, timeout: Duration) -> (Option<u16>, Vec<u8>) {
    let mut out = std::io::stdout();
    if out.write_all(b"\x1b[6n").is_err() || out.flush().is_err() {
        return (None, Vec::new());
    }

    let deadline = Instant::now() + timeout;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        if let Some((start, end, row)) = find_cpr(&buf) {
            let mut leftover = buf[..start].to_vec();
            leftover.extend_from_slice(&buf[end..]);
            return (Some(row), leftover);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || !wait_readable(tty, remaining) {
            break;
        }
        let mut chunk = [0u8; 256];
        match (&mut &*tty).read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    // No reply inside the window: the whole buffer is the user's.
    (None, buf)
}

/// Whether `tty` has readable bytes within `timeout`.
fn wait_readable(tty: &File, timeout: Duration) -> bool {
    let mut pfd = libc::pollfd {
        fd: tty.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: one initialised pollfd, described honestly by the count.
    let n = unsafe { libc::poll(&mut pfd, 1, ms) };
    n > 0 && pfd.revents & libc::POLLIN != 0
}

/// Locate a `ESC [ <row> ; <col> R` reply: its byte span and the 0-based row.
fn find_cpr(buf: &[u8]) -> Option<(usize, usize, u16)> {
    for start in 0..buf.len().saturating_sub(1) {
        if buf[start] != 0x1b || buf[start + 1] != b'[' {
            continue;
        }
        let digits_start = start + 2;
        let mut i = digits_start;
        while i < buf.len() && buf[i].is_ascii_digit() {
            i += 1;
        }
        if i == digits_start || i >= buf.len() || buf[i] != b';' {
            continue;
        }
        let row_end = i;
        i += 1;
        let col_start = i;
        while i < buf.len() && buf[i].is_ascii_digit() {
            i += 1;
        }
        if i == col_start || i >= buf.len() || buf[i] != b'R' {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&buf[digits_start..row_end]) else {
            continue;
        };
        let Ok(row) = text.parse::<u32>() else {
            continue;
        };
        // The reply is 1-based; the grid is 0-based.
        let row0 = row.saturating_sub(1).min(u16::MAX as u32) as u16;
        return Some((start, i + 1, row0));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::find_cpr;

    #[test]
    fn cpr_reply_is_located_and_the_row_is_zero_based() {
        assert_eq!(find_cpr(b"\x1b[24;1R"), Some((0, 7, 23)));
        assert_eq!(find_cpr(b"\x1b[1;1R"), Some((0, 6, 0)));
    }

    #[test]
    fn keystrokes_around_the_reply_are_leftover() {
        let buf = b"ab\x1b[7;3Rcd";
        let (start, end, row) = find_cpr(buf).unwrap();
        assert_eq!(row, 6);
        let mut leftover = buf[..start].to_vec();
        leftover.extend_from_slice(&buf[end..]);
        assert_eq!(leftover, b"abcd".to_vec());
    }

    #[test]
    fn a_partial_or_absent_reply_is_not_matched() {
        assert_eq!(find_cpr(b""), None);
        assert_eq!(find_cpr(b"\x1b[24;1"), None, "no final byte yet");
        assert_eq!(find_cpr(b"\x1b[24R"), None, "no column parameter");
        assert_eq!(find_cpr(b"hello"), None);
        // A different CSI must not be mistaken for the reply.
        assert_eq!(find_cpr(b"\x1b[15~"), None);
    }
}
