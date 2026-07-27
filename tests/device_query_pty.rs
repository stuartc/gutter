//! Slice 11 — gutter answers the child's device queries (the PTY-driven leg).
//!
//! The unit suite in `src/callbacks.rs` pins the exact reply bytes for each query
//! against the pinned vt100, with no PTY. This file layers the end-to-end check:
//! a real child emits a query through a real PTY, gutter recognises it on the
//! render thread, buffers a reply and writes it to the PTY master — the child's
//! stdin — and the child reads it back. We prove two things the offline suite
//! cannot: the reply actually reaches the child's stdin, and the cursor-position
//! reply is in the child's **W-grid** coordinates — not shifted by the band's
//! left margin — even when gutter paints at a non-zero offset on the real
//! terminal.
//!
//! Headless: a real PTY, no display, `TERM=xterm-256color`.

use std::time::Duration;

mod common;
use common::{read_until, spawn_gutter_argv as spawn_gutter};

/// **Cursor-position round-trip in W-grid coordinates (the gate).** gutter runs a
/// centred 40-column band in an 80-column terminal, so the band's left margin is
/// 20. The child positions its cursor at row 3, col 7 (1-based), emits `CSI 6 n`,
/// reads gutter's reply from its stdin and prints the reported coordinates back.
/// The reply must be `3 ; 7` — the child's position in its own 40-column grid —
/// **not** `3 ; 27`, which is what a margin-shifted (real-terminal) reply would
/// carry. This proves gutter answers, and answers in the child's coordinate space.
#[test]
fn cursor_position_reply_is_in_w_grid_coords() {
    // bash: move to (3,7), query, read the CPR reply up to its `R` delimiter,
    // strip the `ESC [` prefix and print the bare `row;col` between markers.
    let script = r#"printf '\033[3;7H\033[6n'
IFS= read -rs -d R -t 5 cpr
cpr=${cpr#$'\033'}
cpr=${cpr#'['}
printf 'CPR<%s>' "$cpr"
sleep 1"#;
    let mut session = spawn_gutter(&["--width", "40", "--center", "bash", "-c", script]);

    let (_elapsed, out) = read_until(&mut session, "CPR<", Duration::from_secs(6));

    assert!(
        out.contains("CPR<3;7>"),
        "cursor-position reply must be the child's W-grid position (3;7), not a \
         margin-shifted column.\nouter bytes (lossy): {out:?}"
    );
    assert!(
        !out.contains("CPR<3;27>"),
        "the band's left-margin offset (20) must not leak into the reply"
    );

    drop(session);
}

/// **Prompt exit — no DA1 stall (the regression this slice removes).** The child
/// emits a Primary Device Attributes query (`CSI c`) and blocks reading the reply
/// with a generous 6s timeout, then prints `DONE` and exits. With gutter
/// answering DA1 the read returns at once; without it the child waits out its full
/// timeout. Assert `DONE` appears well under the timeout — the ~2s exit stall is
/// gone.
#[test]
fn da1_query_is_answered_without_stalling_exit() {
    let script = r#"printf '\033[c'
IFS= read -rs -d c -t 6 _
printf 'DONE'"#;
    let mut session = spawn_gutter(&["bash", "-c", script]);

    let (elapsed, out) = read_until(&mut session, "DONE", Duration::from_secs(6));

    assert!(
        out.contains("DONE"),
        "the child must unblock and finish; got: {out:?}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "DA1 must be answered promptly — the child reached DONE in {elapsed:?}, \
         which means it blocked on its query (the ~2s stall this slice removes)"
    );

    drop(session);
}
