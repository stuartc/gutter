//! PTY-driven integration tests for the resize-mode rails + width readout.
//!
//! The paint itself (rails, readout, exit-clear) is independently covered by the
//! mock/`RecordingGrid` unit tests in `src/render.rs`'s `margins` module; this file
//! only proves the real escape bytes reach the real outer terminal in the right
//! physical columns.
//!
//! These drive the real `gutter` binary through a real PTY and assert on what the
//! outer terminal actually shows — never on gutter internals.
//!
//! CI runs these headlessly: a real PTY, no display, `TERM=xterm-256color`.

use std::io::Write;
use std::time::Duration;

mod common;
use common::{assert_cols_blank, cell_text, drain_window, outer_grid, spawn_gutter, Gutter};

/// **Entering resize mode paints the rails at the band edges and a width readout in
/// the right gutter.** A centred 60-column band in a 160-column terminal has margin
/// 50, so the left rail sits at column 49 and the right rail at `band_end` (110).
/// The readout ("60") lands right-aligned in the right gutter's bottom row.
#[test]
fn resize_mode_paints_rails_and_readout() {
    let child = "/bin/sh -c 'while true; do sleep 0.2; done'";
    let mut session =
        spawn_gutter(160, 40, &format!("--width 60 --center --resize-key ctrl-o {child}"));

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
    let mut session =
        spawn_gutter(160, 40, &format!("--width 60 --center --resize-key ctrl-o {child}"));

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
/// columns the rails occupied are blank again. The screen is one parser fed from
/// before the enter chord through gutter's exit — a fresh grid starts blank, so one
/// fed only the bytes after `Esc` would pass even if the exit clear were a no-op.
#[test]
fn resize_mode_exit_clears_rails() {
    // The child prints nothing and holds on a `read`; Enter, once the mode is left
    // and keys reach the child again, ends the run.
    let child = "/bin/sh -c 'read _'";
    let mut gutter =
        Gutter::spawn(160, 40, &format!("--width 60 --center --resize-key ctrl-o {child}"));

    gutter.send(&[0x0F]); // enter
    // The rails are up before Esc, so the blank gutters below were cleared rather
    // than never drawn.
    gutter.wait_for("the rails at columns 49 and 110", |s| {
        cell_text(s, 10, 49) == "\u{258f}" && cell_text(s, 10, 110) == "\u{2595}"
    });

    gutter.send(&[0x1b]); // Esc: exit
    // In the mode Enter would be swallowed, and straight after the Esc it would join
    // it as one key. The rails going shows the Esc was taken alone.
    gutter.wait_for("the rails to go", |s| {
        cell_text(s, 10, 49).trim().is_empty() && cell_text(s, 10, 110).trim().is_empty()
    });
    gutter.send(b"\n");

    let done = gutter.finish();
    // Rails sat at columns 49 and 110; both gutters must read blank post-exit.
    assert_cols_blank(&done.screen, 0, 50, 40);
    assert_cols_blank(&done.screen, 110, 160, 40);
}
