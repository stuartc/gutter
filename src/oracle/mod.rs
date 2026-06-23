//! The `tattoy-wezterm-term` comparison oracle and the recorded-target
//! equivalence gate (ADR-001), behind the `oracle` feature flag.
//!
//! Slice 02 wired the oracle in so it **compiles** and produces a grid
//! comparable to gutter's own vt100 grid — making the day-1 gate a flag flip,
//! not a fresh integration. Slice 08 builds the gate itself **on** that wiring:
//! [`build_terminal`] is the single wezterm-term construction site, used by both
//! the slice-02 text-alignment check ([`grid`]) and the slice-08 cell+SGR diff
//! ([`WeztermGrid`]). There is exactly one oracle, not two (the re-scope
//! checkpoint's most-likely cruft, avoided).
//!
//! Using a **different** emulator from gutter's vt100 on the bare side is
//! mandatory (ADR-001): a single shared parser would hide any sequence vt100
//! silently swallows. The gate asserts this two-emulator invariant in its
//! harness construction ([`gate::Replay`]), not just in a comment.

use std::sync::Arc;

use tattoy_wezterm_term::color::ColorPalette;
use tattoy_wezterm_term::{Terminal, TerminalConfiguration, TerminalSize};

pub mod cellview;
pub mod gate;

pub use cellview::{CellView, Color, Grid};

/// A minimal `TerminalConfiguration` for the oracle. Only `color_palette` has no
/// default on the trait; everything else uses the trait defaults, which is all
/// the grid comparison needs.
#[derive(Debug)]
struct OracleConfig;

impl TerminalConfiguration for OracleConfig {
    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }
}

/// Build a wezterm-term [`Terminal`] of `width × rows` and replay `bytes`
/// through it. **The single wezterm construction site** — both the slice-02
/// text-alignment check ([`grid`]) and the slice-08 [`WeztermGrid`] go through
/// here, so there is one oracle, not two.
#[must_use]
fn build_terminal(bytes: &[u8], width: u16, rows: u16) -> Terminal {
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
    term
}

/// Replay `bytes` through wezterm-term at `width × rows` and return the visible
/// grid as one trimmed `String` per row — aligned the same way
/// `Screen::rows(0, W)` aligns gutter's vt100 grid, so the two are diffable at
/// the text level. The slice-02 acceptance check (the gate's cell+SGR diff is
/// [`gate::gate`]).
#[must_use]
pub fn grid(bytes: &[u8], width: u16, rows: u16) -> Vec<String> {
    let term = build_terminal(bytes, width, rows);
    let screen = term.screen();
    let phys = screen.phys_range(&(0..rows as i64));
    screen
        .lines_in_phys_range(phys)
        .iter()
        .map(|line| line.as_str().trim_end().to_string())
        .collect()
}

/// The bare-side grid for the equivalence gate: a wezterm-term screen read
/// through the shared [`Grid`] trait so the gate's `align`/`diff_cells` see it
/// uniformly with gutter's vt100 grid.
///
/// Built from the same [`build_terminal`] the slice-02 [`grid`] uses — the gate
/// reuses the oracle wiring rather than standing up a second copy.
pub struct WeztermGrid {
    /// One owned wezterm `Line` per visible row (read off the settled screen).
    lines: Vec<tattoy_wezterm_term::Line>,
    cols: u16,
}

impl WeztermGrid {
    /// Replay `bytes` through the oracle at `width × rows` and snapshot the
    /// settled visible grid.
    #[must_use]
    pub fn replay(bytes: &[u8], width: u16, rows: u16) -> Self {
        let term = build_terminal(bytes, width, rows);
        let screen = term.screen();
        let phys = screen.phys_range(&(0..rows as i64));
        Self {
            lines: screen.lines_in_phys_range(phys),
            cols: width,
        }
    }
}

impl Grid for WeztermGrid {
    fn dims(&self) -> (u16, u16) {
        (self.lines.len() as u16, self.cols)
    }

    fn cell(&self, row: u16, col: u16) -> CellView {
        let Some(line) = self.lines.get(row as usize) else {
            return CellView::blank();
        };
        let Some(cell) = line.get_cell(col as usize) else {
            return CellView::blank();
        };
        let attrs = cell.attrs();
        CellView {
            contents: cellview::normalise_blank(cell.str()),
            fgcolor: wez_color(attrs.foreground()),
            bgcolor: wez_color(attrs.background()),
            bold: matches!(attrs.intensity(), tattoy_wezterm_term::Intensity::Bold),
            italic: attrs.italic(),
            underline: !matches!(
                attrs.underline(),
                tattoy_wezterm_term::Underline::None
            ),
            inverse: attrs.reverse(),
        }
    }
}

/// Project a wezterm `ColorAttribute` onto the gate's normalised [`Color`].
/// True-colour-with-fallback collapses to its RGB; a bare palette index stays
/// indexed; default stays default — the same three buckets vt100's `Color`
/// uses, so the two emulators line up.
fn wez_color(c: tattoy_wezterm_term::color::ColorAttribute) -> Color {
    use tattoy_wezterm_term::color::ColorAttribute as CA;
    match c {
        CA::Default => Color::Default,
        CA::PaletteIndex(i) => Color::Indexed(i),
        CA::TrueColorWithPaletteFallback(srgb, _) | CA::TrueColorWithDefaultFallback(srgb) => {
            let (r, g, b, _) = srgb.to_srgb_u8();
            Color::Rgb(r, g, b)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The oracle compiles, runs, and produces a grid comparable to gutter's own
    /// vt100 grid for a plain ASCII fixture (the equivalence diff itself is in
    /// `gate`; here we only prove text-level grid alignment, the slice-02 check).
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
