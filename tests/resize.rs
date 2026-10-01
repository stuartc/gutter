//! PTY-driven integration tests for resize, `--center`/`--left` and proportional
//! `--width Npct`.
//!
//! These drive the real `gutter` binary through a real PTY (via `expectrl` /
//! `ptyprocess`) and assert on what the outer terminal actually shows — never on
//! gutter internals. The outer terminal is parsed back through a second `vt100`
//! so physical-column assertions can read which column each glyph landed in.
//!
//! Two harness facts (shared with `tests/pty.rs`):
//! - **Outer size** is set by the test before the wrapping shell is let through to
//!   `exec gutter`, so gutter reads the intended size at startup with no race
//!   (`spawn_sized` in `tests/common/mod.rs`). For the resize tests the outer PTY is
//!   then resized live, which sends SIGWINCH to gutter's process group; gutter's own
//!   signal thread turns that into a `Msg::Resize` (ADR-020).
//! - **The child is held until the test has seen its output.** Each child blocks on
//!   a `read` after it prints; the test waits for that output on the outer screen
//!   and only then sends Enter. Everything here is on the primary screen, which
//!   survives teardown, so blank-gutter checks run on the screen gutter leaves.
//!
//! CI runs these headlessly: a real PTY, no display, `TERM=xterm-256color`.

mod common;
use common::{
    assert_cols_blank, first_painted_col, row_text, stty_size_under, Finished, Gutter, SIZE_CHILD,
};

/// Run a child that fills the band's first row with 100 `#`s under `gutter_flags` on
/// a 120 × 40 terminal, and return what gutter left once the child's `READY` —
/// printed after the row — has been seen.
fn full_row_under(gutter_flags: &str) -> Finished {
    let child = "/bin/sh -c 'printf \"%0.s#\" $(seq 1 100); printf \"\\nREADY\"; read _'";
    let mut gutter = Gutter::spawn(120, 40, &format!("{gutter_flags} {child}"));
    gutter.wait_for("the child's READY", |s| s.contents().contains("READY"));
    gutter.send(b"\n");
    gutter.finish()
}

/// **`--center` centres the band with gutters on BOTH sides.** Outer 120,
/// `--width 100` → margin (120-100)/2 = 10. Content fills row 0; it must start at
/// physical col 10 and end before col 110, with both gutters `[0,10)` and
/// `[110,120)` blank.
#[test]
fn center_band_has_gutters_both_sides() {
    let done = full_row_under("--width 100 --center");

    let first = first_painted_col(&done.screen, 120).expect("row 0 has painted content");
    assert_eq!(first, 10, "centred band starts at physical col 10 (margin)");
    // Left gutter and right gutter both empty.
    assert_cols_blank(&done.screen, 0, 10, 40);
    assert_cols_blank(&done.screen, 110, 120, 40);
}

/// **`--left` left-aligns: gutter on the RIGHT only.** Same content; content
/// starts at col 0, the right gutter `[100,120)` is blank.
#[test]
fn left_band_has_right_gutter_only() {
    let done = full_row_under("--width 100 --left");

    let first = first_painted_col(&done.screen, 120).expect("row 0 has painted content");
    assert_eq!(first, 0, "left-aligned band starts at physical col 0");
    assert_cols_blank(&done.screen, 100, 120, 40);
}

/// **`--width 50pct` renders a band ~50% of the terminal at launch.** Outer 200;
/// 50% → W = 100. The child sees `COLUMNS == 100` (asserted from inside via
/// `stty size`).
#[test]
fn proportional_width_pct_at_launch() {
    let done = stty_size_under(200, 40, "--width 50pct");
    assert_eq!(
        row_text(&done.screen, 0).trim(),
        "40 100",
        "child must see 50% of 200 = 100 columns"
    );
}

/// **`--width 50%` alias** behaves identically to `50pct`.
#[test]
fn proportional_width_percent_alias_at_launch() {
    let done = stty_size_under(160, 40, "--width 50%");
    assert_eq!(
        row_text(&done.screen, 0).trim(),
        "40 80",
        "child must see 50% of 160 = 80 columns"
    );
}

/// **An absolute width stays fixed across a resize, and the band moves with its
/// content intact.** Wrap a plain child that prints its `stty size`, resize the
/// outer terminal once wider, and assert the child's line is repainted at the new
/// margin with both gutters clean. An absolute `--width 100` keeps `W = 100`, so the
/// child's PTY size is unchanged and the child does not re-report: the line it
/// already printed has to be preserved across the resize, NOT erased.
///
/// This is also the primary-mode resize check on a real PTY (ADR-012): the child is
/// a plain command (no alt screen), so the band is repainted in place on the
/// primary screen rather than blanked.
#[test]
fn resize_once_absolute_width_stays_fixed() {
    let mut gutter = Gutter::spawn(
        120,
        40,
        "--width 100 --center /bin/sh -c 'stty size; read _'",
    );
    gutter.wait_for("the child's size at margin 10", |s| {
        row_text(s, 0) == format!("{:10}40 100", "")
    });

    // Wider: 120 → 160, so the margin goes from 10 to 30. The child prints nothing
    // on this resize, so gutter's repaint at the new margin — the old copy at column
    // 10 gone with it — is the only sign it was handled.
    gutter.resize(160, 40);
    gutter.wait_for("the child's size repainted at margin 30", |s| {
        row_text(s, 0) == format!("{:30}40 100", "")
    });

    gutter.send(b"\n");
    let done = gutter.finish();
    assert_eq!(
        row_text(&done.screen, 0),
        format!("{:30}40 100", ""),
        "absolute --width 100 keeps the child at 100 columns, its line preserved at the new margin"
    );
    assert_cols_blank(&done.screen, 0, 30, 40);
    assert_cols_blank(&done.screen, 130, 160, 40);
}

/// **Proportional band tracks on resize.** With `--width 50pct`, resizing the
/// outer terminal from 200 to 160 must recompute the band: the child's reported
/// `COLUMNS` tracks from 100 to 80. Proves `W` is recomputed and the child PTY
/// is resized to the new `W` (ADR-011), not frozen at the launch value.
#[test]
fn proportional_band_tracks_on_resize() {
    let mut gutter = Gutter::spawn(200, 40, &format!("--width 50pct {SIZE_CHILD}"));
    gutter.wait_for("the child's launch size, 50% of 200 = 100 columns", |s| {
        row_text(s, 0).trim() == "40 100"
    });

    // Resize narrower: 200 → 160. 50% → 80.
    gutter.resize(160, 40);
    gutter.wait_for(
        "the child's own report of 50% of 160 = 80 columns — the proportional band must track",
        |s| s.rows(0, 160).any(|r| r.trim() == "40 80"),
    );

    gutter.send(b"\n");
    assert_eq!(gutter.finish().code, Some(0));
}
