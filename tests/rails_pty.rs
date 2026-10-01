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

mod common;
use common::{
    assert_cols_blank, cell_text, leave_resize_mode, Finished, Gutter, LEFT_RAIL, RIGHT_RAIL,
};

/// The child every test here wraps: it prints nothing and holds on a `read`, so
/// Enter, once resize mode is left and keys reach the child again, ends the run.
const SILENT_CHILD: &str = "/bin/sh -c 'read _'";

/// Whether both rails are up on a mid-band row — they are drawn across every row of
/// the span, so any row does — at columns `left` and `right`.
fn rails_at(s: &vt100::Screen, left: u16, right: u16) -> bool {
    cell_text(s, 10, left) == LEFT_RAIL && cell_text(s, 10, right) == RIGHT_RAIL
}

/// Leave resize mode, end the child — Enter reaches it again once the mode is left —
/// and return what gutter left.
fn leave_mode_and_finish(mut gutter: Gutter) -> Finished {
    leave_resize_mode(&mut gutter);
    gutter.send(b"\n");
    gutter.finish()
}

/// **Entering resize mode paints the rails at the band edges and a width readout in
/// the right gutter.** A centred 60-column band in a 160-column terminal has margin
/// 50, so the left rail sits at column 49 and the right rail at `band_end` (110).
/// The readout ("60") lands right-aligned in the right gutter's bottom row.
#[test]
fn resize_mode_paints_rails_and_readout() {
    let mut gutter = Gutter::spawn(
        160,
        40,
        &format!("--width 60 --center --resize-key ctrl-o {SILENT_CHILD}"),
    );

    gutter.send(&[0x0F]); // enter (Ctrl-O)
    // Leaving the mode erases both, so the wait is the assertion.
    gutter.wait_for(
        "the left rail at margin - 1 (column 49), the right rail at band_end (column \
         110), and the readout \"60\" right-aligned in the bottom row",
        |s| rails_at(s, 49, 110) && cell_text(s, 39, 158) == "6" && cell_text(s, 39, 159) == "0",
    );

    let done = leave_mode_and_finish(gutter);
    assert_eq!(done.code, Some(0));
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
    let mut gutter = Gutter::spawn(
        160,
        40,
        &format!("--width 60 --center --resize-key ctrl-o {SILENT_CHILD}"),
    );

    gutter.send(&[0x0F]); // enter (Ctrl-O)
    gutter.wait_for("the rails at columns 49 and 110", |s| rails_at(s, 49, 110));

    gutter.send(b"llllllllllllllllllll"); // grow by 20 (width 60 -> 80)
    // New rails at the new edges: margin - 1 = 39, band_end = 120. They are only
    // there once the last of the twenty steps has been taken.
    gutter.wait_for(
        "the rails to slide to the new margin - 1 (column 39) and band_end (column 120)",
        |s| rails_at(s, 39, 120),
    );

    let done = leave_mode_and_finish(gutter);
    // The band is now [40, 120), and the child printed nothing into it. Every column
    // a rail passed through on the way out — 49 and 110 first — sits inside it, where
    // leaving the mode clears nothing, so a rail that was left behind is still here.
    assert_cols_blank(&done.screen, 40, 120, 40);
}

/// **Exiting the mode erases the rails and readout.** After `Esc`, the gutter
/// columns the rails occupied are blank again. The screen is one parser fed from
/// before the enter chord through gutter's exit — a fresh grid starts blank, so one
/// fed only the bytes after `Esc` would pass even if the exit clear were a no-op.
#[test]
fn resize_mode_exit_clears_rails() {
    let mut gutter = Gutter::spawn(
        160,
        40,
        &format!("--width 60 --center --resize-key ctrl-o {SILENT_CHILD}"),
    );

    gutter.send(&[0x0F]); // enter
    // The rails are up before Esc, so the blank gutters below were cleared rather
    // than never drawn.
    gutter.wait_for("the rails at columns 49 and 110", |s| rails_at(s, 49, 110));

    let done = leave_mode_and_finish(gutter);
    // Rails sat at columns 49 and 110; both gutters must read blank post-exit.
    assert_cols_blank(&done.screen, 0, 50, 40);
    assert_cols_blank(&done.screen, 110, 160, 40);
}
