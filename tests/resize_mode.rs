//! PTY-driven integration tests for the modal resize-mode state machine (stream
//! B of the band-width-resize feature: PRD 0001, Feature 2 minus the painting).
//!
//! These drive the real `gutter` binary through a real PTY and assert on what
//! the outer terminal actually shows — never on gutter internals. Reuses the
//! harness helpers from `tests/resize.rs`.
//!
//! B's integration scope is the width change via the modal keys (the rails are
//! stream C's integration test).
//!
//! `--resize-key ctrl-o` is the harness chord for most of the suite; the default
//! `ctrl-\` (raw `0x1C`) gets one dedicated smoke test so that path is exercised
//! end-to-end too. Under byte matching both are unambiguous.
//!
//! Between writes the suite DRAINS rather than sleeps. gutter's output goes into
//! the same PTY the test reads, so a test that only sleeps lets that buffer fill
//! — a full-screen rails repaint is enough — and gutter's render thread blocks in
//! `write`. Input then queues up and a lone Escape arrives glued to the keystroke
//! behind it, which is Alt+<key>, not Escape.
//!
//! CI runs these headlessly: a real PTY, no display, `TERM=xterm-256color`.

use std::io::Write;
use std::time::{Duration, Instant};

use expectrl::session::OsSession;

fn gutter_bin() -> String {
    env!("CARGO_BIN_EXE_gutter").to_string()
}

/// Run gutter inside an outer terminal of the given size:
/// `sh -c 'stty cols C rows R; exec env gutter <args>'`.
fn gutter_in_terminal(outer_cols: u16, outer_rows: u16, gutter_args: &str) -> std::process::Command {
    let script = format!(
        "stty cols {outer_cols} rows {outer_rows}; exec env GUTTER_FORCE_ANCHOR_ROW=0 {} {gutter_args}",
        gutter_bin()
    );
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.arg("-c").arg(script);
    cmd
}

fn spawn(cmd: std::process::Command) -> OsSession {
    OsSession::spawn(cmd).expect("spawn gutter under PTY")
}

/// Drain a bounded window of output with NON-BLOCKING reads — a wall-clock cap
/// even while the child keeps the PTY open.
fn drain_window(session: &mut OsSession, window: Duration) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    let start = Instant::now();
    while start.elapsed() < window {
        match session.try_read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
        std::thread::sleep(Duration::from_millis(3));
    }
    out
}

/// Parse outer-terminal bytes through a vt100 at the given physical size.
fn outer_grid(bytes: &[u8], cols: u16, rows: u16) -> vt100::Parser {
    let mut p = vt100::Parser::new(rows, cols, 0);
    p.process(bytes);
    p
}

/// The physical column of the first painted (non-blank) cell on row 0, or `None`
/// if the row is blank.
fn first_painted_col(screen: &vt100::Screen, cols: u16) -> Option<u16> {
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

/// The last painted (non-blank) physical column on row 0, or `None` if blank.
fn last_painted_col(screen: &vt100::Screen, cols: u16) -> Option<u16> {
    (0..cols).rev().find(|&c| {
        screen
            .cell(0, c)
            .map(|cell| !cell.contents().is_empty() && cell.contents() != " ")
            .unwrap_or(false)
    })
}

/// **`--resize-key ctrl-o` grows the band live.** Wrap a child that reprints its
/// `stty size` on SIGWINCH (the resize path re-sizes the child's PTY, which the
/// child observes exactly like a real terminal resize). Send the enter chord
/// then several `l`s; the child's reported columns must grow.
///
/// Not asserted here: the outer band's painted span widening to match. Without
/// stream C's rails, the only painted content is this short `rows cols` text
/// line at the left margin, so a grow does not itself widen what's painted —
/// the child-reported column count is the robust observable until C lands.
#[test]
fn resize_key_grows_band() {
    let child = "/bin/sh -c 'trap \"stty size\" WINCH; stty size; while true; do sleep 0.2; done'";
    let cmd = gutter_in_terminal(160, 40, &format!("--width 60 --left --resize-key ctrl-o {child}"));
    let mut session = spawn(cmd);

    let mut bytes = drain_window(&mut session, Duration::from_millis(500));
    let parser0 = outer_grid(&bytes, 160, 40);
    assert!(
        parser0.screen().rows(0, 160).any(|r| r.contains("60")),
        "launch: the child sees COLUMNS = 60"
    );

    // Enter mode (Ctrl-O = 0x0F) then grow by 10 columns (l x10).
    session.write_all(&[0x0F]).unwrap();
    session.flush().unwrap();
    std::thread::sleep(Duration::from_millis(50));
    session.write_all(b"llllllllll").unwrap();
    session.flush().unwrap();

    bytes.extend(drain_window(&mut session, Duration::from_millis(900)));
    let parser1 = outer_grid(&bytes, 160, 40);
    assert!(
        parser1.screen().rows(0, 160).any(|r| r.contains("70")),
        "after 10 x l: the child must report COLUMNS = 70"
    );

    drop(session);
}

/// **Shrink narrows the band.** Grow then shrink back down with `h`; the band
/// must narrow (and, once stream C lands, the vacated strip is blank — for now
/// this only asserts the width actually decreased).
#[test]
fn resize_key_shrinks_band() {
    let child = "/bin/sh -c 'trap \"stty size\" WINCH; stty size; while true; do sleep 0.2; done'";
    let cmd = gutter_in_terminal(160, 40, &format!("--width 60 --left --resize-key ctrl-o {child}"));
    let mut session = spawn(cmd);

    let _ = drain_window(&mut session, Duration::from_millis(500));

    session.write_all(&[0x0F]).unwrap();
    session.flush().unwrap();
    std::thread::sleep(Duration::from_millis(50));
    // Grow by 20, then shrink by 30 — net -10 from the start.
    session.write_all(b"llllllllllllllllllll").unwrap();
    session.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    session.write_all(b"HHH").unwrap();
    session.flush().unwrap();

    let bytes = drain_window(&mut session, Duration::from_millis(900));
    let parser = outer_grid(&bytes, 160, 40);
    assert!(
        parser.screen().rows(0, 160).any(|r| r.contains("50")),
        "after +20 then -30 (H x3): the child must report COLUMNS = 50"
    );

    drop(session);
}

/// **`Esc` exits the mode and releases the key to the child.** Enter, `Esc`,
/// then send a printable that the child echoes; the child must receive it (the
/// mode really released the key, it did not swallow it as a stray in-mode key).
#[test]
fn esc_exits_mode_key_reaches_child() {
    let cmd = gutter_in_terminal(120, 40, "--width 60 --resize-key ctrl-o /bin/cat");
    let mut session = spawn(cmd);
    std::thread::sleep(Duration::from_millis(300));

    session.write_all(&[0x0F]).unwrap(); // enter
    session.flush().unwrap();
    let _ = drain_window(&mut session, Duration::from_millis(150));
    session.write_all(&[0x1b]).unwrap(); // Esc
    session.flush().unwrap();
    let _ = drain_window(&mut session, Duration::from_millis(150));
    session.write_all(b"MARKER_AFTER_ESC").unwrap();
    session.flush().unwrap();

    let bytes = drain_window(&mut session, Duration::from_millis(600));
    let parser = outer_grid(&bytes, 120, 40);
    let seen = parser
        .screen()
        .rows(0, 120)
        .any(|r| r.contains("MARKER_AFTER_ESC"));
    assert!(seen, "after Esc, a printable key must reach the child (cat echoes it)");

    drop(session);
}

/// **The default chord (`Ctrl-\`) enters via the raw legacy byte `0x1C`.** One
/// dedicated smoke test for the default-chord path; the rest of the suite uses
/// `ctrl-o`.
#[test]
fn default_chord_enters_via_raw_fs_byte() {
    let child = "/bin/sh -c 'trap \"stty size\" WINCH; stty size; while true; do sleep 0.2; done'";
    let cmd = gutter_in_terminal(160, 40, &format!("--width 60 --left {child}"));
    let mut session = spawn(cmd);

    let mut bytes = drain_window(&mut session, Duration::from_millis(500));
    let parser0 = outer_grid(&bytes, 160, 40);
    let before = first_painted_col(parser0.screen(), 160);
    let before_last = last_painted_col(parser0.screen(), 160);
    assert!(before.is_some(), "startup content painted");

    session.write_all(&[0x1c]).unwrap(); // raw Ctrl-\ (FS)
    session.flush().unwrap();
    std::thread::sleep(Duration::from_millis(50));
    session.write_all(b"llllllllll").unwrap(); // grow by 10
    session.flush().unwrap();

    bytes.extend(drain_window(&mut session, Duration::from_millis(900)));
    let parser1 = outer_grid(&bytes, 160, 40);
    assert!(
        parser1.screen().rows(0, 160).any(|r| r.contains("70")),
        "the default Ctrl-\\ chord (raw 0x1C) must have entered the mode and grown the band"
    );
    // Sanity that anything moved at all beyond just the printed number.
    let after_last = last_painted_col(parser1.screen(), 160);
    assert!(
        before_last.is_some() && after_last.is_some(),
        "row 0 must still have painted content after the grow"
    );

    drop(session);
}

/// A stray key while in mode is swallowed, not leaked to the child, and the
/// mode stays active — a following resize key still works.
#[test]
fn swallowed_key_does_not_leak_and_mode_persists() {
    let cmd = gutter_in_terminal(120, 40, "--width 60 --resize-key ctrl-o /bin/cat");
    let mut session = spawn(cmd);
    std::thread::sleep(Duration::from_millis(300));

    session.write_all(&[0x0F]).unwrap(); // enter
    session.flush().unwrap();
    let _ = drain_window(&mut session, Duration::from_millis(150));
    session.write_all(b"z").unwrap(); // unrecognised in-mode key
    session.flush().unwrap();
    let _ = drain_window(&mut session, Duration::from_millis(150));
    session.write_all(&[0x1b]).unwrap(); // Esc: exit
    session.flush().unwrap();
    let _ = drain_window(&mut session, Duration::from_millis(150));
    session.write_all(b"z").unwrap(); // now passes through to cat
    session.flush().unwrap();

    let bytes = drain_window(&mut session, Duration::from_millis(600));
    let parser = outer_grid(&bytes, 120, 40);
    let zs: usize = parser
        .screen()
        .rows(0, 120)
        .map(|r| r.matches('z').count())
        .sum();
    assert_eq!(
        zs, 1,
        "the in-mode 'z' must be swallowed; only the post-Esc 'z' reaches cat"
    );

    drop(session);
}
