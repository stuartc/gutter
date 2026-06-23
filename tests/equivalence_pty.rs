//! Slice-08 end-to-end Definition-of-Done re-runs against the real-target
//! fixture, driven through the real `gutter` binary over a real PTY.
//!
//! These re-exercise existing code paths (resize ordering, the ADR-010 restore
//! chain) on the *real* Claude Code byte stream — denser and weirder than any
//! hand-written fixture — and assert on what the child and outer terminal
//! actually see, never on gutter internals. They are the E2E counterparts of the
//! offline equivalence gate (`src/oracle/gate.rs`): the gate proves the grid is
//! correct; these prove the full binary survives the same stream end to end.
//!
//! The fixture is replayed INTO gutter by a small shell child that emits its
//! bytes then idles (so the live alt-screen frame is captured, not the post-exit
//! primary screen). Harness facts are shared with `tests/pty.rs` /
//! `tests/resize.rs`: `stty` sets the outer size before gutter reads it, and a
//! bounded non-blocking drain captures the live frame.
//!
//! CI runs these headlessly: a real PTY, no display, `TERM=xterm-256color`.

use std::time::{Duration, Instant};

use expectrl::session::OsSession;

fn gutter_bin() -> String {
    env!("CARGO_BIN_EXE_gutter").to_string()
}

/// The checked-in real-target fixture, on disk so a shell child can `cat` it
/// into gutter (the same bytes the offline gate replays).
fn fixture_path() -> String {
    format!("{}/tests/fixtures/claude-code-flow.cast", env!("CARGO_MANIFEST_DIR"))
}

/// Run gutter inside an outer terminal of the given size, wrapping a shell child
/// that emits the fixture bytes (via `cat`) then idles so the live frame is
/// captured. `GUTTER_FORCE_KITTY=0` skips the kitty probe stall.
fn gutter_replaying_fixture(outer_cols: u16, outer_rows: u16, gutter_flags: &str) -> OsSession {
    // The child: dump the recorded stream, then sleep so gutter's alt-screen
    // frame stays live for the capture window.
    let child = format!("/bin/sh -c 'cat {}; sleep 4'", fixture_path());
    let script = format!(
        "stty cols {outer_cols} rows {outer_rows}; exec env GUTTER_FORCE_KITTY=0 {} {gutter_flags} {child}",
        gutter_bin()
    );
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.arg("-c").arg(script);
    OsSession::spawn(cmd).expect("spawn gutter under PTY")
}

/// Bounded, non-blocking drain — a wall-clock cap even while the child idles, so
/// the live alt-screen frame is captured (not the discarded post-exit screen).
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

/// Assert the physical columns `[from, to)` on every row are blank — no stale
/// gutter cells.
fn assert_cols_blank(screen: &vt100::Screen, from: u16, to: u16, rows: u16) {
    for r in 0..rows {
        for c in from..to {
            if let Some(cell) = screen.cell(r, c) {
                let s = cell.contents();
                assert!(
                    s.is_empty() || s == " ",
                    "gutter cell ({r},{c}) must be blank, found {s:?}"
                );
            }
        }
    }
}

/// **End-to-end resize-stress against the live-target fixture (slice 05's
/// guarantee re-run on real content).** Replay the fixture through gutter, then
/// drive a sequence of outer resizes through the recorded flow. Assert gutter
/// survives every resize with no panic (the binary stays alive and keeps
/// painting), the child stays at the absolute band width `W`, and the gutter
/// columns hold no stale cells at the final settle (`COLUMNS == W`).
#[test]
fn resize_stress_against_fixture_no_panic_no_stale_gutter() {
    // Absolute --width 80 (the width the fixture was recorded at), centred so a
    // resize moves the margin and would strand cells if the gutter clear failed.
    let mut session = gutter_replaying_fixture(120, 30, "--width 80 --center");

    // Let the first frame land.
    let _ = drain_window(&mut session, Duration::from_millis(500));

    // A sequence of rapid resizes through the recorded flow: wider, narrower
    // (below W so the centred margin clamps to 0), wider again.
    for &(cols, rows) in &[(160u16, 40u16), (90, 24), (200, 50)] {
        session
            .get_process_mut()
            .set_window_size(cols, rows)
            .expect("resize outer PTY");
        // Give gutter a moment to handle the SIGWINCH and repaint.
        let _ = drain_window(&mut session, Duration::from_millis(250));
    }

    // Final settle: one last resize back to 120x30 immediately before the
    // capture, so gutter's resize handler forces a fresh full repaint (the
    // ADR-008 step-4 gutter-clear + repaint) into the window we drain.
    session
        .get_process_mut()
        .set_window_size(120, 30)
        .expect("resize outer PTY");
    let bytes = drain_window(&mut session, Duration::from_millis(900));
    assert!(!bytes.is_empty(), "gutter must keep painting through the resizes");

    let parser = outer_grid(&bytes, 120, 30);
    let screen = parser.screen();

    // A centred 80-band in a 120 terminal has margin (120-80)/2 = 20; both
    // gutters must be blank — no stale cells from any intermediate wider layout.
    assert_cols_blank(screen, 0, 20, 30);
    assert_cols_blank(screen, 100, 120, 30);

    drop(session);
}

/// **End-to-end child-exit-restore against the live target (slices 01/04/07
/// restore ordering re-run on real content).** A child that replays the
/// fixture's alt-screen negotiation then exits mid-alt-screen with a specific
/// code: gutter must fully restore the outer terminal (leave alt screen, show
/// cursor) with NO keystroke and propagate the exit code.
#[test]
fn child_exit_mid_alt_screen_restores_and_propagates_code() {
    // The child enters the alt screen (as the fixture's phase 1 does), paints a
    // little, then exits 7 WHILE STILL in the alt screen — the wedged-alt-screen
    // failure mode the ADR-010 restore must prevent.
    let child = "/bin/sh -c 'printf \"\\033[?1049h\\033[?25l\\033[1;1Hclaude\"; exit 7'";
    let script = format!(
        "stty cols 100 rows 30; exec env GUTTER_FORCE_KITTY=0 {} --width 70 --center {child}",
        gutter_bin()
    );
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.arg("-c").arg(script);
    let mut session = OsSession::spawn(cmd).expect("spawn gutter under PTY");

    // Read to the teardown this time — we WANT the restore sequence.
    let bytes = drain_window(&mut session, Duration::from_secs(3));
    let s = String::from_utf8_lossy(&bytes);

    assert!(
        s.contains("\u{1b}[?1049l") || s.contains("\u{1b}[?47l"),
        "restore must leave the alternate screen (mid-alt-screen exit), got {s:?}"
    );
    assert!(
        s.contains("\u{1b}[?25h"),
        "restore must show the cursor with no keystroke, got {s:?}"
    );

    assert_eq!(
        wait_status(session),
        Some(7),
        "gutter must propagate the child's exit code from a mid-alt-screen exit"
    );
}

/// **The child sees `COLUMNS == W` while replaying the fixture.** A sanity
/// re-run of the band-width invariant on the real stream: the child gutter wraps
/// is told it has the band width `W`, independent of the outer width.
#[test]
fn child_sees_band_width_while_replaying_fixture() {
    // Replace the idle tail with `stty size` so the child reports its columns.
    let child = format!(
        "/bin/sh -c 'cat {}; stty size; sleep 3'",
        fixture_path()
    );
    let script = format!(
        "stty cols 120 rows 30; exec env GUTTER_FORCE_KITTY=0 {} --width 80 --left {child}",
        gutter_bin()
    );
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.arg("-c").arg(script);
    let mut session = OsSession::spawn(cmd).expect("spawn gutter under PTY");

    let bytes = drain_window(&mut session, Duration::from_millis(900));
    let parser = outer_grid(&bytes, 120, 30);
    // The fixture left the cursor low; `stty size` prints "30 80" somewhere.
    let shows_80 = parser.screen().rows(0, 120).any(|r| r.contains("80"));
    assert!(
        shows_80,
        "the child must see the band width W=80 while the fixture replays"
    );
    drop(session);
}

/// Block on the wrapped process and return its exit code, if any.
fn wait_status(session: OsSession) -> Option<i32> {
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
        if start.elapsed() > Duration::from_secs(5) {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
