//! PTY-driven integration tests for the resize-mode rails + width readout (stream
//! C of the band-width-resize feature: PRD 0001, Feature 2's "Visual indication").
//!
//! Written against stream B's landed `--resize-key` chord handling
//! (`tests/resize_mode.rs`) — entering the mode needs a real chord dispatch, which
//! these tests drive exactly as `resize_mode.rs` does. The paint itself (rails,
//! readout, exit-clear) is independently covered by the mock/`RecordingGrid` unit
//! tests in `src/render.rs`'s `margins` module; this file only proves the real
//! escape bytes reach the real outer terminal in the right physical columns.
//!
//! These drive the real `gutter` binary through a real PTY and assert on what the
//! outer terminal actually shows — never on gutter internals.
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

/// Assert columns `[from, to)` on every row are blank.
fn assert_cols_blank(screen: &vt100::Screen, from: u16, to: u16, rows: u16) {
    for r in 0..rows {
        for c in from..to {
            if let Some(cell) = screen.cell(r, c) {
                let s = cell.contents();
                assert!(s.is_empty() || s == " ", "col {c} row {r} must be blank, found {s:?}");
            }
        }
    }
}

/// **Entering resize mode paints the rails at the band edges and a width readout in
/// the right gutter.** A centred 60-column band in a 160-column terminal has margin
/// 50, so the left rail sits at column 49 and the right rail at `band_end` (110).
/// The readout ("60") lands right-aligned in the right gutter's bottom row.
#[test]
fn resize_mode_paints_rails_and_readout() {
    let child = "/bin/sh -c 'while true; do sleep 0.2; done'";
    let cmd = gutter_in_terminal(160, 40, &format!("--width 60 --center --resize-key ctrl-o {child}"));
    let mut session = spawn(cmd);

    let _ = drain_window(&mut session, Duration::from_millis(400));

    session.write_all(&[0x0F]).unwrap(); // enter (Ctrl-O)
    session.flush().unwrap();

    let bytes = drain_window(&mut session, Duration::from_millis(500));
    let parser = outer_grid(&bytes, 160, 40);
    let screen = parser.screen();

    // A mid-band row: rails are drawn across every row of the span, so any row does.
    let mid = 10u16;
    let left_rail = screen.cell(mid, 49).map(|c| c.contents()).unwrap_or_default();
    let right_rail = screen.cell(mid, 110).map(|c| c.contents()).unwrap_or_default();
    assert_eq!(left_rail, "\u{258f}", "left rail at margin - 1 (column 49)");
    assert_eq!(right_rail, "\u{2595}", "right rail at band_end (column 110)");

    // The readout digits ("60") right-aligned in the bottom row's right gutter.
    let bottom = 39u16;
    let readout: String = (158..160)
        .map(|c| screen.cell(bottom, c).map(|cell| cell.contents()).unwrap_or_default())
        .collect();
    assert_eq!(readout, "60", "the width readout shows the current column count");

    drop(session);
}

/// **A manual step slides the rails to the new edges and clears the old ones.** A
/// centred 60-column band in a 160-column terminal has rails at 49/110; growing by
/// 20 columns (`llllllllllllllllllll`, PRD's `l` step) moves the band to
/// `[40, 120)`, so BOTH old rail columns (49, 110) land inside the new band. This
/// is the end-to-end pin for the grow-strands-old-rails regression: the
/// `src/render.rs` mock/`RecordingGrid` unit test covers the same shape without a
/// PTY, but only a real step keypress here proves the live chord-to-repaint path
/// actually clears them.
#[test]
fn resize_mode_step_slides_rails() {
    let child = "/bin/sh -c 'while true; do sleep 0.2; done'";
    let cmd = gutter_in_terminal(160, 40, &format!("--width 60 --center --resize-key ctrl-o {child}"));
    let mut session = spawn(cmd);

    let mut bytes = drain_window(&mut session, Duration::from_millis(400));

    session.write_all(&[0x0F]).unwrap(); // enter (Ctrl-O)
    session.flush().unwrap();
    bytes.extend(drain_window(&mut session, Duration::from_millis(300)));

    session.write_all(b"llllllllllllllllllll").unwrap(); // grow by 20 (width 60 -> 80)
    session.flush().unwrap();
    bytes.extend(drain_window(&mut session, Duration::from_millis(1500)));

    let parser = outer_grid(&bytes, 160, 40);
    let screen = parser.screen();
    let mid = 10u16;

    // Old rail columns (margin 50) now sit inside the new band [40, 120) and must
    // read blank, not the stale glyph.
    let old_left = screen.cell(mid, 49).map(|c| c.contents()).unwrap_or_default();
    let old_right = screen.cell(mid, 110).map(|c| c.contents()).unwrap_or_default();
    assert!(old_left.is_empty() || old_left == " ", "old left rail (col 49) must not strand: {old_left:?}");
    assert!(old_right.is_empty() || old_right == " ", "old right rail (col 110) must not strand: {old_right:?}");

    // New rails at the new edges: margin - 1 = 39, band_end = 120.
    let new_left = screen.cell(mid, 39).map(|c| c.contents()).unwrap_or_default();
    let new_right = screen.cell(mid, 120).map(|c| c.contents()).unwrap_or_default();
    assert_eq!(new_left, "\u{258f}", "left rail slides to the new margin - 1 (column 39)");
    assert_eq!(new_right, "\u{2595}", "right rail slides to the new band_end (column 120)");

    drop(session);
}

/// **Exiting the mode erases the rails and readout.** After `Esc`, the gutter
/// columns the rails occupied are blank again. The byte stream is accumulated into
/// ONE parser from before the enter chord through the exit — parsing only the
/// post-Esc bytes into a fresh (already-blank) grid would pass even if the exit
/// clear were a no-op, since a fresh vt100 grid starts blank regardless.
#[test]
fn resize_mode_exit_clears_rails() {
    let child = "/bin/sh -c 'while true; do sleep 0.2; done'";
    let cmd = gutter_in_terminal(160, 40, &format!("--width 60 --center --resize-key ctrl-o {child}"));
    let mut session = spawn(cmd);

    let mut bytes = drain_window(&mut session, Duration::from_millis(400));

    session.write_all(&[0x0F]).unwrap(); // enter
    session.flush().unwrap();
    bytes.extend(drain_window(&mut session, Duration::from_millis(300)));

    // Confirm the rails are actually up before Esc, so the post-Esc blank assertion
    // below proves they were cleared rather than never having been drawn.
    {
        let parser = outer_grid(&bytes, 160, 40);
        let screen = parser.screen();
        let left_rail = screen.cell(10, 49).map(|c| c.contents()).unwrap_or_default();
        let right_rail = screen.cell(10, 110).map(|c| c.contents()).unwrap_or_default();
        assert_eq!(left_rail, "\u{258f}", "left rail must be present before Esc");
        assert_eq!(right_rail, "\u{2595}", "right rail must be present before Esc");
    }

    session.write_all(&[0x1b]).unwrap(); // Esc: exit
    session.flush().unwrap();
    bytes.extend(drain_window(&mut session, Duration::from_millis(500)));

    let parser = outer_grid(&bytes, 160, 40);
    // Rails sat at columns 49 and 110; both gutters must read blank post-exit.
    assert_cols_blank(parser.screen(), 0, 50, 40);
    assert_cols_blank(parser.screen(), 110, 160, 40);

    drop(session);
}
