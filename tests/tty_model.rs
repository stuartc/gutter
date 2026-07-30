//! gutter talks to exactly one terminal. The band, the keyboard and the startup CPR
//! probe all reach it through `/dev/tty` — or, when there is no controlling terminal,
//! through the terminal stdin names. stdout is never written and a redirect never
//! captures the band, the tmux behaviour. The same open is the guard: no terminal
//! either way, no run.

mod common;

use std::ffi::{CStr, OsStr};
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use common::{
    answer_cpr, drain_window, first_painted_col, outer_grid, poll_bytes, pty_guard, spawn_gutter,
    spawn_gutter_probing,
};

/// A fresh path under the system temp dir, unique per process and tag. Not created —
/// the caller (or gutter's own redirect) makes it.
fn temp_path(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("gutter-tty-{tag}-{}", std::process::id()))
}

/// With no controlling terminal there is nothing to render a band on: gutter prints
/// one line to stderr, exits 1, and never gets as far as spawning the child.
///
/// The child is a path that cannot exist, which is what makes the ordering readable:
/// reaching the spawn produces `failed to spawn`, refusing first produces
/// `no controlling terminal`, and stderr says which happened. A marker file the child
/// would touch cannot tell them apart — the refusal closes the PTY master and SIGHUPs
/// the child long before it execs, so the marker never appears either way.
///
/// `setsid` in the forked child is what removes the controlling terminal — it leaves
/// the session the test binary inherited from whoever ran `cargo test`. A fresh
/// `Command` child is never a process-group leader, so the call cannot fail.
#[test]
fn no_controlling_terminal_refuses_before_spawning_the_child() {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gutter"));
    cmd.arg("/nonexistent/gutter-never-spawns-this");
    cmd.env("TERM", "xterm-256color");
    cmd.env_remove("GUTTER_FORCE_ANCHOR_ROW");
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    // SAFETY: `setsid` is async-signal-safe and touches nothing the child allocated.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let out = cmd.output().expect("run gutter with no controlling terminal");

    assert_eq!(out.status.code(), Some(1), "refusal must exit 1");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stderr.lines().count(),
        1,
        "the refusal must be one line, got {stderr:?}"
    );
    assert!(
        stderr.contains("no controlling terminal"),
        "the refusal must name the cause, got {stderr:?}"
    );
    assert!(
        out.stdout.is_empty(),
        "nothing may reach stdout — not the band, not the CPR query: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        !stderr.contains("failed to spawn"),
        "the guard must refuse before the child is spawned, got {stderr:?}"
    );
}

/// A `cols × rows` PTY pair, made here in the parent. The slave is opened here too,
/// deliberately: opening a terminal is how a session leader acquires a controlling
/// terminal, so a child that only inherits the descriptor ends up with a terminal on
/// its stdio and no controlling terminal — the shape this file's fallback test needs.
fn pty_pair(cols: u16, rows: u16) -> (File, File) {
    // SAFETY: each call is checked before its result is used; `ptsname`'s pointer is
    // copied out before anything else can overwrite its static buffer, and the master
    // fd is handed to `File` exactly once.
    unsafe {
        let master_fd = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        assert!(master_fd >= 0, "posix_openpt failed");
        assert_eq!(libc::grantpt(master_fd), 0, "grantpt failed");
        assert_eq!(libc::unlockpt(master_fd), 0, "unlockpt failed");
        let name = libc::ptsname(master_fd);
        assert!(!name.is_null(), "ptsname failed");
        let path = PathBuf::from(OsStr::from_bytes(CStr::from_ptr(name).to_bytes()));
        assert_eq!(
            libc::fcntl(master_fd, libc::F_SETFL, libc::O_NONBLOCK),
            0,
            "O_NONBLOCK failed"
        );
        let slave = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open(&path)
            .expect("open the PTY slave");
        let size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        assert_eq!(
            libc::ioctl(slave.as_raw_fd(), libc::TIOCSWINSZ as _, &size),
            0,
            "TIOCSWINSZ failed: {}",
            std::io::Error::last_os_error()
        );
        (File::from_raw_fd(master_fd), slave)
    }
}

/// A terminal on descriptors 0/1/2 that was never made the controlling terminal:
/// `setsid gutter bash`, `subprocess.Popen` with a PTY but no `start_new_session`, a
/// Go `exec` without `Setsid`+`Setctty`. `/dev/tty` does not open there, but there is
/// a perfectly usable screen on stdin — gutter names it with `ttyname(0)`, reopens it,
/// and paints, rather than refusing.
///
/// stdout goes to a **file**, deliberately: it is what makes the geometry assertion
/// bite. crossterm's own `terminal::size` falls back to an ioctl on stdout when its
/// `/dev/tty` open fails, which is exactly this route — with stdout on the same PTY it
/// would answer 120×40 and a regression that never asked the resolved device would
/// still look right. On a regular file that ioctl fails too, so 120×40 can only have
/// come from the terminal gutter resolved. 120×40 with a centred 40-column band starts
/// at column 40; the 80×24 fallback would start it at column 20.
#[test]
fn a_terminal_with_no_controlling_terminal_still_paints() {
    let _guard = pty_guard();
    let (mut master, slave) = pty_pair(120, 40);
    let log = temp_path("no-ctty-stdout");
    let _ = fs::remove_file(&log);

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gutter"));
    cmd.args([
        "--width",
        "40",
        "--center",
        "sh",
        "-c",
        "printf hi-no-ctty; sleep 0.5",
    ]);
    cmd.env("TERM", "xterm-256color");
    cmd.env("GUTTER_FORCE_ANCHOR_ROW", "0");
    cmd.stdin(Stdio::from(slave.try_clone().expect("clone the slave")));
    cmd.stdout(Stdio::from(File::create(&log).expect("create the stdout target")));
    cmd.stderr(Stdio::from(slave.try_clone().expect("clone the slave")));
    // SAFETY: `setsid` is async-signal-safe and touches nothing the child allocated.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().expect("spawn gutter on a PTY it does not control");

    let (_, out) = poll_bytes(
        |buf| master.read(buf),
        Duration::from_secs(5),
        Duration::from_millis(3),
        |b| String::from_utf8_lossy(b).contains("hi-no-ctty"),
    );
    let _ = child.kill();
    let _ = child.wait();

    let parser = outer_grid(&out, 120, 40);
    let screen = parser.screen();
    let rows: Vec<String> = screen.rows(0, 120).collect();
    assert!(
        !rows.iter().any(|r| r.contains("no controlling terminal")),
        "gutter must not refuse with a usable terminal on stdin: {rows:?}"
    );
    assert!(
        rows.iter().any(|r| r.contains("hi-no-ctty")),
        "the band must paint on the terminal stdin names: {rows:?}"
    );
    assert_eq!(
        first_painted_col(screen, 120),
        Some(40),
        "the band must be centred in that terminal's own 120 columns; rows: {rows:?}"
    );
    let captured = fs::read(&log).expect("the stdout target must exist");
    assert!(
        captured.is_empty(),
        "nothing may reach stdout on this route either, got {:?}",
        String::from_utf8_lossy(&captured)
    );

    let _ = fs::remove_file(&log);
}

/// The same terminal, but on stdout with stdin fed from `/dev/null`: a supervisor that
/// pipes gutter's input while leaving a screen on 1 and 2. `ttyname(0)` answers nothing
/// there, so a fallback that only ever asks stdin would refuse a terminal it can see.
#[test]
fn a_terminal_on_stdout_alone_still_paints() {
    let _guard = pty_guard();
    let (mut master, slave) = pty_pair(120, 40);

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gutter"));
    cmd.args([
        "--width",
        "40",
        "--center",
        "sh",
        "-c",
        "printf hi-stdout-only; sleep 0.5",
    ]);
    cmd.env("TERM", "xterm-256color");
    cmd.env("GUTTER_FORCE_ANCHOR_ROW", "0");
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::from(slave.try_clone().expect("clone the slave")));
    cmd.stderr(Stdio::from(slave.try_clone().expect("clone the slave")));
    // SAFETY: `setsid` is async-signal-safe and touches nothing the child allocated.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().expect("spawn gutter with a terminal on stdout only");

    let (_, out) = poll_bytes(
        |buf| master.read(buf),
        Duration::from_secs(5),
        Duration::from_millis(3),
        |b| String::from_utf8_lossy(b).contains("hi-stdout-only"),
    );
    let _ = child.kill();
    let _ = child.wait();

    let parser = outer_grid(&out, 120, 40);
    let screen = parser.screen();
    let rows: Vec<String> = screen.rows(0, 120).collect();
    assert!(
        !rows.iter().any(|r| r.contains("no controlling terminal")),
        "gutter must not refuse with a usable terminal on stdout: {rows:?}"
    );
    assert!(
        rows.iter().any(|r| r.contains("hi-stdout-only")),
        "the band must paint on the terminal stdout names: {rows:?}"
    );
    assert_eq!(
        first_painted_col(screen, 120),
        Some(40),
        "the band must be centred in that terminal's own 120 columns; rows: {rows:?}"
    );
}

/// `gutter cmd > log` paints on the screen and leaves the log empty: the band goes to
/// the controlling terminal, and the child's output is not teed into the redirect.
///
/// 120×40 rather than 80×24, because 80×24 is `crossterm::terminal::size`'s own
/// fallback — a geometry regression that read the size through the redirect target
/// would land on it and still look right. Centred at `--width 40` the band starts at
/// column 40 on this terminal, and at column 20 if the fallback is what answered.
#[test]
fn a_redirect_captures_nothing_and_the_band_still_paints() {
    let _guard = pty_guard();
    let log = temp_path("redirect");
    let _ = fs::remove_file(&log);

    let mut session = spawn_gutter(
        120,
        40,
        &format!(
            "--width 40 --center sh -c 'printf hi-from-the-band; sleep 0.5' > {}",
            log.display()
        ),
    );
    let out = drain_window(&mut session, Duration::from_secs(2));
    drop(session);

    let parser = outer_grid(&out, 120, 40);
    let screen = parser.screen();
    let rows: Vec<String> = screen.rows(0, 120).collect();
    assert!(
        rows.iter().any(|r| r.contains("hi-from-the-band")),
        "the band must paint on the terminal even with stdout redirected: {rows:?}"
    );
    assert_eq!(
        first_painted_col(screen, 120),
        Some(40),
        "the band must be centred in the terminal's own 120 columns; rows: {rows:?}"
    );
    let captured = fs::read(&log).expect("the redirect target must exist");
    assert!(
        captured.is_empty(),
        "the redirect must capture nothing, got {:?}",
        String::from_utf8_lossy(&captured)
    );

    let _ = fs::remove_file(&log);
}

/// The startup probe's query leaves by the terminal, not stdout — so it still gets an
/// answer under a redirect, and the band anchors where the terminal said rather than
/// falling back after a silent timeout.
#[test]
fn the_cpr_query_survives_a_redirect() {
    let _guard = pty_guard();
    let log = temp_path("probe");
    let _ = fs::remove_file(&log);
    const PROBED_ROW: u16 = 10;

    let mut session = spawn_gutter_probing(
        80,
        24,
        &format!(
            "--width 40 --left sh -c 'printf probed; sleep 0.5' > {}",
            log.display()
        ),
    );
    // Panics unless `ESC[6n` arrives on the PTY: proof the query took the terminal.
    answer_cpr(&mut session, PROBED_ROW, Duration::from_secs(5));
    let out = drain_window(&mut session, Duration::from_secs(2));
    drop(session);

    let parser = outer_grid(&out, 80, 24);
    let rows: Vec<String> = parser.screen().rows(0, 80).collect();
    assert_eq!(
        rows.iter().position(|r| r.contains("probed")),
        Some(PROBED_ROW as usize - 1),
        "the band must anchor at the probed row, not the rows-1 fallback; rows: {rows:?}"
    );
    let mut captured = Vec::new();
    fs::File::open(&log)
        .expect("the redirect target must exist")
        .read_to_end(&mut captured)
        .expect("read the redirect target");
    assert!(
        captured.is_empty(),
        "neither the query nor the band may reach the redirect, got {:?}",
        String::from_utf8_lossy(&captured)
    );

    let _ = fs::remove_file(&log);
}
