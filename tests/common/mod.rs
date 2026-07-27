//! Shared harness for the PTY integration suites: spawn gutter under a real outer
//! PTY, drain what it wrote, and read the result back as a grid.
//!
//! Each test binary compiles its own copy of this file and uses a subset of it, so
//! unused helpers are expected rather than dead.
#![allow(dead_code)]

use std::process::Command;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use expectrl::session::OsSession;
use expectrl::Session;

/// Serialize every PTY test in a binary — run in parallel they flake under
/// PTY/process contention (issue #1).
pub fn pty_guard() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Spawn gutter under a real `cols × rows` outer PTY wrapping `gutter_args` (a
/// single string, so the inner `sh` parses any nested quoting), with the band
/// anchored at `anchor_row` and after `prelude` — `;`-terminated shell commands, or
/// `""` — has run on the outer terminal.
pub fn spawn_gutter_anchored(
    cols: u16,
    rows: u16,
    anchor_row: u16,
    prelude: &str,
    gutter_args: &str,
) -> OsSession {
    let script = format!(
        "stty cols {cols} rows {rows}; {prelude} exec env GUTTER_FORCE_ANCHOR_ROW={anchor_row} TERM=xterm-256color {} {gutter_args}",
        env!("CARGO_BIN_EXE_gutter")
    );
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(script);
    OsSession::spawn(cmd).expect("spawn gutter under PTY")
}

/// [`spawn_gutter_anchored`] at row 0 with nothing seeded on the outer terminal.
pub fn spawn_gutter(cols: u16, rows: u16, gutter_args: &str) -> OsSession {
    spawn_gutter_anchored(cols, rows, 0, "", gutter_args)
}

/// Spawn gutter directly (no shell in between) wrapping `child_argv`, under a real
/// 80 × 24 PTY sized after the spawn. The seam for the tests that need to configure
/// the `Command` — a working directory, an extra env var — before it runs.
///
/// The sibling of [`spawn_gutter`], not a duplicate of it: this form can only size
/// the PTY once gutter is already running, whereas the shell form's `stty` lands
/// before `exec` so gutter reads the intended size at startup with no resize race.
pub fn spawn_gutter_argv_with(
    child_argv: &[&str],
    configure: impl FnOnce(&mut Command),
) -> OsSession {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gutter"));
    cmd.args(child_argv);
    cmd.env("TERM", "xterm-256color");
    cmd.env("GUTTER_FORCE_ANCHOR_ROW", "0");
    configure(&mut cmd);
    let mut session = Session::spawn(cmd).expect("spawn gutter under PTY");
    session
        .get_process_mut()
        .set_window_size(80, 24)
        .expect("set outer PTY window size");
    session.set_expect_timeout(Some(Duration::from_secs(10)));
    session
}

/// [`spawn_gutter_argv_with`] with nothing to configure.
pub fn spawn_gutter_argv(child_argv: &[&str]) -> OsSession {
    spawn_gutter_argv_with(child_argv, |_| {})
}

/// Spawn gutter under a real `cols × rows` outer PTY *without*
/// `GUTTER_FORCE_ANCHOR_ROW`, so the startup CPR probe really runs and the test has
/// to answer it — see [`answer_cpr`].
pub fn spawn_gutter_probing(cols: u16, rows: u16, gutter_args: &str) -> OsSession {
    let script = format!(
        "stty cols {cols} rows {rows}; exec env TERM=xterm-256color {} {gutter_args}",
        env!("CARGO_BIN_EXE_gutter")
    );
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(script);
    OsSession::spawn(cmd).expect("spawn gutter under PTY")
}

/// Poll the outer PTY with non-blocking reads until `done` accepts everything read
/// so far or `deadline` elapses, returning the elapsed time and every byte read.
/// Stops early on EOF. The non-blocking reads are what make the deadline a real cap
/// even while the child holds the PTY open.
pub fn poll_until(
    session: &mut OsSession,
    deadline: Duration,
    interval: Duration,
    mut done: impl FnMut(&[u8]) -> bool,
) -> (Duration, Vec<u8>) {
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    let start = Instant::now();
    while start.elapsed() < deadline {
        match session.try_read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                if done(&out) {
                    break;
                }
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
        std::thread::sleep(interval);
    }
    (start.elapsed(), out)
}

/// Play terminal for the startup probe: read the outer PTY until gutter's `ESC[6n`
/// query appears, then answer `ESC[<row>;1R` the way a real terminal would. Panics
/// if the query never arrives. Must beat `CPR_TIMEOUT`, hence the 1 ms poll.
pub fn answer_cpr(session: &mut OsSession, row_1based: u16, deadline: Duration) {
    use std::io::Write;
    let (_, seen) = poll_until(session, deadline, Duration::from_millis(1), |b| {
        find(b, b"\x1b[6n").is_some()
    });
    assert!(
        find(&seen, b"\x1b[6n").is_some(),
        "gutter never sent its CPR query (ESC[6n)"
    );
    let reply = format!("\x1b[{row_1based};1R");
    session.write_all(reply.as_bytes()).expect("answer the CPR");
    session.flush().expect("flush the CPR answer");
}

/// Drain a bounded wall-clock window, returning every byte the outer terminal saw.
pub fn drain_window(session: &mut OsSession, window: Duration) -> Vec<u8> {
    poll_until(session, window, Duration::from_millis(3), |_| false).1
}

/// Where `needle` first appears in `hay`.
pub fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// A screen's rows joined to a single string, so a test can search for content
/// regardless of which row it landed on.
pub fn screen_text(screen: &vt100::Screen, cols: u16) -> String {
    screen.rows(0, cols).collect::<Vec<_>>().join("\n")
}

/// The outer terminal's bytes parsed at its physical size and joined to a single
/// string.
pub fn grid_text(bytes: &[u8], cols: u16, rows: u16) -> String {
    let parser = outer_grid(bytes, cols, rows);
    screen_text(parser.screen(), cols)
}

/// The first non-empty row's trimmed text, or "" if the screen is blank.
pub fn first_content_row(screen: &vt100::Screen, cols: u16) -> String {
    screen
        .rows(0, cols)
        .map(|r| r.trim_end().to_string())
        .find(|r| !r.is_empty())
        .unwrap_or_default()
}

/// Whether `tag` is anywhere in the parsed stream at any of the scrollback
/// `offsets` (in rows back from the visible window). Leaves the view at offset 0.
pub fn recoverable_from_scrollback(
    parser: &mut vt100::Parser,
    cols: u16,
    tag: &str,
    offsets: std::ops::RangeInclusive<usize>,
) -> bool {
    for offset in offsets {
        parser.screen_mut().set_scrollback(offset);
        if screen_text(parser.screen(), cols).contains(tag) {
            parser.screen_mut().set_scrollback(0);
            return true;
        }
    }
    parser.screen_mut().set_scrollback(0);
    false
}

/// Run a child that emits `emit` and lingers, and return everything the outer
/// terminal saw.
pub fn outer_bytes_for(emit: &str) -> Vec<u8> {
    let mut session = spawn_gutter(80, 24, &format!("--width 40 sh -c 'printf \"{emit}\"; sleep 0.5'"));
    let out = drain_window(&mut session, Duration::from_secs(2));
    drop(session);
    out
}

/// Read the outer PTY until `marker` is seen or `deadline` elapses, returning the
/// elapsed time and everything read. The elapsed time is the no-stall signal: a
/// child blocked on an unanswered query only emits its marker once its own read
/// times out.
pub fn read_until(session: &mut OsSession, marker: &str, deadline: Duration) -> (Duration, String) {
    let (elapsed, out) = poll_until(session, deadline, Duration::from_millis(3), |b| {
        String::from_utf8_lossy(b).contains(marker)
    });
    (elapsed, String::from_utf8_lossy(&out).into_owned())
}

/// The outer terminal's bytes parsed at its physical size, so a test can inspect
/// which physical column each glyph landed in.
pub fn outer_grid(bytes: &[u8], cols: u16, rows: u16) -> vt100::Parser {
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(bytes);
    parser
}

/// The physical column of the first painted (non-blank) cell on row 0, or `None`
/// if the row is blank.
pub fn first_painted_col(screen: &vt100::Screen, cols: u16) -> Option<u16> {
    for c in 0..cols {
        if let Some(cell) = screen.cell(0, c) {
            let s = cell.contents();
            if !s.is_empty() && s != " " {
                return Some(c);
            }
        }
    }
    None
}

/// Assert columns `[from, to)` on every row are blank — no stale gutter cells.
pub fn assert_cols_blank(screen: &vt100::Screen, from: u16, to: u16, rows: u16) {
    for r in 0..rows {
        for c in from..to {
            if let Some(cell) = screen.cell(r, c) {
                let s = cell.contents();
                assert!(
                    s.is_empty() || s == " ",
                    "col {c} row {r} must be blank, found {s:?}"
                );
            }
        }
    }
}

/// Block (up to `timeout`) on the wrapped process and return its exit code, or
/// `None` if it died by signal or never exited. Polls `get_status`, a non-blocking
/// `waitpid`.
pub fn wait_exit(session: &OsSession, timeout: Duration) -> Option<i32> {
    use expectrl::process::unix::WaitStatus;
    use expectrl::process::Healthcheck;
    let proc = session.get_process();
    let start = Instant::now();
    loop {
        match proc.get_status() {
            Ok(WaitStatus::Exited(_, code)) => return Some(code),
            Ok(WaitStatus::Signaled(_, _, _)) => return None,
            _ => {}
        }
        if start.elapsed() > timeout {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
