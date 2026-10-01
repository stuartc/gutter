//! Shared harness for the PTY integration suites: spawn gutter under a real outer
//! PTY, drain what it wrote, and read the result back as a grid.
//!
//! Each test binary compiles its own copy of this file and uses a subset of it, so
//! unused helpers are expected rather than dead.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::process::Command;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
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

/// Run `script` under `/bin/sh` on a real outer PTY that is `cols × rows` before the
/// script's first command runs.
///
/// `ptyprocess` sets the master to 80 × 24 from the parent after the fork, so a
/// `stty` inside the child races it and can lose. The size is set here instead, once
/// `spawn` has returned, and the shell is held on a `read` until that is done.
fn spawn_sized(cols: u16, rows: u16, script: &str) -> OsSession {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(format!("read _; {script}"));
    let mut session = OsSession::spawn(cmd).expect("spawn gutter under PTY");
    session
        .get_process_mut()
        .set_window_size(cols, rows)
        .expect("set outer PTY window size");
    session.write_all(b"\n").expect("release the sized shell");
    session.flush().expect("release the sized shell");
    session
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
    spawn_sized(
        cols,
        rows,
        &format!(
            "{prelude} exec env GUTTER_FORCE_ANCHOR_ROW={anchor_row} TERM=xterm-256color {} {gutter_args}",
            env!("CARGO_BIN_EXE_gutter")
        ),
    )
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
/// the PTY once gutter is already running, whereas the shell form holds gutter back
/// until the size is set, so gutter reads it at startup with no resize race.
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
    spawn_sized(
        cols,
        rows,
        &format!(
            "exec env TERM=xterm-256color {} {gutter_args}",
            env!("CARGO_BIN_EXE_gutter")
        ),
    )
}

/// Poll the outer PTY with non-blocking reads until `done` accepts everything read
/// so far or `deadline` elapses, returning the elapsed time and every byte read.
/// Stops early on EOF. The non-blocking reads are what make the deadline a real cap
/// even while the child holds the PTY open.
pub fn poll_until(
    session: &mut OsSession,
    deadline: Duration,
    interval: Duration,
    done: impl FnMut(&[u8]) -> bool,
) -> (Duration, Vec<u8>) {
    poll_bytes(|buf| session.try_read(buf), deadline, interval, done)
}

/// The same poll against any non-blocking reader — a bare PTY master a test built
/// itself, not just an [`OsSession`]. A would-block is nothing-yet; `Ok(0)` is the
/// end of the stream.
pub fn poll_bytes(
    mut read: impl FnMut(&mut [u8]) -> std::io::Result<usize>,
    deadline: Duration,
    interval: Duration,
    mut done: impl FnMut(&[u8]) -> bool,
) -> (Duration, Vec<u8>) {
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    let start = Instant::now();
    while start.elapsed() < deadline {
        match read(&mut buf) {
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

/// How long a [`Gutter`] wait may run before the test is declared failed. Only a
/// failing test ever reaches it, so it is sized for a starved machine, not for speed.
const DEADLINE: Duration = Duration::from_secs(30);

const ALT_LEAVE: &[u8] = b"\x1b[?1049l";

/// A gutter running under a real outer PTY, with the outer terminal's screen kept up
/// to date as its bytes arrive.
///
/// Waiting is always reading ([`Gutter::wait_for`], [`Gutter::finish`]): there is no
/// way to pause without draining the PTY, so gutter is never left blocked in `write`.
///
/// A live gutter gives no "caught up" signal, and a wait can return with half a frame
/// read. So a check that something is *absent* belongs after `finish`, once a
/// `wait_for` has shown the trigger was handled.
pub struct Gutter {
    session: OsSession,
    output: Receiver<Vec<u8>>,
    bytes: Vec<u8>,
    /// How much of `bytes` the parser has been fed.
    fed: usize,
    parser: vt100::Parser,
    alt_screen: Option<vt100::Screen>,
}

/// What a gutter left behind once it exited.
pub struct Finished {
    /// Every byte the outer terminal was sent.
    pub bytes: Vec<u8>,
    /// The outer terminal as gutter left it.
    pub screen: vt100::Screen,
    /// The alternate screen as it stood just before it was last left — the last
    /// frame of a child that exited inside it, which `screen` no longer shows.
    pub alt_screen: Option<vt100::Screen>,
    /// gutter's exit code, or `None` if a signal killed it.
    pub code: Option<i32>,
}

impl Gutter {
    /// Spawn gutter under a `cols × rows` outer PTY wrapping `gutter_args`, anchored
    /// at row 0, and return once it is in raw mode and reading input.
    ///
    /// Input sent any earlier meets a cooked tty: the kernel line-buffers it, and a
    /// `0x03` kills gutter.
    pub fn spawn(cols: u16, rows: u16, gutter_args: &str) -> Self {
        let session = spawn_gutter(cols, rows, gutter_args);
        let mut master = session
            .get_process()
            .get_raw_handle()
            .expect("duplicate the outer PTY master");
        let (tx, output) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            // The end of the stream is a zero read on macOS and an EIO on Linux.
            while let Ok(n @ 1..) = master.read(&mut buf) {
                if tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        let mut gutter = Gutter {
            session,
            output,
            bytes: Vec::new(),
            fed: 0,
            parser: vt100::Parser::new(rows, cols, 0),
            alt_screen: None,
        };
        // Autowrap-off is the last thing gutter writes before its render loop, after
        // raw mode is on and the input thread is running.
        gutter.read_until("gutter to start (ESC[?7l)", |g| {
            find(&g.bytes, b"\x1b[?7l").is_some()
        });
        gutter
    }

    /// Read until `seen` accepts the outer screen. Panics with the screen if that
    /// takes longer than [`DEADLINE`] or gutter exits first.
    pub fn wait_for(&mut self, what: &str, mut seen: impl FnMut(&vt100::Screen) -> bool) {
        self.read_until(what, |g| seen(g.parser.screen()));
    }

    /// Type `bytes` at the outer terminal.
    pub fn send(&mut self, bytes: &[u8]) {
        self.session.write_all(bytes).expect("write to the outer PTY");
        self.session.flush().expect("flush the outer PTY");
    }

    /// Resize the outer terminal, and the screen the waits read with it.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.session
            .get_process_mut()
            .set_window_size(cols, rows)
            .expect("resize the outer PTY");
        self.parser.screen_mut().set_size(rows, cols);
    }

    /// Read until gutter exits and return everything it wrote. Panics with the screen
    /// if that takes longer than [`DEADLINE`].
    ///
    /// What the child writes as it exits is only painted if it reaches gutter within
    /// its teardown grace. A test that asserts on such output waits for it first and
    /// lets the child go afterwards, unless that race is its subject.
    pub fn finish(mut self) -> Finished {
        let deadline = Instant::now() + DEADLINE;
        while self.read_chunk(deadline, "gutter to exit") {}
        self.parser.process(&self.bytes[self.fed..]);
        // The outer PTY only reaches its end once gutter has let go of it, so this
        // returns at once.
        let code = match self.session.get_process().wait() {
            Ok(expectrl::process::unix::WaitStatus::Exited(_, code)) => Some(code),
            _ => None,
        };
        Finished {
            bytes: self.bytes,
            screen: self.parser.screen().clone(),
            alt_screen: self.alt_screen,
            code,
        }
    }

    fn read_until(&mut self, what: &str, mut done: impl FnMut(&Self) -> bool) {
        let deadline = Instant::now() + DEADLINE;
        while !done(self) {
            if !self.read_chunk(deadline, what) {
                panic!("gutter exited while the test waited for {what}\n{}", self.dump());
            }
        }
    }

    /// Take in the next chunk of output, or return `false` at the end of the stream.
    fn read_chunk(&mut self, deadline: Instant, what: &str) -> bool {
        match self
            .output
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            Ok(chunk) => {
                self.bytes.extend_from_slice(&chunk);
                self.feed();
                true
            }
            Err(RecvTimeoutError::Disconnected) => false,
            Err(RecvTimeoutError::Timeout) => {
                panic!("timed out after {DEADLINE:?} waiting for {what}\n{}", self.dump())
            }
        }
    }

    /// Feed the parser what has arrived, stopping at each alt-screen leave to keep the
    /// screen it is about to discard. A tail that could be the start of a leave waits
    /// for the next chunk.
    fn feed(&mut self) {
        loop {
            let rest = &self.bytes[self.fed..];
            if let Some(at) = find(rest, ALT_LEAVE) {
                self.parser.process(&rest[..at]);
                self.alt_screen = Some(self.parser.screen().clone());
                self.parser.process(ALT_LEAVE);
                self.fed += at + ALT_LEAVE.len();
                continue;
            }
            let held = (1..ALT_LEAVE.len())
                .rev()
                .find(|&n| rest.ends_with(&ALT_LEAVE[..n]))
                .unwrap_or(0);
            self.parser.process(&rest[..rest.len() - held]);
            self.fed = self.bytes.len() - held;
            return;
        }
    }

    fn dump(&self) -> String {
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        let grid: String = screen
            .rows(0, cols)
            .enumerate()
            .map(|(r, text)| format!("{r:>3}|{}\n", text.trim_end()))
            .collect();
        format!(
            "outer screen, {cols}x{rows}, {} screen, {} bytes read:\n{grid}",
            if screen.alternate_screen() { "alternate" } else { "primary" },
            self.bytes.len(),
        )
    }
}

/// The text of one row of `screen`, trailing blanks trimmed.
pub fn row_text(screen: &vt100::Screen, row: u16) -> String {
    let (_, cols) = screen.size();
    screen
        .rows(0, cols)
        .nth(row as usize)
        .unwrap_or_default()
        .trim_end()
        .to_string()
}

/// What one cell of `screen` holds, or "" for a blank or missing cell.
pub fn cell_text(screen: &vt100::Screen, row: u16, col: u16) -> &str {
    screen.cell(row, col).map(|c| c.contents()).unwrap_or_default()
}
