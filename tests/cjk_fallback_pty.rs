//! Wide-char edge-of-band over a **real PTY** (the CJK suite's integration leg).
//!
//! The unit suite in `src/render.rs::cjk` drives `parser.process()` + a
//! `RecordingGrid` directly (pure, no PTY) and proves the four edge-of-band
//! cases two-sidedly. This file layers the end-to-end check on top: a tiny
//! scripted child emits absolute-column wide-char sequences through a real PTY,
//! gutter wraps it in a band, and we parse gutter's painted outer frame back
//! through a second `vt100` to assert the physical cells — the child-side oracle
//! and the physical outer cells agree, and nothing bleeds past `margin + W`.
//!
//! The escape bytes are constructed explicitly (no CJK font or locale needed);
//! the child is `printf` only, deterministic. CI runs this headlessly: a real
//! PTY, no display, `TERM=xterm-256color`.

use std::time::{Duration, Instant};

use expectrl::session::OsSession;

fn gutter_bin() -> String {
    env!("CARGO_BIN_EXE_gutter").to_string()
}

/// Run gutter inside an outer terminal of a fixed size:
/// `sh -c 'stty cols C rows R; exec gutter <args>'`. `stty` sets gutter's own
/// controlling-terminal size BEFORE it reads it at startup — no resize race.
fn gutter_in_terminal(outer_cols: u16, outer_rows: u16, gutter_args: &str) -> std::process::Command {
    // `GUTTER_FORCE_KITTY=0`: this suite is not about the keyboard, and a dumb
    // test PTY cannot answer the `supports_keyboard_enhancement()` query — the
    // real probe would stall ~2s on its timeout and then return false anyway.
    // Injecting the known result skips the stall without changing behaviour
    // (slice 04's injectable-capability seam).
    let script = format!(
        "stty cols {outer_cols} rows {outer_rows}; exec env GUTTER_FORCE_KITTY=0 {} {gutter_args}",
        gutter_bin()
    );
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.arg("-c").arg(script);
    cmd
}

fn spawn(cmd: std::process::Command) -> OsSession {
    OsSession::spawn(cmd).expect("spawn gutter under PTY")
}

/// Drain a bounded window with non-blocking reads, so the window is a real
/// wall-clock cap even while the child keeps the PTY open — we capture the live
/// alt-screen frame gutter painted, not the post-exit primary screen.
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

/// Parse gutter's outer-terminal bytes through a vt100 at the physical size, so
/// the test can read which physical column each glyph landed in.
fn outer_grid(bytes: &[u8], cols: u16, rows: u16) -> vt100::Parser {
    let mut p = vt100::Parser::new(rows, cols, 0);
    p.process(bytes);
    p
}

/// Assert the physical band-edge column `margin + W` and every gutter column to
/// its right hold no painted glyph on any row.
fn assert_band_edge_and_gutters_blank(
    screen: &vt100::Screen,
    band: u16,
    outer_cols: u16,
    rows: u16,
) {
    for r in 0..rows {
        for c in band..outer_cols {
            if let Some(cell) = screen.cell(r, c) {
                let s = cell.contents();
                assert!(
                    s.is_empty() || s == " ",
                    "physical cell ({r},{c}) past the band must be blank, found {s:?}"
                );
            }
        }
    }
}

/// **Wide glyph at the band edge over a real PTY.** A child positions the cursor
/// at the band's last column (`CSI 1;W H`) and emits U+4E00 (一). gutter renders
/// it in a `W`-column band; the wide glyph must stay inside the band and the
/// physical band-edge column `margin + W` (and every gutter column) must be
/// blank — no half-glyph spill, asserted on the physical outer cells.
#[test]
fn wide_glyph_at_band_edge_does_not_bleed() {
    let (band, outer_cols, outer_rows) = (40u16, 100u16, 24u16);
    // CSI 1;40H positions at child col 39 (= W-1, 0-based); then U+4E00.
    // \343\200\200-ish: use the literal UTF-8 of 一 = E4 B8 80.
    let child = format!(
        "/bin/sh -c 'printf \"\\033[1;{band}H\\344\\270\\200\"; sleep 3'"
    );
    let cmd = gutter_in_terminal(outer_cols, outer_rows, &format!("--width {band} --left {child}"));
    let mut session = spawn(cmd);
    let bytes = drain_window(&mut session, Duration::from_millis(800));

    let parser = outer_grid(&bytes, outer_cols, outer_rows);
    let screen = parser.screen();

    // The glyph must not be a wide LEAD at the band's last column — that would
    // push its continuation half into the gutter at margin + W.
    if let Some(cell) = screen.cell(0, band - 1) {
        assert!(
            !cell.is_wide(),
            "wide glyph must not sit at the band's last column (would spill into the gutter)"
        );
    }
    assert_band_edge_and_gutters_blank(screen, band, outer_cols, outer_rows);

    drop(session);
}

/// **A wide-char line renders inside the band over a real PTY.** A child emits a
/// short CJK line from the home position; the glyphs render inside the band with
/// correct column alignment and the gutter columns stay empty — the integration
/// counterpart to the unit golden-master.
#[test]
fn wide_char_line_renders_inside_band() {
    let (band, outer_cols, outer_rows) = (40u16, 100u16, 24u16);
    // 一二三 = E4 B8 80  E4 BA 8C  E4 B8 89 at the home position.
    let child = "/bin/sh -c 'printf \"\\033[1;1H\\344\\270\\200\\344\\272\\214\\344\\270\\211\"; sleep 3'";
    let cmd = gutter_in_terminal(outer_cols, outer_rows, &format!("--width {band} --left {child}"));
    let mut session = spawn(cmd);
    let bytes = drain_window(&mut session, Duration::from_millis(800));

    let parser = outer_grid(&bytes, outer_cols, outer_rows);
    let screen = parser.screen();

    // The three glyphs land at physical cols 0, 2, 4 (margin 0, each wide).
    assert_eq!(
        screen.cell(0, 0).map(|c| c.contents().to_string()),
        Some("\u{4e00}".to_string()),
        "first glyph at physical col 0"
    );
    assert_eq!(
        screen.cell(0, 2).map(|c| c.contents().to_string()),
        Some("\u{4e8c}".to_string()),
        "second glyph at physical col 2 (no drift)"
    );
    assert_eq!(
        screen.cell(0, 4).map(|c| c.contents().to_string()),
        Some("\u{4e09}".to_string()),
        "third glyph at physical col 4 (no drift)"
    );
    assert_band_edge_and_gutters_blank(screen, band, outer_cols, outer_rows);

    drop(session);
}
