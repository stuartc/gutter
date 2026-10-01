//! The startup cursor probe (ADR-013): gutter asks the terminal where the cursor
//! is, waits for the answer, swallows it, and anchors the band at the row it names.
//!
//! Every other PTY test sets `GUTTER_FORCE_ANCHOR_ROW` and skips the probe
//! entirely, so these are the only tests in which it runs.

mod common;

use common::Gutter;

/// The row the fake terminal reports, 1-based on the wire and 0-based in the grid.
const PROBED_ROW: u16 = 10;
const ANCHOR_ROW: usize = PROBED_ROW as usize - 1;

#[test]
fn the_cpr_reply_is_swallowed_and_never_reaches_the_child() {
    // Exposed to gutter's `CPR_TIMEOUT` (100 ms): the reply has to be written within it.
    let mut gutter =
        Gutter::spawn_probed(80, 24, PROBED_ROW, "--width 40 --left sh -c 'stty -echo; cat -v'");

    // The reply has no newline, so the child's line discipline holds it until this
    // marker arrives: whatever leaked and the marker land in `cat -v` together.
    gutter.send(b"hello\r");
    gutter.wait_for("the child's echo of the marker", |s| s.contents().contains("hello"));
    // End of input for `cat`.
    gutter.send(b"\x04");
    let text = gutter.finish().screen.contents();

    assert!(
        !text.contains("^[[10;1R"),
        "the CPR reply leaked into the child: {text:?}"
    );
}

#[test]
fn the_band_anchors_at_the_probed_row() {
    // Exposed to gutter's `CPR_TIMEOUT` (100 ms): the reply has to be written within it.
    let mut gutter = Gutter::spawn_probed(
        80,
        24,
        PROBED_ROW,
        "--width 40 --left sh -c 'printf hello; read _'",
    );
    gutter.wait_for("the child's output", |s| s.contents().contains("hello"));
    gutter.send(b"\n");
    let done = gutter.finish();

    let rows: Vec<String> = done.screen.rows(0, 80).collect();
    let painted = rows.iter().position(|r| r.contains("hello"));
    assert_eq!(
        painted,
        Some(ANCHOR_ROW),
        "the band must anchor at the probed row, not the rows-1 fallback; rows: {rows:?}"
    );
}
