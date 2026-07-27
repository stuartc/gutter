//! The startup cursor probe (ADR-013): gutter asks the terminal where the cursor
//! is, waits for the answer, swallows it, and anchors the band at the row it names.
//!
//! Every other PTY test sets `GUTTER_FORCE_ANCHOR_ROW` and skips the probe
//! entirely, so these are the only tests in which it runs.

mod common;

use std::io::Write;
use std::time::Duration;

use common::{answer_cpr, drain_window, outer_grid, pty_guard, read_until, spawn_gutter_probing};

/// The row the fake terminal reports, 1-based on the wire and 0-based in the grid.
const PROBED_ROW: u16 = 10;
const ANCHOR_ROW: usize = PROBED_ROW as usize - 1;

#[test]
fn the_cpr_reply_is_swallowed_and_never_reaches_the_child() {
    let _guard = pty_guard();
    let mut session =
        spawn_gutter_probing(80, 24, "--width 40 --left sh -c 'stty -echo; cat -v'");
    answer_cpr(&mut session, PROBED_ROW, Duration::from_secs(5));

    // The reply has no newline, so the child's line discipline holds it until this
    // marker arrives: whatever leaked and the marker land in `cat -v` together.
    session.write_all(b"hello\r").expect("send a keystroke");
    let (_, text) = read_until(&mut session, "hello", Duration::from_secs(5));
    drop(session);

    assert!(text.contains("hello"), "the child never echoed the marker: {text:?}");
    assert!(
        !text.contains("^[[10;1R"),
        "the CPR reply leaked into the child: {text:?}"
    );
}

#[test]
fn the_band_anchors_at_the_probed_row() {
    let _guard = pty_guard();
    let mut session =
        spawn_gutter_probing(80, 24, "--width 40 --left sh -c 'printf hello; sleep 0.5'");
    answer_cpr(&mut session, PROBED_ROW, Duration::from_secs(5));
    let out = drain_window(&mut session, Duration::from_secs(2));
    drop(session);

    let parser = outer_grid(&out, 80, 24);
    let rows: Vec<String> = parser.screen().rows(0, 80).collect();
    let painted = rows.iter().position(|r| r.contains("hello"));
    assert_eq!(
        painted,
        Some(ANCHOR_ROW),
        "the band must anchor at the probed row, not the rows-1 fallback; rows: {rows:?}"
    );
}
