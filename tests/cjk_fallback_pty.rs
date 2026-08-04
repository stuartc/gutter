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

use std::time::Duration;

mod common;
use common::{assert_cols_blank, drain_window, outer_grid, spawn_gutter};

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
    let mut session =
        spawn_gutter(outer_cols, outer_rows, &format!("--width {band} --left {child}"));
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
    assert_cols_blank(screen, band, outer_cols, outer_rows);

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
    let mut session =
        spawn_gutter(outer_cols, outer_rows, &format!("--width {band} --left {child}"));
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
    assert_cols_blank(screen, band, outer_cols, outer_rows);

    drop(session);
}
