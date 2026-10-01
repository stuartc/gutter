//! Shared harness for the PTY integration suites: run gutter under a real outer PTY
//! and wait on what that terminal shows — see [`Gutter`].
//!
//! Each test binary compiles its own copy of this file and uses a subset of it, so
//! unused helpers are expected rather than dead.
#![allow(dead_code)]

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::process::Command;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use expectrl::session::OsSession;

/// Spawn the command `build` makes on a fresh PTY.
///
/// macOS can fail a `/dev/ptmx` open that races another thread's (errno -6, which
/// surfaces as `UnknownErrno`), so a failed spawn is tried again. A real failure
/// fails every time and is reported.
fn spawn_session(build: impl Fn() -> Command) -> OsSession {
    let mut failures = 0;
    loop {
        match OsSession::spawn(build()) {
            Ok(session) => return session,
            Err(_) if failures < 20 => failures += 1,
            Err(e) => panic!("spawn gutter under PTY: {e:?}"),
        }
    }
}

/// Run `script` under `/bin/sh` on a real outer PTY that is `cols × rows` before the
/// script's first command runs.
///
/// `ptyprocess` sets the master to 80 × 24 from the parent after the fork, so a
/// `stty` inside the child races it and can lose. The size is set here instead, once
/// `spawn` has returned, and the shell is held on a `read` until that is done.
fn spawn_sized(cols: u16, rows: u16, script: &str) -> OsSession {
    let mut session = spawn_session(|| {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(format!("read _; {script}"));
        cmd
    });
    session
        .get_process_mut()
        .set_window_size(cols, rows)
        .expect("set outer PTY window size");
    session.write_all(b"\n").expect("release the sized shell");
    session.flush().expect("release the sized shell");
    session
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

/// Whether `tag` is anywhere on `screen` at any of the scrollback `offsets` (in rows
/// back from the visible window). Leaves the view at offset 0.
pub fn recoverable_from_scrollback(
    screen: &mut vt100::Screen,
    cols: u16,
    tag: &str,
    offsets: std::ops::RangeInclusive<usize>,
) -> bool {
    let found = offsets.into_iter().any(|offset| {
        screen.set_scrollback(offset);
        screen_text(screen, cols).contains(tag)
    });
    screen.set_scrollback(0);
    found
}

/// Run a child that emits `emit` (a `printf` format) and return everything the outer
/// terminal saw, through gutter's exit.
///
/// The child prints `READY` after `emit` and is held on a `read` until that is on
/// screen. gutter paints only what it has parsed, so everything it does on parsing
/// `emit` is in the stream by then, whatever the teardown race does afterwards.
pub fn outer_bytes_for(emit: &str) -> Vec<u8> {
    let mut gutter = Gutter::spawn(
        80,
        24,
        &format!("--width 40 sh -c 'printf \"{emit}READY\"; read _'"),
    );
    gutter.wait_for("the child's READY", |s| s.contents().contains("READY"));
    gutter.send(b"\n");
    gutter.finish().bytes
}

/// Run `stty size` in a child under `gutter_flags` on a `cols × rows` terminal and
/// return what gutter left, once the child's `READY` — printed after the size, on the
/// row below it — has been seen.
pub fn stty_size_under(cols: u16, rows: u16, gutter_flags: &str) -> Finished {
    let mut gutter = Gutter::spawn(
        cols,
        rows,
        &format!("{gutter_flags} /bin/sh -c 'stty size; printf READY; read _'"),
    );
    gutter.wait_for("the child's READY", |s| s.contents().contains("READY"));
    gutter.send(b"\n");
    gutter.finish()
}

/// A child that prints its `stty size` at launch and again on every SIGWINCH, and
/// exits on Enter. A trapped signal ends a `read` in some shells and not in others,
/// so the gate is a loop that only a real line gets through.
pub const SIZE_CHILD: &str =
    "/bin/sh -c 'trap \"stty size\" WINCH; stty size; until read _; do :; done'";

/// The resize-mode rails: the left one sits at `margin - 1`, the right at `band_end`.
pub const LEFT_RAIL: &str = "\u{258f}";
pub const RIGHT_RAIL: &str = "\u{2595}";

/// Leave resize mode, returning once the rails are gone. The caller has seen a rail
/// up first.
///
/// In the mode every key is swallowed, and a key typed straight after the Escape
/// would join it as Alt+<key>. The rails going shows the Escape was taken alone and
/// keys reach the child again.
pub fn leave_resize_mode(gutter: &mut Gutter) {
    gutter.send(&[0x1b]);
    // The rails are drawn across every row of the span, so any row does.
    gutter.wait_for("the rails to go", |s| {
        let row = row_text(s, 10);
        !row.contains(LEFT_RAIL) && !row.contains(RIGHT_RAIL)
    });
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

/// How long a [`Gutter`] wait may run before the test is declared failed. Only a
/// failing test ever reaches it, so it is sized for a starved machine, not for speed.
const DEADLINE: Duration = Duration::from_secs(30);

const GUTTER: &str = env!("CARGO_BIN_EXE_gutter");

const ALT_LEAVE: &[u8] = b"\x1b[?1049l";

/// Rows of scrollback the harness's own terminal keeps, so a test can scroll
/// [`Finished::screen`] back to what left the top.
const SCROLLBACK: usize = 4096;

/// A gutter running under a real outer PTY, with the outer terminal's screen kept up
/// to date as its bytes arrive.
///
/// The whole surface is a constructor family and four methods:
///
/// - [`Gutter::spawn`], [`Gutter::spawn_anchored`] — through a shell, on a terminal
///   sized before gutter starts. [`Gutter::spawn_probed`] — the same, with the startup
///   cursor probe live and answered. [`Gutter::spawn_argv`],
///   [`Gutter::spawn_argv_with`] — gutter exec'd directly on an 80 × 24 terminal.
///   [`Gutter::attach`] — a gutter the test started itself. Each returns once gutter
///   is in raw mode and reading input.
/// - [`Gutter::wait_for`] — read until a condition holds on the outer screen or in
///   the raw stream.
/// - [`Gutter::send`] — type at the outer terminal.
/// - [`Gutter::resize`] — resize the outer terminal.
/// - [`Gutter::finish`] — read until gutter exits; returns a [`Finished`].
///
/// Waiting is always reading ([`Gutter::wait_for`], [`Gutter::finish`]): there is no
/// way to pause without draining the PTY, so gutter is never left blocked in `write`.
///
/// A live gutter gives no "caught up" signal, and a wait can return with half a frame
/// read. So a check that something is *absent* belongs after `finish`, once a
/// `wait_for` has shown the trigger was handled.
///
/// A child stays alive, and paces its own output, by blocking on `read _`; the test
/// lets it through with `send(b"\n")` once it has seen what came before.
pub struct Gutter {
    master: File,
    /// Waits for gutter and returns its exit code. Owns whatever spawned gutter, so
    /// dropping an unfinished `Gutter` lets go of the process too.
    exit: Box<dyn FnOnce() -> Option<i32>>,
    output: Receiver<Vec<u8>>,
    bytes: Vec<u8>,
    /// How much of `bytes` the parser has been fed.
    fed: usize,
    parser: vt100::Parser,
    alt_screen: Option<vt100::Screen>,
}

/// What a [`Gutter::wait_for`] condition is shown: the outer screen, which it derefs
/// to, and every byte read so far — for what leaves no mark on the screen.
pub struct Outer<'a> {
    screen: &'a vt100::Screen,
    pub bytes: &'a [u8],
}

impl std::ops::Deref for Outer<'_> {
    type Target = vt100::Screen;

    fn deref(&self) -> &vt100::Screen {
        self.screen
    }
}

/// What a gutter left behind once it exited.
pub struct Finished {
    /// Every byte the outer terminal was sent.
    pub bytes: Vec<u8>,
    /// The outer terminal as gutter left it, with what scrolled off its top in its
    /// scrollback.
    pub screen: vt100::Screen,
    /// The alternate screen as it stood just before it was last left — the last
    /// frame of a child that exited inside it, which `screen` no longer shows.
    pub alt_screen: Option<vt100::Screen>,
    /// gutter's exit code, or `None` if a signal killed it.
    pub code: Option<i32>,
}

impl Gutter {
    /// Spawn gutter under a `cols × rows` outer PTY wrapping `gutter_args` (a single
    /// string, parsed by a shell), with the band anchored at `anchor_row` and after
    /// `prelude` — `;`-terminated shell commands, or `""` — has run on the outer
    /// terminal.
    pub fn spawn_anchored(
        cols: u16,
        rows: u16,
        anchor_row: u16,
        prelude: &str,
        gutter_args: &str,
    ) -> Self {
        let script = format!(
            "{prelude} exec env GUTTER_FORCE_ANCHOR_ROW={anchor_row} TERM=xterm-256color {GUTTER} {gutter_args}"
        );
        Self::from_session(spawn_sized(cols, rows, &script), cols, rows).started()
    }

    /// [`Gutter::spawn_anchored`] at row 0 with nothing seeded on the outer terminal.
    pub fn spawn(cols: u16, rows: u16, gutter_args: &str) -> Self {
        Self::spawn_anchored(cols, rows, 0, "", gutter_args)
    }

    /// Spawn gutter without `GUTTER_FORCE_ANCHOR_ROW`, so its startup cursor probe
    /// really runs, and play terminal for it: answer the `ESC[6n` query with
    /// `probed_row` (1-based), the way a real terminal would.
    ///
    /// The answer has to reach gutter within its `CPR_TIMEOUT` (100 ms). Past that
    /// gutter anchors at the bottom row and the late reply is typed at the child.
    pub fn spawn_probed(cols: u16, rows: u16, probed_row: u16, gutter_args: &str) -> Self {
        let script = format!("exec env TERM=xterm-256color {GUTTER} {gutter_args}");
        let mut gutter = Self::from_session(spawn_sized(cols, rows, &script), cols, rows);
        gutter.wait_for("gutter's cursor query (ESC[6n)", |o| {
            find(o.bytes, b"\x1b[6n").is_some()
        });
        gutter.send(format!("\x1b[{probed_row};1R").as_bytes());
        gutter.started()
    }

    /// Spawn gutter directly (no shell in between) wrapping `child_argv` under an
    /// 80 × 24 outer PTY, for a test that has to `configure` the `Command` — a working
    /// directory, an extra env var — before it runs.
    ///
    /// This form can only size the PTY once gutter is already running, whereas the
    /// shell forms hold gutter back until the size is set.
    pub fn spawn_argv_with(child_argv: &[&str], configure: impl Fn(&mut Command)) -> Self {
        let mut session = spawn_session(|| {
            let mut cmd = Command::new(GUTTER);
            cmd.args(child_argv);
            cmd.env("TERM", "xterm-256color");
            cmd.env("GUTTER_FORCE_ANCHOR_ROW", "0");
            configure(&mut cmd);
            cmd
        });
        session
            .get_process_mut()
            .set_window_size(80, 24)
            .expect("set outer PTY window size");
        Self::from_session(session, 80, 24).started()
    }

    /// [`Gutter::spawn_argv_with`] with nothing to configure.
    pub fn spawn_argv(child_argv: &[&str]) -> Self {
        Self::spawn_argv_with(child_argv, |_| {})
    }

    /// Take over a gutter the test started itself on a `cols × rows` terminal it
    /// built: `master` is that terminal's master side, in blocking mode, and `exit`
    /// waits for gutter and returns its exit code.
    ///
    /// The stream only ends once every copy of the slave side is closed, so the test
    /// must have dropped its own.
    pub fn attach(
        master: File,
        cols: u16,
        rows: u16,
        exit: impl FnOnce() -> Option<i32> + 'static,
    ) -> Self {
        Self::reading(master, cols, rows, exit).started()
    }

    fn from_session(session: OsSession, cols: u16, rows: u16) -> Self {
        let master = session
            .get_process()
            .get_raw_handle()
            .expect("duplicate the outer PTY master");
        Self::reading(master, cols, rows, move || {
            // The outer PTY only reaches its end once gutter has let go of it, so
            // this returns at once.
            match session.get_process().wait() {
                Ok(expectrl::process::unix::WaitStatus::Exited(_, code)) => Some(code),
                _ => None,
            }
        })
    }

    /// Start reading `master` into the stream and the screen.
    fn reading(
        master: File,
        cols: u16,
        rows: u16,
        exit: impl FnOnce() -> Option<i32> + 'static,
    ) -> Self {
        let mut reader = master.try_clone().expect("duplicate the outer PTY master");
        let (tx, output) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            // The end of the stream is a zero read on macOS and an EIO on Linux.
            while let Ok(n @ 1..) = reader.read(&mut buf) {
                if tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        Gutter {
            master,
            exit: Box::new(exit),
            output,
            bytes: Vec::new(),
            fed: 0,
            parser: vt100::Parser::new(rows, cols, SCROLLBACK),
            alt_screen: None,
        }
    }

    /// Return once gutter is in raw mode and reading input. Input sent any earlier
    /// meets a cooked tty: the kernel line-buffers it, and a `0x03` kills gutter.
    fn started(mut self) -> Self {
        // Autowrap-off is the last thing gutter writes before its render loop, after
        // raw mode is on and the input thread is running.
        self.wait_for("gutter to start (ESC[?7l)", |o| {
            find(o.bytes, b"\x1b[?7l").is_some()
        });
        self
    }

    /// Read until `seen` accepts the outer terminal. Panics with the screen if that
    /// takes longer than [`DEADLINE`] or gutter exits first.
    pub fn wait_for(&mut self, what: &str, mut seen: impl FnMut(&Outer) -> bool) {
        let deadline = Instant::now() + DEADLINE;
        while !seen(&Outer { screen: self.parser.screen(), bytes: &self.bytes }) {
            if !self.read_chunk(deadline, what) {
                panic!("gutter exited while the test waited for {what}\n{}", self.dump());
            }
        }
    }

    /// Type `bytes` at the outer terminal.
    pub fn send(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).expect("write to the outer PTY");
    }

    /// Resize the outer terminal, and the screen the waits read with it.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: `master` is an open descriptor and `size` outlives the call.
        let rc = unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ as _, &size) };
        assert_eq!(rc, 0, "resize the outer PTY: {}", std::io::Error::last_os_error());
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
        Finished {
            code: (self.exit)(),
            bytes: self.bytes,
            screen: self.parser.screen().clone(),
            alt_screen: self.alt_screen,
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
