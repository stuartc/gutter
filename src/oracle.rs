//! The `tattoy-wezterm-term` comparison oracle (ADR-001), behind the `oracle`
//! feature flag.
//!
//! In slice 02 this only has to **compile** and produce a grid **comparable** to
//! gutter's own vt100 grid, so the day-1 equivalence gate (slice 08) is a flag
//! flip, not a fresh integration. The actual two-emulator cell+SGR diff against
//! the checked-in Claude Code fixture is slice 08; here the oracle just replays a
//! byte stream at width `W` and exposes the resulting per-row text, aligned the
//! same way gutter's grid is (one `String` per visible row).
//!
//! Using a **different** emulator from gutter's vt100 on the bare side is
//! mandatory (ADR-001): a single shared parser would hide any sequence vt100
//! silently swallows.

use std::sync::Arc;

use tattoy_wezterm_term::color::ColorPalette;
use tattoy_wezterm_term::{Terminal, TerminalConfiguration, TerminalSize};

/// A minimal `TerminalConfiguration` for the oracle. Only `color_palette` has no
/// default on the trait; everything else uses the trait defaults, which is all
/// the grid-text comparison needs.
///
/// Constructed only by [`grid`] (and its test). The slice-08 equivalence gate is
/// the production caller; in this slice the oracle just has to compile and
/// produce a comparable grid, so it has no shipping call site yet.
#[derive(Debug)]
#[allow(dead_code)]
struct OracleConfig;

impl TerminalConfiguration for OracleConfig {
    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }
}

/// Replay `bytes` through wezterm-term at `width × rows` and return the visible
/// grid as one trimmed `String` per row — aligned the same way
/// `Screen::rows(0, W)` aligns gutter's vt100 grid, so the two are diffable.
///
/// Exercised by this module's test in slice 02 (proving the oracle compiles and
/// aligns); the slice-08 equivalence gate is the production caller.
#[must_use]
#[allow(dead_code)]
pub fn grid(bytes: &[u8], width: u16, rows: u16) -> Vec<String> {
    let size = TerminalSize {
        rows: rows as usize,
        cols: width as usize,
        pixel_width: 0,
        pixel_height: 0,
        dpi: 0,
    };
    let mut term = Terminal::new(
        size,
        Arc::new(OracleConfig),
        "gutter-oracle",
        "0",
        Box::new(std::io::sink()),
    );
    term.advance_bytes(bytes);

    let screen = term.screen();
    let phys = screen.phys_range(&(0..rows as i64));
    screen
        .lines_in_phys_range(phys)
        .iter()
        .map(|line| line.as_str().trim_end().to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The oracle compiles, runs, and produces a grid comparable to gutter's own
    /// vt100 grid for a plain ASCII fixture (the equivalence diff itself is
    /// slice 08; here we only prove grid alignment).
    #[test]
    fn oracle_grid_matches_vt100_for_ascii() {
        let bytes = b"hello\r\nworld\r\nthird line";
        let width = 20;
        let rows = 5;

        let oracle_rows = grid(bytes, width, rows);

        let mut p: vt100::Parser = vt100::Parser::new(rows, width, 0);
        p.process(bytes);
        let vt_rows: Vec<String> = p
            .screen()
            .rows(0, width)
            .map(|r| r.trim_end().to_string())
            .collect();

        assert_eq!(oracle_rows, vt_rows, "oracle and vt100 grids must align");
    }
}
