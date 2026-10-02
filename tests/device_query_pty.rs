//! gutter answers the child's device queries (the PTY-driven leg).
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

mod common;
use common::Gutter;

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
IFS= read -rs -d R cpr
cpr=${cpr#$'\033'}
cpr=${cpr#'['}
printf 'CPR<%s>' "$cpr"
read _"#;
    let mut gutter = Gutter::spawn_argv(&["--width", "40", "--center", "bash", "-c", script]);

    // The whole report, closing `>` included, whatever coordinates it carries.
    gutter.wait_for("the child's CPR report", |s| {
        s.contents().split_once("CPR<").is_some_and(|(_, rest)| rest.contains('>'))
    });
    gutter.send(b"\n");
    let out = gutter.finish().screen.contents();

    assert!(
        out.contains("CPR<3;7>"),
        "cursor-position reply must be the child's W-grid position (3;7), not a \
         margin-shifted column.\nouter screen: {out:?}"
    );
    assert!(
        !out.contains("CPR<3;27>"),
        "the band's left-margin offset (20) must not leak into the reply"
    );
}

/// **Prompt exit — no DA1 stall.** The child
/// emits a Primary Device Attributes query (`CSI c`) and blocks reading the reply,
/// with no timeout of its own, then prints `DONE`. Only gutter's answer can unblock
/// it: unanswered, the child never reaches `DONE` and the wait below fails.
#[test]
fn da1_query_is_answered_without_stalling_exit() {
    let script = r#"printf '\033[c'
IFS= read -rs -d c _
printf 'DONE'
read _"#;
    let mut gutter = Gutter::spawn_argv(&["bash", "-c", script]);

    gutter.wait_for("the child's DONE, which only a DA1 reply lets it print", |s| {
        s.contents().contains("DONE")
    });
    gutter.send(b"\n");
    gutter.finish();
}
