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
use common::{assert_cols_blank, cell_text, leave_resize_mode, rails, Finished, Gutter};

/// The child every test here wraps: it prints nothing and holds on a `read`, so
/// Enter, once resize mode is left and keys reach the child again, ends the run.
const SILENT_CHILD: &str = "/bin/sh -c 'read _'";

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
        |s| {
            rails(s) == (Some(49), Some(110))
                && cell_text(s, 39, 158) == "6"
                && cell_text(s, 39, 159) == "0"
        },
    );

    let done = leave_mode_and_finish(gutter);
    assert_eq!(done.code, Some(0));
}

/// **A manual step slides the rails to the new edges.** A centred 60-column band in
/// a 160-column terminal has rails at 49/110; growing by 20 columns
/// (`llllllllllllllllllll`, PRD's `l` step) moves the band to `[40, 120)` and the
/// rails to 39/120, and the columns they passed through, now inside the band, end
/// up blank.
///
/// This does not pin the grow-strands-old-rails regression. A step repaints the
/// band's whole row span, which wipes a rail the step left behind, so the test
/// passes with `blank_vacated_chrome` taken out. The `src/render.rs`
/// mock/`RecordingGrid` unit test is what holds that.
#[test]
fn resize_mode_step_slides_rails() {
    let mut gutter = Gutter::spawn(
        160,
        40,
        &format!("--width 60 --center --resize-key ctrl-o {SILENT_CHILD}"),
    );

    gutter.send(&[0x0F]); // enter (Ctrl-O)
    gutter.wait_for("the rails at columns 49 and 110", |s| rails(s) == (Some(49), Some(110)));

    gutter.send(b"llllllllllllllllllll"); // grow by 20 (width 60 -> 80)
    // New rails at the new edges: margin - 1 = 39, band_end = 120. They are only
    // there once the last of the twenty steps has been taken.
    gutter.wait_for(
        "the rails to slide to the new margin - 1 (column 39) and band_end (column 120)",
        |s| rails(s) == (Some(39), Some(120)),
    );

    let done = leave_mode_and_finish(gutter);
    // The band is now [40, 120), and the child printed nothing into it, so every
    // column a rail passed through on the way out — 49 and 110 first — reads blank.
    assert_cols_blank(&done.screen, 40, 120, 40);
}

/// **Exiting the mode erases the rails and readout.** Once the mode is left, the
/// gutter columns the rails occupied are blank again — whether the `Esc` or the idle
/// exit is what left it, which this test cannot tell apart. The screen is one parser
/// fed from before the enter chord through gutter's exit — a fresh grid starts
/// blank, so one fed only the bytes after `Esc` would pass even if the exit clear
/// were a no-op.
#[test]
fn resize_mode_exit_clears_rails() {
    let mut gutter = Gutter::spawn(
        160,
        40,
        &format!("--width 60 --center --resize-key ctrl-o {SILENT_CHILD}"),
    );

    gutter.send(&[0x0F]); // enter
    // The rails are up before the mode is left, so the blank gutters below were
    // cleared rather than never drawn.
    gutter.wait_for("the rails at columns 49 and 110", |s| rails(s) == (Some(49), Some(110)));

    let done = leave_mode_and_finish(gutter);
    // Rails sat at columns 49 and 110; both gutters must read blank post-exit.
    assert_cols_blank(&done.screen, 0, 50, 40);
    assert_cols_blank(&done.screen, 110, 160, 40);
}
