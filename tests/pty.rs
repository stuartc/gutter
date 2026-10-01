//! PTY-driven integration tests.
//!
//! These drive the real `gutter` binary through a real PTY (via `expectrl` /
//! `ptyprocess`) and assert on what the child and the outer terminal actually
//! see — never on gutter internals. The outer terminal is parsed back through a
//! second `vt100` so physical-cell assertions can read which column each glyph
//! landed in.
//!
//! Two harness facts shape the tests:
//!
//! - **Outer size.** The PTY's default is 80x24. To run gutter inside a wider
//!   terminal (so a `--width 100` band leaves real gutters) the outer PTY is sized
//!   before the wrapping shell is let through to `exec gutter` (`spawn_sized` in
//!   `tests/common/mod.rs`), so gutter reads the intended size at startup —
//!   deterministic, no resize race.
//! - **The child is held until the test has seen its output.** Each child blocks on
//!   a `read` after it prints; the test waits for that output on the outer screen
//!   and only then sends Enter. gutter paints a child's last bytes only if they
//!   reach it within its teardown grace, so output is never left to race the exit.
//!   Inline output is on the primary screen and survives teardown, so it is checked
//!   on the screen gutter leaves. What teardown undoes — the alt screen, a hidden
//!   cursor, the cursor's position — is checked by the wait itself, or on the alt
//!   screen as it stood when it was left.
//!
//! CI runs these headlessly: a real PTY, no display, `TERM=xterm-256color`.

mod common;
use common::{
    assert_cols_blank, cell_text, first_painted_col, recoverable_from_scrollback, row_text,
    screen_text, stty_size_under, Gutter,
};

/// **Child sees `COLUMNS == W`** — asserted from inside the child (`stty size`
/// prints `rows W`), not from gutter internals. gutter sizes the child PTY to
/// `W × real_rows` regardless of the outer width, so this holds independently of
/// the outer terminal size.
#[test]
fn child_sees_band_width() {
    let done = stty_size_under(120, 40, "--width 100");
    assert_eq!(
        row_text(&done.screen, 0).trim(),
        "40 100",
        "child must see W=100 columns"
    );
}

/// **Default width narrows a wide terminal.** No `--width` at all: the new
/// default is a 100-column band, so on a 120-column outer terminal the child
/// still only sees `W=100` (`stty size` reports it), not the full 120.
#[test]
fn default_width_narrows_on_wide_terminal() {
    let done = stty_size_under(120, 40, "");
    assert_eq!(
        row_text(&done.screen, 0).trim(),
        "40 100",
        "default (no --width) must narrow the child to W=100"
    );
}

/// **Default width centres the band.** Same wide outer terminal, no `--width`:
/// content wraps at the 100-column band and must start at physical column
/// `(120-100)/2 = 10`, with both gutters `[0,10)` and `[110,120)` blank.
#[test]
fn default_width_centres_on_wide_terminal() {
    let child = "/bin/sh -c 'printf \"%0.s#\" $(seq 1 200); read _'";
    let mut gutter = Gutter::spawn(120, 40, child);
    // 200 `#` fill two band rows exactly; the last lands at the band's right edge.
    gutter.wait_for("the last # at row 1, column 109", |s| cell_text(s, 1, 109) == "#");
    gutter.send(b"\n");
    let done = gutter.finish();

    let first = first_painted_col(&done.screen, 120).expect("row 0 has painted content");
    assert_eq!(first, 10, "default band must be centred: (120-100)/2 = 10");
    assert_cols_blank(&done.screen, 0, 10, 40);
    assert_cols_blank(&done.screen, 110, 120, 40);
}

/// **Default width clamps on a narrow terminal.** No `--width`, outer terminal
/// at 80 cols (narrower than the 100-col default): the existing clamp makes the
/// band full-width — no narrowing, no panic — so the child sees `W=80`.
#[test]
fn default_width_clamps_on_narrow_terminal() {
    let done = stty_size_under(80, 24, "");
    assert_eq!(
        row_text(&done.screen, 0).trim(),
        "24 80",
        "default width must clamp to the narrow terminal"
    );
}

/// **`--width full` reproduces today's transparent passthrough exactly.** The
/// child sees the full outer width, and content starts at physical column 0
/// (margin 0) — not just `W == real_cols`, but no centring offset either.
/// `--width 100%` is asserted alongside as the equivalent spelling.
#[test]
fn full_literal_is_passthrough() {
    let done = stty_size_under(120, 40, "--width full");
    // Untrimmed at the front: the size starts at column 0, with no centring offset.
    assert_eq!(
        row_text(&done.screen, 0),
        "40 120",
        "--width full must be full-width passthrough, flush at margin 0"
    );

    let done = stty_size_under(120, 40, "--width 100%");
    assert_eq!(
        row_text(&done.screen, 0),
        "40 120",
        "--width 100% must behave identically to --width full"
    );
}

/// **Band render + gutter empties.** `gutter --width 100 <cmd>` renders the
/// child inside a 100-column band; content starts at left margin 0 and the
/// gutter columns to the right (>= 100) hold no stale cells.
#[test]
fn content_in_band_gutters_empty() {
    let child = "/bin/sh -c 'printf HELLO_FROM_THE_BAND; read _'";
    let mut gutter = Gutter::spawn(120, 40, &format!("--width 100 --left {child}"));
    gutter.wait_for("the child's text", |s| s.contents().contains("HELLO_FROM_THE_BAND"));
    gutter.send(b"\n");
    let done = gutter.finish();

    let row0 = row_text(&done.screen, 0);
    assert!(
        row0.starts_with("HELLO_FROM_THE_BAND"),
        "content must start at the left margin (row 0 = {row0:?})"
    );
    assert_cols_blank(&done.screen, 100, 120, 40);
}

/// **Cursor tracking.** After the repaint the real cursor lands inside the band
/// at `(left_margin + col, row)` following the child's cursor (margin 0 in this
/// slice, so physical col == child col).
#[test]
fn cursor_tracks_child_inside_band() {
    // Print a marker on row 5, then move to row 3, col 10 (1-based CSI) and hold.
    let child = "/bin/sh -c 'printf \"\\033[5;1HMARK\\033[3;10H\"; read _'";
    let mut gutter = Gutter::spawn(120, 40, &format!("--width 100 --left {child}"));

    // CSI 3;10H is row 2, col 9 zero-based; margin 0 so physical == child. A frame
    // paints its rows and places the cursor last, and painting the marker takes the
    // cursor to row 4 — so (2, 9) with the marker up is where gutter put the cursor,
    // not where a paint happened to leave it. Teardown moves it again, so the wait is
    // the assertion.
    gutter.wait_for("the outer cursor at (2, 9), following the child into the band", |s| {
        row_text(s, 4) == "MARK" && s.cursor_position() == (2, 9)
    });

    gutter.send(b"\n");
    assert_eq!(gutter.finish().code, Some(0));
}

/// **Cursor hide/show (DECTCEM) mirrored.** The child hides its cursor; gutter
/// must mirror that on the outer terminal (the outer vt100 reports hidden).
#[test]
fn cursor_visibility_mirrored_on_outer() {
    let child = "/bin/sh -c 'printf \"\\033[?25lX\"; read _'";
    let mut gutter = Gutter::spawn(120, 40, &format!("--width 100 --left {child}"));

    // Teardown shows the cursor again, so the wait is the assertion.
    gutter.wait_for("the outer cursor hidden, mirroring the child", |s| {
        row_text(s, 0) == "X" && s.hide_cursor()
    });

    gutter.send(b"\n");
    assert_eq!(gutter.finish().code, Some(0));
}

/// **Full-screen TUI usable.** Drive vim inside the band: open it, type text,
/// and assert the edited text renders at the band's left edge with the gutter
/// still empty — proving a real full-screen app (alt screen, absolute
/// positioning, SGR) works through gutter, not just `echo`.
#[test]
fn vim_renders_inside_band() {
    let child = "/usr/bin/vim -u NONE -N -n -i NONE";
    let mut gutter = Gutter::spawn(120, 40, &format!("--width 100 --left {child}"));

    // Once the `~` column reaches the last text row vim is in the alt screen, laid
    // out and reading keys.
    gutter.wait_for("vim's ~ rows", |s| {
        s.alternate_screen() && cell_text(s, 1, 0) == "~" && cell_text(s, 38, 0) == "~"
    });

    // Insert mode, type a marker at the top-left, then leave insert mode — which
    // steps the cursor back onto the marker's last character.
    gutter.send(b"ggIGUTTERVIMOK\x1b");
    gutter.wait_for("the marker on row 0 and vim back in normal mode", |s| {
        row_text(s, 0).starts_with("GUTTERVIMOK") && s.cursor_position() == (0, 10)
    });

    // Quit without saving; the frame vim leaves is the one checked.
    gutter.send(b":q!\r");
    let done = gutter.finish();
    let screen = done.alt_screen.expect("vim leaves the alt screen on exit");

    let row0 = row_text(&screen, 0);
    assert!(
        row0.starts_with("GUTTERVIMOK"),
        "vim edit must render at the band's left margin (row 0 = {row0:?})"
    );
    // Content stays inside the band — gutter columns still empty.
    assert_cols_blank(&screen, 100, 120, 40);
    assert_eq!(done.code, Some(0), "vim exits cleanly through gutter");
}

/// **Child-exit restore + exit code (real-PTY smoke).** A child that enters the
/// alt screen and exits inside it: gutter must leave the alt screen and show the
/// cursor with no keypress after the exit, and propagate the child's exit code.
#[test]
fn child_exit_restores_terminal_and_propagates_code() {
    let child = "/bin/sh -c 'printf \"\\033[?1049hIN-ALT\"; read _; exit 7'";
    let mut gutter = Gutter::spawn(80, 24, &format!("--width 60 {child}"));
    gutter.wait_for("the child in the alt screen", |s| {
        s.alternate_screen() && s.contents().contains("IN-ALT")
    });
    gutter.send(b"\n");

    let done = gutter.finish();
    let s = String::from_utf8_lossy(&done.bytes);
    assert!(
        s.contains("\u{1b}[?1049l") || s.contains("\u{1b}[?47l"),
        "restore must leave the alternate screen, got {s:?}"
    );
    assert!(
        s.contains("\u{1b}[?25h"),
        "restore must show the cursor, got {s:?}"
    );
    assert_eq!(done.code, Some(7), "gutter must propagate the child's exit code");
}

/// **Autowrap is turned off for the run and back on at teardown (ADR-010/ADR-014).**
/// gutter disables host autowrap at setup so a byte overrunning the screen's last column
/// is clamped rather than wrapped; a shell handed back a terminal that no longer wraps
/// would be a visible regression, so the restore turns it on again — unconditionally,
/// like the mouse disable (ADR-005).
///
/// The order is asserted on the bytes: `?7h` lands before the `?25h` whose flush is what
/// puts the restore on screen (ADR-023), which is the same best-effort step list
/// `disable_raw_mode` closes. Raw mode itself is a `tcsetattr` and writes nothing, so it
/// leaves no byte to order against.
#[test]
fn teardown_restores_autowrap_before_the_restores_flush() {
    // Nothing asserted here is the child's own output, so it needs no holding.
    let child = "/bin/sh -c 'printf hi; exit 0'";
    let done = Gutter::spawn(80, 24, &format!("--width 60 {child}")).finish();
    let s = String::from_utf8_lossy(&done.bytes);

    let off = s.find("\u{1b}[?7l").expect("setup must disable autowrap");
    let on = s.rfind("\u{1b}[?7h").expect("the restore must re-enable autowrap");
    let show = s.rfind("\u{1b}[?25h").expect("the restore must show the cursor");
    assert!(off < on, "the restore's `?7h` must follow setup's `?7l`");
    assert!(
        on < show,
        "the `?7h` must be queued before `show_cursor`'s flush, got {s:?}"
    );

    assert_eq!(done.code, Some(0));
}

/// **Plain output survives to the primary screen (the E2 regression, ADR-012).**
/// A plain command that only prints to the primary screen (three lines, no
/// `?1049h`): gutter must mirror the child's mode, never force the alt screen, and
/// leave the output visible on the primary screen after exit. Asserted on the
/// post-exit stream — the inverse of the alt-screen tests: the three lines are
/// present AND `?1049h`/`?1049l` are NEVER emitted.
#[test]
fn plain_command_output_survives_to_primary_screen() {
    // No trailing newline, no alt-screen negotiation — a pure primary-screen command.
    let child = "/bin/sh -c \"printf 'line1\\nline2\\nline3'; read _\"";
    let mut gutter = Gutter::spawn(80, 24, &format!("--width 60 --left {child}"));
    gutter.wait_for("the child's third line", |s| s.contents().contains("line3"));
    gutter.send(b"\n");

    let done = gutter.finish();
    let s = String::from_utf8_lossy(&done.bytes);

    // gutter must NEVER force the alt screen for a plain command — the E2 fix.
    assert!(
        !s.contains("\u{1b}[?1049h") && !s.contains("\u{1b}[?1049l"),
        "a plain command must never emit ?1049h/?1049l, got {s:?}"
    );

    // All three lines are visible on the primary screen after exit.
    let text = screen_text(&done.screen, 80);
    for marker in ["line1", "line2", "line3"] {
        assert!(
            text.contains(marker),
            "plain output {marker:?} must survive on the primary screen, got {text:?}"
        );
    }

    assert_eq!(done.code, Some(0), "gutter propagates the zero exit");
}

/// **Mode-switch mid-run (ADR-012).** A child that prints to the PRIMARY screen
/// first, THEN enters the alt screen (`?1049h`), paints, and exits while in alt.
/// gutter must enter the outer alt screen on the child's edge — AFTER the primary
/// lines — not at startup, and restore cleanly (leave the alt screen) on exit.
#[test]
fn mode_switch_mid_run_enters_alt_after_primary_lines() {
    // Print a primary marker, then enter the alt screen and paint, then exit in
    // alt. Each `read` holds the child until the test has seen the step before it
    // on the outer screen, so gutter paints each in a frame of its own.
    let child = "/bin/sh -c \"printf 'primline'; read _; printf '\\033[?1049h\\033[1;1Halt-frame'; read _; exit 0\"";
    let mut gutter = Gutter::spawn(80, 24, &format!("--width 60 --left {child}"));

    gutter.wait_for("primline on the primary screen", |s| {
        !s.alternate_screen() && s.contents().contains("primline")
    });
    gutter.send(b"\n");
    gutter.wait_for("alt-frame on the alt screen", |s| {
        s.alternate_screen() && s.contents().contains("alt-frame")
    });
    gutter.send(b"\n");

    let done = gutter.finish();
    let s = String::from_utf8_lossy(&done.bytes);

    // The outer alt screen is entered (the child's `?1049h` edge) and later left.
    let enter = s
        .find("\u{1b}[?1049h")
        .expect("gutter must enter the outer alt screen on the child's edge");
    assert!(
        s.contains("\u{1b}[?1049l") || s.contains("\u{1b}[?47l"),
        "gutter must leave the outer alt screen on exit, got {s:?}"
    );

    // The primary marker was painted BEFORE the alt screen was entered — proving
    // the alt screen is entered lazily on the child's edge, not forced at startup.
    let prim = s
        .find("primline")
        .expect("the primary marker must be painted on the primary screen");
    assert!(
        prim < enter,
        "the primary lines must be painted before the ?1049h edge (not forced at startup)"
    );

    assert_eq!(done.code, Some(0), "gutter propagates the zero exit");
}

/// **Real-PTY smoke (cap).** Feed a few MB of scrolling output through a real
/// PTY: gutter must keep up, stay alive, drain it all, and the settled frame must
/// show the end of the flood (not a frozen early frame). The exact frame-count
/// proof is the virtual-clock unit test; this is the real-PTY sanity check that
/// the coalescing loop neither hangs nor tears.
#[test]
fn multi_mb_scroll_stays_bounded() {
    let child = "/bin/sh -c 'i=0; while [ $i -lt 20000 ]; do printf \"line %d of the flood test\\n\" $i; i=$((i+1)); done; printf FLOOD-DONE; read _'";
    let mut gutter = Gutter::spawn(120, 40, &format!("--width 100 --left {child}"));

    // A gutter that hung or fell behind for good never shows the end of the flood,
    // and the wait's deadline is what bounds it.
    gutter.wait_for("the end of the flood", |s| {
        let text = s.contents();
        text.contains("line 19999 of the flood test") && text.contains("FLOOD-DONE")
    });
    gutter.send(b"\n");

    let done = gutter.finish();
    let text = screen_text(&done.screen, 120);
    assert!(
        text.contains("line 19999 of the flood test"),
        "the settled frame must show the end of the flood, got {text:?}"
    );
    // Even under a flood the band edge holds — no bleed into the gutter.
    assert_cols_blank(&done.screen, 100, 120, 40);
    assert_eq!(done.code, Some(0));
}

/// **Scroll-off survival (ADR-013).** A plain command that
/// prints MORE than one screenful on the primary screen — 40 distinctly-tagged
/// lines on a 24-row terminal — must have its early (scrolled-off) lines reach the
/// **real terminal's own scrollback**, even though they are NOT in the visible last
/// screenful. gutter keeps vt100 at `scrollback=0` and emits each departed top line
/// into the terminal as it scrolls off, so the early tags reach it via the scroll
/// emit, not the band repaint. Not just "the last screenful survives" — every line
/// reaches scrollback.
///
/// The child prints one line and waits for Enter, and the test sends Enter once it
/// has seen that line painted — so each frame advances exactly one line. The burst
/// test below is the opposite case.
#[test]
fn scroll_off_lines_reach_real_terminal_scrollback() {
    // 40 lines, each `SCROLLTAG-NN`. No alt screen — a pure primary-screen command.
    // Echo is off so the Enters that pace it add no lines of their own.
    let child = "/bin/sh -c 'stty -echo; i=0; while [ $i -lt 40 ]; do printf \"SCROLLTAG-%02d\\n\" $i; i=$((i+1)); read _; done; exit 0'";
    let mut gutter = Gutter::spawn(80, 24, &format!("--width 60 --left {child}"));

    for i in 0..40 {
        let tag = format!("SCROLLTAG-{i:02}");
        gutter.wait_for(&tag, |s| s.contents().contains(&tag));
        gutter.send(b"\n");
    }

    let mut done = gutter.finish();
    let s = String::from_utf8_lossy(&done.bytes);

    // gutter must never force the alt screen for this plain command.
    assert!(
        !s.contains("\u{1b}[?1049h") && !s.contains("\u{1b}[?1049l"),
        "a plain scrolling command must never emit ?1049h/?1049l"
    );

    // The final visible frame (offset 0) holds the LAST screenful — the early
    // lines must NOT be visible there (they scrolled off).
    let visible_text = screen_text(&done.screen, 80);
    assert!(
        !visible_text.contains("SCROLLTAG-00"),
        "the earliest line must have scrolled OFF the visible window, but it is \
         still visible: {visible_text:?}"
    );
    assert!(
        visible_text.contains("SCROLLTAG-39"),
        "the final line must be in the visible window, got {visible_text:?}"
    );

    // The harness's screen keeps scrollback, modelling the real terminal's own
    // store. The discriminator: with the scroll emit, each departed top line was
    // `\r\n`-advanced into scrollback, so the early lines survive there; an in-place
    // `[0, rows)` repaint would have overwritten them, leaving them NOT recoverable.
    for tag in ["SCROLLTAG-00", "SCROLLTAG-01", "SCROLLTAG-02", "SCROLLTAG-03"] {
        assert!(
            recoverable_from_scrollback(&mut done.screen, 80, tag, 1..=60),
            "early scrolled-off line {tag:?} must reach the real terminal's own \
             scrollback (recoverable by scrolling back), got {} bytes of stream",
            done.bytes.len()
        );
    }

    assert_eq!(done.code, Some(0), "gutter propagates the zero exit");
}

/// **Scroll-off survival under a COALESCED BURST (ADR-007 + ADR-013).** The same
/// more-than-a-screenful print, but emitted as fast as the child
/// can — no per-line pacing — so the render loop drains a whole 16 ms window of
/// PTY bytes into the parser before one `render_once`, advancing the content by
/// far more than one band-height in a single frame (the real `cat largefile` /
/// `make` / verbose-test path). The lines that arrive and depart within that one
/// coalesced frame are never on a painted grid; a grid-overlap scroll-delta sees
/// no surviving rows and reports 0, silently dropping the whole burst. The
/// count-based emit (sourced from the scroll tracker, vt100's own scroll
/// machinery) still lands every departed line in the real terminal's scrollback.
///
/// This is the discriminator the paced test cannot make: it deliberately drives a
/// single-frame advance >= the band height.
#[test]
fn scroll_off_burst_reaches_scrollback_without_pacing() {
    // 120 lines, printed as fast as possible: a single 16 ms frame swallows dozens
    // at once on a 24-row terminal. Each line is uniquely tagged. The child holds
    // only once the whole burst is out.
    let child = "/bin/sh -c 'i=0; while [ $i -lt 120 ]; do printf \"BURSTTAG-%03d\\n\" $i; i=$((i+1)); done; read _'";
    let mut gutter = Gutter::spawn(80, 24, &format!("--width 60 --left {child}"));
    gutter.wait_for("the last burst line", |s| s.contents().contains("BURSTTAG-119"));
    gutter.send(b"\n");

    let mut done = gutter.finish();
    let s = String::from_utf8_lossy(&done.bytes);

    // Still a pure primary-screen command — never the alt screen.
    assert!(
        !s.contains("\u{1b}[?1049h") && !s.contains("\u{1b}[?1049l"),
        "a plain bursting command must never emit ?1049h/?1049l"
    );

    // The final visible frame holds the LAST screenful; the earliest line scrolled
    // off and the last line is visible.
    let visible_text = screen_text(&done.screen, 80);
    assert!(
        !visible_text.contains("BURSTTAG-000"),
        "the earliest burst line must have scrolled OFF the visible window: {visible_text:?}"
    );
    assert!(
        visible_text.contains("BURSTTAG-119"),
        "the final burst line must be in the visible window, got {visible_text:?}"
    );

    // The early lines — including ones that arrived and left within a single
    // coalesced frame — must be recoverable from the terminal's own scrollback.
    // Sample across the whole departed range, including the middle (the lines most
    // likely to have arrived-and-departed inside one coalesced frame).
    for tag in [
        "BURSTTAG-000",
        "BURSTTAG-001",
        "BURSTTAG-040",
        "BURSTTAG-080",
        "BURSTTAG-090",
    ] {
        assert!(
            recoverable_from_scrollback(&mut done.screen, 80, tag, 1..=200),
            "burst-scrolled-off line {tag:?} must reach scrollback under coalescing \
             (the count-based emit must not drop a whole-frame turnover), got {} \
             bytes of stream",
            done.bytes.len()
        );
    }

    assert_eq!(done.code, Some(0), "gutter propagates the zero exit");
}

/// **Non-zero exit shows the dim `Exited with: N` status line (the inline hand-back,
/// ADR-013).** A plain child that prints
/// inline output and then exits non-zero: gutter mirrors the child's mode (ADR-012),
/// so it never forces the alt screen, and the teardown hand-back emits the dim
/// `\r\n\x1b[2mExited with: N\x1b[0m` below the band on exit. The hand-back is gated
/// on the positive `ever_painted_inline` signal (ADR-013): the status line
/// captions the band, so it surfaces when a non-zero exit follows real inline output.
/// Asserted on the raw teardown bytes: the status line is present AND gutter never
/// emits `?1049h`/`?1049l` for this plain command.
#[test]
fn non_zero_exit_shows_dim_status_line() {
    let child = "/bin/sh -c 'printf boom; read _; exit 3'";
    let mut gutter = Gutter::spawn(80, 24, &format!("--width 60 {child}"));
    gutter.wait_for("the child's output", |s| s.contents().contains("boom"));
    gutter.send(b"\n");

    let done = gutter.finish();
    let s = String::from_utf8_lossy(&done.bytes);

    assert!(
        s.contains("\u{1b}[2mExited with: 3\u{1b}[0m"),
        "non-zero exit must emit the dim status line, got {s:?}"
    );
    // A plain command never touches the alt screen — neither enter nor leave.
    assert!(
        !s.contains("\u{1b}[?1049h") && !s.contains("\u{1b}[?1049l"),
        "a plain command must never enter/leave the alt screen, got {s:?}"
    );

    assert_eq!(done.code, Some(3), "gutter must propagate the non-zero exit code");
}

/// **A no-output non-zero exit is silent (ADR-013).** The inline
/// hand-back is gated on `ever_painted_inline` alone, never the exit code: a child
/// that prints nothing (`gutter false`) has no band to caption, so no dim status
/// line is stamped onto the restored shell — and crucially the exit code still
/// propagates. This locks the gate so a TUI that drops back to the primary screen
/// and exits non-zero (indistinguishable here by live state) can never be captioned.
#[test]
fn no_output_nonzero_exit_is_silent() {
    // The child prints nothing, so there is nothing to see first; the exit code is
    // the proof it ran.
    let child = "/bin/sh -c 'exit 3'";
    let done = Gutter::spawn(80, 24, &format!("--width 60 {child}")).finish();
    let s = String::from_utf8_lossy(&done.bytes);

    assert!(
        !s.contains("Exited with:"),
        "a no-output non-zero exit must not stamp a status line, got {s:?}"
    );
    assert_eq!(done.code, Some(3), "gutter must still propagate the non-zero exit code");
}

/// **Zero exit is silent.** A plain child that prints inline output and exits
/// cleanly: gutter must emit NO status line — a clean run leaves a clean screen —
/// and, mirroring the child's mode (ADR-012), must never enter or leave the alt
/// screen for a plain command. Asserted on the raw teardown bytes.
#[test]
fn zero_exit_shows_no_status_line() {
    // The child paints the band first: with nothing painted there is no status line
    // at any exit code, and the zero would not be what kept it away.
    let child = "/bin/sh -c 'printf fine; read _; exit 0'";
    let mut gutter = Gutter::spawn(80, 24, &format!("--width 60 {child}"));
    gutter.wait_for("the child's output", |s| s.contents().contains("fine"));
    gutter.send(b"\n");

    let done = gutter.finish();
    let s = String::from_utf8_lossy(&done.bytes);

    // No status line on a clean exit.
    assert!(
        !s.contains("Exited with:"),
        "a zero exit must emit no status line, got {s:?}"
    );
    // And a plain command never touches the alt screen.
    assert!(
        !s.contains("\u{1b}[?1049h") && !s.contains("\u{1b}[?1049l"),
        "a plain command must never enter/leave the alt screen, got {s:?}"
    );

    assert_eq!(done.code, Some(0), "gutter must propagate the zero exit code");
}
