//! End-to-end re-runs against the real-target fixture, driven through the real
//! `gutter` binary over a real PTY.
//!
//! These re-exercise existing code paths (resize ordering, the ADR-010 restore
//! chain) on the *real* Claude Code byte stream — denser and weirder than any
//! hand-written fixture — and assert on what the child and outer terminal
//! actually see, never on gutter internals. They are the E2E counterparts of the
//! offline equivalence gate (`src/oracle/gate.rs`): the gate proves the grid is
//! correct; these prove the full binary survives the same stream end to end.
//!
//! The fixture is replayed INTO gutter by a small shell child that emits its
//! bytes then blocks on a `read`. Harness facts are shared with `tests/pty.rs`: the
//! outer size is set before gutter starts and reads it, and the child is held until
//! the test has seen its output on the outer screen, then let go with Enter.
//!
//! CI runs these headlessly: a real PTY, no display, `TERM=xterm-256color`.

mod common;
use common::{
    assert_cols_blank, cell_text, recoverable_from_scrollback, row_text, screen_text, Gutter,
};

/// Every row of `screen`, trailing blanks trimmed.
fn rows_text(screen: &vt100::Screen) -> Vec<String> {
    let (rows, _) = screen.size();
    (0..rows).map(|r| row_text(screen, r)).collect()
}

/// A checked-in fixture's path, so a shell child can `cat` it into gutter (the
/// same bytes the offline gate replays).
fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

/// **Reverse-video statusline highlight stops at the band edge.**
/// A child paints a full-width reverse-video row — the
/// nvim/Claude statusline: `ESC[7m` then a row-final `ESC[K` (attributed-but-
/// empty, the single unbounded sequence vt100 emits) — across the band, then
/// holds. Through the real gutter binary at a centred band, the highlight must
/// stay inside `[margin, margin + W)`: no gutter cell may carry reverse video.
///
/// The assertion reads `cell.inverse()`, not the cell contents: `ESC[K` under
/// reverse video **erases** the gutter cells (their `contents()` stays empty)
/// while flooding their background, so a content-only check is structurally blind
/// to exactly this corruption.
#[test]
fn reverse_video_statusline_highlight_stops_at_band_edge() {
    // A 60-wide centred band in an 80-col terminal → margin (80-60)/2 = 10, so the
    // gutters are columns [0,10) and [70,80). The child enters the alt screen and
    // paints a full-width reverse-video row on its top line.
    let child =
        "/bin/sh -c 'printf \"\\033[?1049h\\033[?25l\\033[1;1H\\033[7m\\033[K\"; read _'";
    let mut gutter = Gutter::spawn(80, 24, &format!("--width 60 --center {child}"));

    // The highlight reaching the band's last column (69) is what shows the row was
    // painted, so the gutter check below cannot pass on an empty frame.
    gutter.wait_for("reverse video at the band's last column", |s| {
        s.cell(0, 69).is_some_and(|cell| cell.inverse())
    });
    gutter.send(b"\n");
    let screen = gutter
        .finish()
        .alt_screen
        .expect("the child's alt screen is left at teardown");

    // No gutter cell — left [0,10) or right [70,80) — may carry the highlight.
    for r in 0..24u16 {
        for c in (0..10u16).chain(70..80u16) {
            if let Some(cell) = screen.cell(r, c) {
                assert!(
                    !cell.inverse(),
                    "the statusline highlight flooded the gutter at ({r},{c}) — \
                     reverse video must stop at the band edge"
                );
            }
        }
    }
}

/// **End-to-end resize-stress against the live-target fixture, with wide-line
/// content equivalence.** Replay the **wide-edge** fixture — a settled alt-screen
/// frame with CJK glyphs and an emoji
/// laid at the band edge, reflow-sensitive content the alt-screen keystone can't
/// reach — through gutter, then drive a sequence of outer resizes. Assert gutter
/// survives every resize with no panic (the binary stays alive and keeps
/// painting), the gutter columns hold no stale cells at the final settle
/// (`COLUMNS == W`), AND the wide glyphs are still rendered correctly inside the
/// band after the reflow (content equivalence, not just blank gutters): the CJK
/// glyphs and the emoji survive, intact and within `[margin, margin + W)`, with no
/// wide-glyph half stranded in the gutter by a re-wrap.
///
/// The wide-edge stream is replayed via the offline gate's own fixture (the same
/// bytes), so the offline-gate cell equivalence and this E2E reflow check share a
/// fixture (the offline gate cannot resize; only this E2E path can).
#[test]
fn resize_stress_against_fixture_no_panic_no_stale_gutter() {
    // Absolute --width 80 (the width the wide-edge fixture was recorded at),
    // centred so a resize moves the margin and would strand cells if the gutter
    // clear failed. The child holds on a `read` after the cat, so the frame stays
    // painted until the test ends the run with Enter.
    let mut gutter = Gutter::spawn(
        120,
        30,
        &format!(
            "--width 80 --center /bin/sh -c 'cat {}; read _'",
            fixture("wide-edge.cast")
        ),
    );

    // The child prints nothing on a resize, so the only sign that gutter handled one
    // is its own repaint at the new margin. The fixture's row 2 is 78 `A`s and a `漢`:
    // it starts exactly at `margin`, with a blank cell before it, at one margin only.
    let row_2_at = |margin: u16| {
        move |s: &common::Outer| {
            cell_text(s, 2, margin - 1).trim().is_empty()
                && cell_text(s, 2, margin) == "A"
                && cell_text(s, 2, margin + 78) == "漢"
        }
    };
    gutter.wait_for("the fixture at margin 20", row_2_at(20));

    // Wider, narrower, wider again, each to a margin of its own (40, 5, 60), and each
    // repainted before the next: resizes fired blind collapse into one SIGWINCH and
    // the layouts in between are never exercised.
    //
    // The last size is one no earlier step used. Ending back at the launch size
    // would leave a screen that looks the same whether the resizes were handled or not.
    for (cols, rows, margin) in [(160u16, 40u16, 40u16), (90, 24, 5), (200, 50, 60), (140, 30, 30)] {
        gutter.resize(cols, rows);
        gutter.wait_for(
            &format!("the repaint at {cols}x{rows}, margin {margin}"),
            row_2_at(margin),
        );
    }

    gutter.send(b"\n");
    let done = gutter.finish();
    let screen = done
        .alt_screen
        .expect("the fixture's alt screen is left at teardown");

    // A centred 80-band in a 140 terminal has margin (140-80)/2 = 30; both gutters
    // must be blank. The screen has been fed every byte since launch and resized in
    // step with the terminal, so a cell an earlier, wider layout left behind — or a
    // wide-glyph half stranded outside the band by a re-wrap — is still on it.
    assert_cols_blank(&screen, 0, 30, 30);
    assert_cols_blank(&screen, 110, 140, 30);

    // Content equivalence after the reflow: the wide glyphs survive
    // the resize stress, intact and inside the band's physical columns
    // `[30, 110)`. A wide glyph mangled by a bad re-wrap would lose its character
    // or strand a half in the gutter — both caught here. We assert at least one
    // CJK glyph and the emoji are present somewhere inside the band region.
    let band_text: String = (0..30u16)
        .flat_map(|r| (30..110u16).map(move |c| (r, c)))
        .map(|(r, c)| cell_text(&screen, r, c))
        .collect();
    assert!(
        band_text.contains('漢') || band_text.contains('字'),
        "a wide CJK glyph must survive the reflow inside the band, got {band_text:?}"
    );
    assert!(
        band_text.contains('🌟'),
        "the wide emoji must survive the reflow inside the band, got {band_text:?}"
    );
}

/// **Plain non-alt-screen output survives to the primary screen (the E2
/// regression, fixture-driven; ADR-012).** Replay the checked-in
/// `plain-scroll.cast` — SGR-coloured lines past one screenful, ending in a
/// marker line, that **never** emit `?1049h` — through the real gutter binary and
/// read through teardown. gutter must mirror the child's (primary) mode, never
/// force the alt screen, and leave the output visible on the primary screen: the
/// marker line and an earlier line are present after exit, and `?1049h`/`?1049l`
/// are NEVER emitted. This is the only corpus fixture that can catch E2 — every
/// alt-screen fixture forces `?1049h` at phase 1, which is exactly why E2 is
/// invisible to them.
#[test]
fn plain_command_output_survives_to_primary_screen() {
    let mut gutter = Gutter::spawn(
        80,
        24,
        &format!(
            "--width 60 --left /bin/sh -c 'cat {}; read _'",
            fixture("plain-scroll.cast")
        ),
    );
    gutter.wait_for("the fixture's last line", |s| {
        s.contents().contains("PLAIN-SCROLL-DONE-MARKER")
    });
    gutter.send(b"\n");

    // Read through teardown: the output is on the primary screen, so it survives
    // (no forced alt screen to wipe it).
    let mut done = gutter.finish();
    let s = String::from_utf8_lossy(&done.bytes);

    // gutter must NEVER force the alt screen for a plain command — the E2 fix.
    // The absence of `?1049l` is what proves no alt-screen discard wiped the band.
    assert!(
        !s.contains("\u{1b}[?1049h") && !s.contains("\u{1b}[?1049l"),
        "a plain command must never emit ?1049h/?1049l (no alt-screen discard), got {s:?}"
    );

    // The marker line and an earlier line are present on the primary screen.
    // gutter keeps vt100 at scrollback=0 and emits departed lines into the real
    // terminal's own scrollback, so the screen's scrollback holds the earlier,
    // scrolled-off line.
    let visible = screen_text(&done.screen, 80);
    assert!(
        visible.contains("PLAIN-SCROLL-DONE-MARKER"),
        "the marker line must survive on the primary screen, got {visible:?}"
    );

    // An earlier line that scrolled off the visible window must be recoverable
    // from the terminal's own scrollback — it was NOT discarded by an alt-screen
    // leave (the E2 bug), it scrolled into the real terminal's history.
    assert!(
        recoverable_from_scrollback(&mut done.screen, 80, "line 05", 0..=80),
        "an earlier line (line 05) must survive in the real terminal's scrollback, \
         not be discarded — the E2 regression"
    );

    assert_eq!(done.code, Some(0), "gutter propagates the zero exit");
}

/// **End-to-end child-exit-restore against the live target.** A child that replays the
/// fixture's alt-screen negotiation then exits mid-alt-screen with a specific
/// code: gutter must fully restore the outer terminal (leave alt screen, show
/// cursor) with NO keystroke and propagate the exit code.
#[test]
fn child_exit_mid_alt_screen_restores_and_propagates_code() {
    // The child enters the alt screen (as the fixture's phase 1 does), paints a
    // little, then exits 7 WHILE STILL in the alt screen — the wedged-alt-screen
    // failure mode the ADR-010 restore must prevent.
    let child = "/bin/sh -c 'printf \"\\033[?1049h\\033[?25l\\033[1;1Hclaude\"; read _; exit 7'";
    let mut gutter = Gutter::spawn(100, 30, &format!("--width 70 --center {child}"));
    gutter.wait_for("the child's alt-screen frame", |s| {
        s.alternate_screen() && s.hide_cursor() && s.contents().contains("claude")
    });
    gutter.send(b"\n");

    // Read to the teardown — we WANT the restore sequence.
    let done = gutter.finish();
    let s = String::from_utf8_lossy(&done.bytes);

    assert!(
        s.contains("\u{1b}[?1049l") || s.contains("\u{1b}[?47l"),
        "restore must leave the alternate screen (mid-alt-screen exit), got {s:?}"
    );
    assert!(
        s.contains("\u{1b}[?25h"),
        "restore must show the cursor with no keystroke, got {s:?}"
    );

    assert_eq!(
        done.code,
        Some(7),
        "gutter must propagate the child's exit code from a mid-alt-screen exit"
    );
}

/// **The child sees `COLUMNS == W` while replaying the fixture.** A sanity
/// re-run of the band-width invariant on the real stream: the child gutter wraps
/// is told it has the band width `W`, independent of the outer width.
#[test]
fn child_sees_band_width_while_replaying_fixture() {
    // `stty size` after the fixture, so the child reports its own rows and columns.
    let child = format!(
        "/bin/sh -c 'cat {}; stty size; read _'",
        fixture("claude-code-flow.cast")
    );
    let mut gutter = Gutter::spawn(120, 30, &format!("--width 80 --left {child}"));

    // The fixture left the cursor low; `stty size` prints "30 80" wherever that is.
    // A child told the outer width would print "30 120".
    gutter.wait_for("the child's `stty size` to read 30 80", |s| {
        s.contents().contains("30 80")
    });
    gutter.send(b"\n");
    gutter.finish();
}

/// **Inline anchor no-top-overlap (ADR-013).**
/// Seed ten history lines on the real terminal, then launch gutter with the band
/// anchored at row 10 (`GUTTER_FORCE_ANCHOR_ROW` — the kernel PTY can't answer the
/// CPR query). A child prints three band lines and holds. The history above the
/// anchor must survive untouched and the band must paint at physical rows `>= 10` —
/// never over row 0. This is the test most likely to fail first if the offset paint
/// regresses to absolute rows.
#[test]
fn inline_anchor_does_not_overpaint_history_above() {
    let child = "/bin/sh -c 'printf \"BAND-A\\nBAND-B\\nBAND-C\"; read _'";
    let seed = "i=1; while [ $i -le 10 ]; do printf 'HIST-%02d\\n' \"$i\"; i=$((i+1)); done;";
    let mut gutter =
        Gutter::spawn_anchored(80, 24, 10, seed, &format!("--width 60 --left {child}"));
    gutter.wait_for("the band's last line", |s| s.contents().contains("BAND-C"));
    gutter.send(b"\n");
    let rows_text = rows_text(&gutter.finish().screen);

    // The history above the anchor survives — HIST-01 is still at the very top.
    assert!(
        rows_text[0].contains("HIST-01"),
        "history line 1 must survive at row 0, got {:?}",
        rows_text[0]
    );
    // Row 0 was NOT overpainted by the band.
    assert!(
        !rows_text[0].contains("BAND"),
        "the band must not overpaint the top of the screen, row 0 = {:?}",
        rows_text[0]
    );
    // The band paints at or below the launch anchor (row 10), never above it.
    let band_row = rows_text
        .iter()
        .position(|r| r.contains("BAND-A"))
        .expect("the band's first line must be painted somewhere");
    assert!(
        band_row >= 10,
        "the band must paint at or below the launch row 10, found BAND-A at row {band_row}"
    );
}

/// **Inline clean hand-back, zero and non-zero (ADR-013).** A plain child
/// prints three lines from a mid-screen anchor (row 3) and exits. The lines survive
/// on the primary screen, the final cursor lands on a **fresh line below** the band
/// (a following prompt would not overlap it), and the dim status line is present
/// **only** on the non-zero exit.
#[test]
fn inline_clean_hand_back_below_band() {
    for (exit_code, expect_status) in [(0i32, false), (5i32, true)] {
        // Echo is off so the Enter that releases the child leaves its cursor at the
        // end of `HB-3`: the fresh line below the band has to be gutter's doing.
        let child = format!(
            "/bin/sh -c \"stty -echo; printf 'HB-1\\nHB-2\\nHB-3'; read _; exit {exit_code}\""
        );
        let mut gutter =
            Gutter::spawn_anchored(80, 24, 3, "", &format!("--width 60 --left {child}"));
        gutter.wait_for("the band's last line", |s| s.contents().contains("HB-3"));
        gutter.send(b"\n");
        let done = gutter.finish();
        let s = String::from_utf8_lossy(&done.bytes);
        let screen = &done.screen;
        let rows_text = rows_text(screen);

        for tag in ["HB-1", "HB-2", "HB-3"] {
            assert!(
                rows_text.iter().any(|r| r.contains(tag)),
                "{tag} must survive on the primary screen (exit {exit_code}), rows = {rows_text:?}"
            );
        }

        // The band sits at rows 3..5 (anchor 3); the final cursor lands below it.
        let band_bottom = rows_text
            .iter()
            .rposition(|r| r.contains("HB-3"))
            .expect("the last band line must be present");
        let (crow, _ccol) = screen.cursor_position();
        assert!(
            crow as usize > band_bottom,
            "the final cursor must land on a fresh line below the band (cursor row {crow} > band bottom {band_bottom}), exit {exit_code}"
        );

        // The dim status line rides only the non-zero exit.
        assert_eq!(
            s.contains("Exited with:"),
            expect_status,
            "status line presence must match the exit code {exit_code}, got {s:?}"
        );

        assert_eq!(
            done.code,
            Some(exit_code),
            "gutter must propagate the exit code {exit_code}"
        );
    }
}

/// **Inline mid-screen scroll-through preserves history (ADR-013).** Seed
/// twelve history lines, anchor the band mid-screen (row 12), and have the child
/// print well past a screenful so the band fills, `base_row` reaches 0, and the
/// scroll-emit engine takes over. The seeded history above the band must
/// survive into the **real terminal's own scrollback** through the `base_row → 0`
/// transition, and the band's own early lines must reach scrollback too.
#[test]
fn inline_mid_screen_scroll_through_preserves_history() {
    let child = "/bin/sh -c 'i=0; while [ $i -lt 40 ]; do printf \"FLOW-%02d\\n\" \"$i\"; i=$((i+1)); done; read _'";
    let seed = "i=1; while [ $i -le 12 ]; do printf 'OLD-%02d\\n' \"$i\"; i=$((i+1)); done;";
    let mut gutter =
        Gutter::spawn_anchored(80, 24, 12, seed, &format!("--width 60 --left {child}"));
    gutter.wait_for("the child's last line", |s| s.contents().contains("FLOW-39"));
    gutter.send(b"\n");

    // The screen's scrollback stands in for the real terminal's history.
    let mut screen = gutter.finish().screen;

    assert!(
        recoverable_from_scrollback(&mut screen, 80, "OLD-01", 0..=300),
        "seeded history must survive into the terminal's scrollback through base_row -> 0"
    );
    assert!(
        recoverable_from_scrollback(&mut screen, 80, "FLOW-00", 0..=300),
        "the band's early lines must reach scrollback once the engine takes over"
    );
}

/// **Inline alt excursion preserves the anchor (bash->vim->bash, ADR-013).**
/// A child prints two inline lines, enters the alt screen and paints a frame, leaves
/// the alt screen, prints two more inline lines, then exits — the bash->vim->bash
/// shape. gutter must toggle the outer alt screen exactly once each way, restore the
/// pre-alt band at its anchor, append the post-alt lines **below** it (the anchor
/// preserved across the excursion), and hand back cleanly on the zero exit.
#[test]
fn inline_alt_excursion_preserves_anchor() {
    // Each step waits on a `read`, so gutter has painted it before the next one
    // starts: steps that reach gutter together are folded into one frame, and an
    // alt enter and leave in the same frame cancel out. Echo is off so the Enters
    // that release the child add no lines of their own to the band.
    let child = "/bin/sh -c \"stty -echo; printf 'PRE-1\\nPRE-2\\n'; read _; printf '\\033[?1049h\\033[1;1HALT-FRAME'; read _; printf '\\033[?1049l'; read _; printf 'POST-1\\nPOST-2'; read _\"";
    let mut gutter =
        Gutter::spawn_anchored(80, 24, 5, "", &format!("--width 60 --left {child}"));

    gutter.wait_for("the pre-alt lines", |s| s.contents().contains("PRE-2"));
    gutter.send(b"\n");
    // Only the frame's tail is waited for: gutter diffs the alt frame against the
    // primary one, so the `-` that `ALT-FRAME` shares with `PRE-1` is never painted.
    gutter.wait_for("the alt-screen frame", |s| {
        s.alternate_screen() && row_text(s, 0).ends_with("FRAME")
    });
    gutter.send(b"\n");
    gutter.wait_for("the primary screen again", |s| {
        !s.alternate_screen() && s.contents().contains("PRE-2")
    });
    gutter.send(b"\n");
    gutter.wait_for("the post-alt lines", |s| s.contents().contains("POST-2"));
    gutter.send(b"\n");
    let done = gutter.finish();
    let s = String::from_utf8_lossy(&done.bytes);

    // Exactly one outer alt-enter and one outer alt-leave (gutter's own toggles,
    // never the child's — gutter parses those into its grid, it does not echo them).
    assert_eq!(
        s.matches("\u{1b}[?1049h").count(),
        1,
        "exactly one outer alt-enter, got {s:?}"
    );
    assert_eq!(
        s.matches("\u{1b}[?1049l").count() + s.matches("\u{1b}[?47l").count(),
        1,
        "exactly one outer alt-leave, got {s:?}"
    );

    // The final primary screen has the pre-alt band restored and the post-alt lines
    // below it.
    let rows_text = rows_text(&done.screen);

    let pre_row = rows_text
        .iter()
        .position(|r| r.contains("PRE-1"))
        .expect("the pre-alt band must be restored after the excursion");
    let post_row = rows_text
        .iter()
        .position(|r| r.contains("POST-1"))
        .expect("the post-alt lines must be present");
    assert!(
        post_row > pre_row,
        "post-alt lines must sit below the pre-alt band (anchor preserved), pre = {pre_row}, post = {post_row}"
    );
    assert_eq!(
        pre_row, 5,
        "the pre-alt band stays anchored at the launch row 5 across the alt excursion"
    );

    assert_eq!(done.code, Some(0), "the inline hand-back propagates the zero exit");
}

