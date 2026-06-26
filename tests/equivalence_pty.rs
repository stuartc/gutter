//! Slice-08 end-to-end Definition-of-Done re-runs against the real-target
//! fixture, driven through the real `gutter` binary over a real PTY.
//!
//! These re-exercise existing code paths (resize ordering, the ADR-010 restore
//! chain) on the *real* Claude Code byte stream — denser and weirder than any
//! hand-written fixture — and assert on what the child and outer terminal
//! actually see, never on gutter internals. They are the E2E counterparts of the
//! offline equivalence gate (`src/oracle/gate.rs`): the gate proves the grid is
//! correct; these prove the full binary survives the same stream end to end.
//!
//! The fixture is replayed INTO gutter by a small shell child that emits its
//! bytes then idles (so the live alt-screen frame is captured, not the post-exit
//! primary screen). Harness facts are shared with `tests/pty.rs` /
//! `tests/resize.rs`: `stty` sets the outer size before gutter reads it, and a
//! bounded non-blocking drain captures the live frame.
//!
//! CI runs these headlessly: a real PTY, no display, `TERM=xterm-256color`.

use std::time::{Duration, Instant};

use expectrl::session::OsSession;

fn gutter_bin() -> String {
    env!("CARGO_BIN_EXE_gutter").to_string()
}

/// The checked-in real-target fixture, on disk so a shell child can `cat` it
/// into gutter (the same bytes the offline gate replays).
fn fixture_path() -> String {
    format!("{}/tests/fixtures/claude-code-flow.cast", env!("CARGO_MANIFEST_DIR"))
}

/// The plain non-alt-screen scrolling fixture (slice 06, A2): SGR-coloured lines
/// past one screenful, no `?1049h`, ending in a marker line.
fn plain_scroll_path() -> String {
    format!("{}/tests/fixtures/plain-scroll.cast", env!("CARGO_MANIFEST_DIR"))
}

/// The wide-char-at-band-edge fixture (slice 06, A2): a settled alt-screen frame
/// with CJK glyphs and an emoji at the band edge. Shared with the offline gate;
/// here it feeds the resize content-equivalence assertion (the wide glyphs must
/// re-wrap correctly when the band width changes — only an E2E test can resize).
fn wide_edge_path() -> String {
    format!("{}/tests/fixtures/wide-edge.cast", env!("CARGO_MANIFEST_DIR"))
}

/// Run gutter inside an outer terminal of the given size, wrapping a shell child
/// that emits the fixture bytes (via `cat`) then idles so the live frame is
/// captured. `GUTTER_FORCE_KITTY=0` skips the kitty probe stall.
fn gutter_replaying_fixture(
    outer_cols: u16,
    outer_rows: u16,
    gutter_flags: &str,
    fixture: &str,
) -> OsSession {
    // The child: dump the recorded stream, then sleep so gutter's live frame
    // stays painted for the capture window.
    let child = format!("/bin/sh -c 'cat {fixture}; sleep 4'");
    let script = format!(
        "stty cols {outer_cols} rows {outer_rows}; exec env GUTTER_FORCE_KITTY=0 GUTTER_FORCE_ANCHOR_ROW=0 {} {gutter_flags} {child}",
        gutter_bin()
    );
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.arg("-c").arg(script);
    OsSession::spawn(cmd).expect("spawn gutter under PTY")
}

/// Run gutter wrapping a shell child that `cat`s the fixture at `fixture` then
/// **exits** (no idle tail) — so teardown runs and the test reads the post-exit
/// stream. The mirror of [`gutter_replaying_fixture`] for the cases that drain
/// through teardown (the plain-command primary-screen regression).
fn gutter_replaying_then_exit(
    outer_cols: u16,
    outer_rows: u16,
    gutter_flags: &str,
    fixture: &str,
) -> OsSession {
    let child = format!("/bin/sh -c 'cat {fixture}; exit 0'");
    let script = format!(
        "stty cols {outer_cols} rows {outer_rows}; exec env GUTTER_FORCE_KITTY=0 GUTTER_FORCE_ANCHOR_ROW=0 {} {gutter_flags} {child}",
        gutter_bin()
    );
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.arg("-c").arg(script);
    OsSession::spawn(cmd).expect("spawn gutter under PTY")
}

/// Bounded, non-blocking drain — a wall-clock cap even while the child idles, so
/// the live alt-screen frame is captured (not the discarded post-exit screen).
fn drain_window(session: &mut OsSession, window: Duration) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    let start = Instant::now();
    while start.elapsed() < window {
        match session.try_read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
        std::thread::sleep(Duration::from_millis(3));
    }
    out
}

/// Parse outer-terminal bytes through a vt100 at the given physical size.
fn outer_grid(bytes: &[u8], cols: u16, rows: u16) -> vt100::Parser {
    let mut p = vt100::Parser::new(rows, cols, 0);
    p.process(bytes);
    p
}

/// Assert the physical columns `[from, to)` on every row are blank — no stale
/// gutter cells.
fn assert_cols_blank(screen: &vt100::Screen, from: u16, to: u16, rows: u16) {
    for r in 0..rows {
        for c in from..to {
            if let Some(cell) = screen.cell(r, c) {
                let s = cell.contents();
                assert!(
                    s.is_empty() || s == " ",
                    "gutter cell ({r},{c}) must be blank, found {s:?}"
                );
            }
        }
    }
}

/// **Reverse-video statusline highlight stops at the band edge (slice 09, the
/// Bug B end-to-end gate).** A child paints a full-width reverse-video row — the
/// nvim/Claude statusline: `ESC[7m` then a row-final `ESC[K` (attributed-but-
/// empty, the single unbounded sequence vt100 emits) — across the band, then
/// idles. Through the real gutter binary at a centred band, the highlight must
/// stay inside `[margin, margin + W)`: no gutter cell may carry reverse video.
///
/// The assertion reads `cell.inverse()`, not the cell contents: `ESC[K` under
/// reverse video **erases** the gutter cells (their `contents()` stays empty)
/// while flooding their background, so a content-only check is structurally blind
/// to exactly this corruption.
#[test]
fn reverse_video_statusline_highlight_stops_at_band_edge() {
    // A 60-wide centred band in an 80-col terminal → margin (80-60)/2 = 10, so the
    // gutters are columns [0,10) and [70,80). Enter the alt screen so the live
    // frame is captured cleanly, paint a full-width reverse-video row, then idle.
    let child =
        "/bin/sh -c 'printf \"\\033[?1049h\\033[?25l\\033[1;1H\\033[7m\\033[K\"; sleep 4'";
    let script = format!(
        "stty cols 80 rows 24; exec env GUTTER_FORCE_KITTY=0 GUTTER_FORCE_ANCHOR_ROW=0 {} --width 60 --center {child}",
        gutter_bin()
    );
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.arg("-c").arg(script);
    let mut session = OsSession::spawn(cmd).expect("spawn gutter under PTY");

    let bytes = drain_window(&mut session, Duration::from_millis(900));
    assert!(!bytes.is_empty(), "gutter must paint the statusline frame");

    let parser = outer_grid(&bytes, 80, 24);
    let screen = parser.screen();

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

    // And the highlight is actually present in-band (the row really was painted,
    // so the test isn't vacuously passing on an empty frame): the last in-band
    // column carries reverse video on the statusline row.
    let in_band_highlight = (0..24u16).any(|r| {
        screen
            .cell(r, 69)
            .map(|cell| cell.inverse())
            .unwrap_or(false)
    });
    assert!(
        in_band_highlight,
        "the reverse-video highlight must reach the last in-band column (69)"
    );

    drop(session);
}

/// **End-to-end resize-stress against the live-target fixture (slice 05's
/// guarantee re-run on real content), now with wide-line content equivalence
/// (slice 06, A2).** Replay the **wide-edge** fixture — CJK glyphs and an emoji
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
    // clear failed. The child idles after the cat so the live frame stays painted.
    let mut session = gutter_replaying_fixture(120, 30, "--width 80 --center", &wide_edge_path());

    // Let the first frame land.
    let _ = drain_window(&mut session, Duration::from_millis(500));

    // A sequence of rapid resizes: wider, narrower (below W so the centred margin
    // clamps to 0), wider again — the wide content must re-wrap at each width.
    for &(cols, rows) in &[(160u16, 40u16), (90, 24), (200, 50)] {
        session
            .get_process_mut()
            .set_window_size(cols, rows)
            .expect("resize outer PTY");
        // Give gutter a moment to handle the SIGWINCH and repaint.
        let _ = drain_window(&mut session, Duration::from_millis(250));
    }

    // Final settle: one last resize back to 120x30 immediately before the
    // capture, so gutter's resize handler forces a fresh full repaint (the
    // ADR-008 step-4 gutter-clear + repaint) into the window we drain.
    session
        .get_process_mut()
        .set_window_size(120, 30)
        .expect("resize outer PTY");
    let bytes = drain_window(&mut session, Duration::from_millis(900));
    assert!(!bytes.is_empty(), "gutter must keep painting through the resizes");

    let parser = outer_grid(&bytes, 120, 30);
    let screen = parser.screen();

    // A centred 80-band in a 120 terminal has margin (120-80)/2 = 20; both
    // gutters must be blank — no stale cells from any intermediate wider layout,
    // and no wide-glyph half stranded outside the band by a re-wrap.
    assert_cols_blank(screen, 0, 20, 30);
    assert_cols_blank(screen, 100, 120, 30);

    // Content equivalence after the reflow (slice 06): the wide glyphs survive
    // the resize stress, intact and inside the band's physical columns
    // `[20, 100)`. A wide glyph mangled by a bad re-wrap would lose its character
    // or strand a half in the gutter — both caught here. We assert at least one
    // CJK glyph and the emoji are present somewhere inside the band region.
    let band_text: String = (0..30u16)
        .flat_map(|r| {
            (20..100u16).filter_map(move |c| {
                screen.cell(r, c).map(|cell| cell.contents())
            })
        })
        .collect();
    assert!(
        band_text.contains('漢') || band_text.contains('字'),
        "a wide CJK glyph must survive the reflow inside the band, got {band_text:?}"
    );
    assert!(
        band_text.contains('🌟'),
        "the wide emoji must survive the reflow inside the band, got {band_text:?}"
    );

    drop(session);
}

/// **Plain non-alt-screen output survives to the primary screen (the E2
/// regression, fixture-driven; slice 06, A2 + ADR-012).** Replay the checked-in
/// `plain-scroll.cast` — SGR-coloured lines past one screenful, ending in a
/// marker line, that **never** emit `?1049h` — through the real gutter binary and
/// drain through teardown. gutter must mirror the child's (primary) mode, never
/// force the alt screen, and leave the output visible on the primary screen: the
/// marker line and an earlier line are present after exit, and `?1049h`/`?1049l`
/// are NEVER emitted. This is the only corpus fixture that can catch E2 — every
/// alt-screen fixture forces `?1049h` at phase 1, which is exactly why E2 is
/// invisible to them.
///
/// Slice 03 (the E2 fix) has landed, so this passes directly. (Had it been built
/// before slice 03 the DoD prescribed landing it `#[ignore]`d pointing at E2; it
/// is not ignored because the fix is present.)
#[test]
fn plain_command_output_survives_to_primary_screen() {
    let mut session = gutter_replaying_then_exit(80, 24, "--width 60 --left", &plain_scroll_path());

    // Drain through teardown: the output is on the primary screen, so it survives
    // (no forced alt screen to wipe it).
    let bytes = drain_window(&mut session, Duration::from_secs(4));
    let s = String::from_utf8_lossy(&bytes);

    // gutter must NEVER force the alt screen for a plain command — the E2 fix.
    // The absence of `?1049l` is what proves no alt-screen discard wiped the band.
    assert!(
        !s.contains("\u{1b}[?1049h") && !s.contains("\u{1b}[?1049l"),
        "a plain command must never emit ?1049h/?1049l (no alt-screen discard), got {s:?}"
    );

    // The marker line and an earlier line are present on the primary screen.
    // gutter keeps vt100 at scrollback=0 and emits departed lines into the real
    // terminal's own scrollback, so parse with a scrollback store to recover both
    // the final visible marker and an earlier scrolled-off line.
    let mut parser = vt100::Parser::new(24, 80, 1000);
    parser.process(&bytes);

    let visible: String = parser.screen().rows(0, 80).collect::<Vec<_>>().join("\n");
    assert!(
        visible.contains("PLAIN-SCROLL-DONE-MARKER"),
        "the marker line must survive on the primary screen, got {visible:?}"
    );

    // An earlier line that scrolled off the visible window must be recoverable
    // from the terminal's own scrollback — it was NOT discarded by an alt-screen
    // leave (the E2 bug), it scrolled into the real terminal's history.
    let mut found_early = false;
    for offset in 0..=80 {
        parser.screen_mut().set_scrollback(offset);
        let text: String = parser.screen().rows(0, 80).collect::<Vec<_>>().join("\n");
        if text.contains("line 05") {
            found_early = true;
            break;
        }
    }
    parser.screen_mut().set_scrollback(0);
    assert!(
        found_early,
        "an earlier line (line 05) must survive in the real terminal's scrollback, \
         not be discarded — the E2 regression"
    );

    assert_eq!(wait_status(session), Some(0), "gutter propagates the zero exit");
}

/// **End-to-end child-exit-restore against the live target (slices 01/04/07
/// restore ordering re-run on real content).** A child that replays the
/// fixture's alt-screen negotiation then exits mid-alt-screen with a specific
/// code: gutter must fully restore the outer terminal (leave alt screen, show
/// cursor) with NO keystroke and propagate the exit code.
#[test]
fn child_exit_mid_alt_screen_restores_and_propagates_code() {
    // The child enters the alt screen (as the fixture's phase 1 does), paints a
    // little, then exits 7 WHILE STILL in the alt screen — the wedged-alt-screen
    // failure mode the ADR-010 restore must prevent.
    let child = "/bin/sh -c 'printf \"\\033[?1049h\\033[?25l\\033[1;1Hclaude\"; exit 7'";
    let script = format!(
        "stty cols 100 rows 30; exec env GUTTER_FORCE_KITTY=0 GUTTER_FORCE_ANCHOR_ROW=0 {} --width 70 --center {child}",
        gutter_bin()
    );
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.arg("-c").arg(script);
    let mut session = OsSession::spawn(cmd).expect("spawn gutter under PTY");

    // Read to the teardown this time — we WANT the restore sequence.
    let bytes = drain_window(&mut session, Duration::from_secs(3));
    let s = String::from_utf8_lossy(&bytes);

    assert!(
        s.contains("\u{1b}[?1049l") || s.contains("\u{1b}[?47l"),
        "restore must leave the alternate screen (mid-alt-screen exit), got {s:?}"
    );
    assert!(
        s.contains("\u{1b}[?25h"),
        "restore must show the cursor with no keystroke, got {s:?}"
    );

    assert_eq!(
        wait_status(session),
        Some(7),
        "gutter must propagate the child's exit code from a mid-alt-screen exit"
    );
}

/// **The child sees `COLUMNS == W` while replaying the fixture.** A sanity
/// re-run of the band-width invariant on the real stream: the child gutter wraps
/// is told it has the band width `W`, independent of the outer width.
#[test]
fn child_sees_band_width_while_replaying_fixture() {
    // Replace the idle tail with `stty size` so the child reports its columns.
    let child = format!(
        "/bin/sh -c 'cat {}; stty size; sleep 3'",
        fixture_path()
    );
    let script = format!(
        "stty cols 120 rows 30; exec env GUTTER_FORCE_KITTY=0 GUTTER_FORCE_ANCHOR_ROW=0 {} --width 80 --left {child}",
        gutter_bin()
    );
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.arg("-c").arg(script);
    let mut session = OsSession::spawn(cmd).expect("spawn gutter under PTY");

    let bytes = drain_window(&mut session, Duration::from_millis(900));
    let parser = outer_grid(&bytes, 120, 30);
    // The fixture left the cursor low; `stty size` prints "30 80" somewhere.
    let shows_80 = parser.screen().rows(0, 120).any(|r| r.contains("80"));
    assert!(
        shows_80,
        "the child must see the band width W=80 while the fixture replays"
    );
    drop(session);
}

/// **Inline anchor no-top-overlap (slice 10 / ADR-013 — the gate for this slice).**
/// Seed ten history lines on the real terminal, then launch gutter with the band
/// anchored at row 10 (`GUTTER_FORCE_ANCHOR_ROW` — the kernel PTY can't answer the
/// CPR query). A child prints three band lines and idles. The history above the
/// anchor must survive untouched and the band must paint at physical rows `>= 10` —
/// never over row 0, the absolute-paint overpaint this slice exists to fix. This is
/// the test most likely to fail first if the offset paint regresses to absolute rows.
#[test]
fn inline_anchor_does_not_overpaint_history_above() {
    let child = "/bin/sh -c 'printf \"BAND-A\\nBAND-B\\nBAND-C\"; sleep 4'";
    let seed = "i=1; while [ $i -le 10 ]; do printf 'HIST-%02d\\n' \"$i\"; i=$((i+1)); done;";
    let script = format!(
        "stty cols 80 rows 24; {seed} exec env GUTTER_FORCE_KITTY=0 GUTTER_FORCE_ANCHOR_ROW=10 {} --width 60 --left {child}",
        gutter_bin()
    );
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.arg("-c").arg(script);
    let mut session = OsSession::spawn(cmd).expect("spawn gutter under PTY");

    let bytes = drain_window(&mut session, Duration::from_millis(900));
    assert!(!bytes.is_empty(), "gutter must paint the band frame");

    let parser = outer_grid(&bytes, 80, 24);
    let screen = parser.screen();
    let rows_text: Vec<String> = screen.rows(0, 80).map(|r| r.trim_end().to_string()).collect();

    // The history above the anchor survives — HIST-01 is still at the very top.
    assert!(
        rows_text[0].contains("HIST-01"),
        "history line 1 must survive at row 0, got {:?}",
        rows_text[0]
    );
    // Row 0 was NOT overpainted by the band — the bug this slice fixes.
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

    drop(session);
}

/// **Inline clean hand-back, zero and non-zero (slice 10 / ADR-013).** A plain child
/// prints three lines from a mid-screen anchor (row 3) and exits. The lines survive
/// on the primary screen, the final cursor lands on a **fresh line below** the band
/// (a following prompt would not overlap it), and the dim status line is present
/// **only** on the non-zero exit.
#[test]
fn inline_clean_hand_back_below_band() {
    for (exit_code, expect_status) in [(0i32, false), (5i32, true)] {
        let child = format!("/bin/sh -c \"printf 'HB-1\\nHB-2\\nHB-3'; exit {exit_code}\"");
        let script = format!(
            "stty cols 80 rows 24; exec env GUTTER_FORCE_KITTY=0 GUTTER_FORCE_ANCHOR_ROW=3 {} --width 60 --left {child}",
            gutter_bin()
        );
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.arg("-c").arg(script);
        let mut session = OsSession::spawn(cmd).expect("spawn gutter under PTY");

        let bytes = drain_window(&mut session, Duration::from_secs(3));
        let s = String::from_utf8_lossy(&bytes);

        let mut parser = vt100::Parser::new(24, 80, 100);
        parser.process(&bytes);
        let screen = parser.screen();
        let rows_text: Vec<String> = screen.rows(0, 80).map(|r| r.trim_end().to_string()).collect();

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
            wait_status(session),
            Some(exit_code),
            "gutter must propagate the exit code {exit_code}"
        );
    }
}

/// **Inline mid-screen scroll-through preserves history (slice 10 / ADR-013).** Seed
/// twelve history lines, anchor the band mid-screen (row 12), and have the child
/// print well past a screenful so the band fills, `base_row` reaches 0, and slice
/// 04's scroll-emit engine takes over. The seeded history above the band must
/// survive into the **real terminal's own scrollback** through the `base_row → 0`
/// transition, and the band's own early lines must reach scrollback too.
#[test]
fn inline_mid_screen_scroll_through_preserves_history() {
    let child = "/bin/sh -c 'i=0; while [ $i -lt 40 ]; do printf \"FLOW-%02d\\n\" \"$i\"; i=$((i+1)); done; sleep 1'";
    let seed = "i=1; while [ $i -le 12 ]; do printf 'OLD-%02d\\n' \"$i\"; i=$((i+1)); done;";
    let script = format!(
        "stty cols 80 rows 24; {seed} exec env GUTTER_FORCE_KITTY=0 GUTTER_FORCE_ANCHOR_ROW=12 {} --width 60 --left {child}",
        gutter_bin()
    );
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.arg("-c").arg(script);
    let mut session = OsSession::spawn(cmd).expect("spawn gutter under PTY");

    let bytes = drain_window(&mut session, Duration::from_secs(3));

    // Parse with a generous scrollback store — modelling the real terminal's history.
    let mut parser = vt100::Parser::new(24, 80, 2000);
    parser.process(&bytes);

    let recovered = |parser: &mut vt100::Parser, tag: &str| -> bool {
        for offset in 0..=300 {
            parser.screen_mut().set_scrollback(offset);
            let text: String = parser.screen().rows(0, 80).collect::<Vec<_>>().join("\n");
            if text.contains(tag) {
                parser.screen_mut().set_scrollback(0);
                return true;
            }
        }
        parser.screen_mut().set_scrollback(0);
        false
    };

    assert!(
        recovered(&mut parser, "OLD-01"),
        "seeded history must survive into the terminal's scrollback through base_row -> 0"
    );
    assert!(
        recovered(&mut parser, "FLOW-00"),
        "the band's early lines must reach scrollback once the engine takes over"
    );

    drop(session);
}

/// **Inline alt excursion preserves the anchor (bash->vim->bash, slice 10 / ADR-013).**
/// A child prints two inline lines, enters the alt screen and paints a frame, leaves
/// the alt screen, prints two more inline lines, then exits — the bash->vim->bash
/// shape. gutter must toggle the outer alt screen exactly once each way, restore the
/// pre-alt band at its anchor, append the post-alt lines **below** it (the anchor
/// preserved across the excursion), and hand back cleanly on the zero exit.
#[test]
fn inline_alt_excursion_preserves_anchor() {
    let child = "/bin/sh -c \"printf 'PRE-1\\nPRE-2\\n'; sleep 0.3; printf '\\033[?1049h\\033[1;1HALT-FRAME'; sleep 0.3; printf '\\033[?1049l'; sleep 0.1; printf 'POST-1\\nPOST-2'; exit 0\"";
    let script = format!(
        "stty cols 80 rows 24; exec env GUTTER_FORCE_KITTY=0 GUTTER_FORCE_ANCHOR_ROW=5 {} --width 60 --left {child}",
        gutter_bin()
    );
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.arg("-c").arg(script);
    let mut session = OsSession::spawn(cmd).expect("spawn gutter under PTY");

    let bytes = drain_window(&mut session, Duration::from_secs(3));
    let s = String::from_utf8_lossy(&bytes);

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

    // Replay the full stream (it carries gutter's own alt enter/leave): the final
    // primary screen has the pre-alt band restored and the post-alt lines below it.
    let mut parser = vt100::Parser::new(24, 80, 100);
    parser.process(&bytes);
    let screen = parser.screen();
    let rows_text: Vec<String> = screen.rows(0, 80).map(|r| r.trim_end().to_string()).collect();

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

    assert_eq!(
        wait_status(session),
        Some(0),
        "the inline hand-back propagates the zero exit"
    );
}

/// Block on the wrapped process and return its exit code, if any.
fn wait_status(session: OsSession) -> Option<i32> {
    use expectrl::process::unix::WaitStatus;
    use expectrl::process::Healthcheck;
    let proc = session.get_process();
    let start = Instant::now();
    loop {
        match proc.get_status() {
            Ok(WaitStatus::Exited(_, code)) => return Some(code),
            Ok(WaitStatus::Signaled(_, _, _)) => return None,
            _ => {}
        }
        if start.elapsed() > Duration::from_secs(5) {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
