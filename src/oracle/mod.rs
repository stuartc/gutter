//! The wezterm-term comparison oracle behind the `oracle` feature flag. The
//! equivalence gate replays a settled byte stream through wezterm-term and diffs
//! its grid against gutter's own vt100 grid. See ADR-001.
//!
//! The bare side uses a different emulator on purpose: a shared parser would
//! hide any sequence vt100 silently swallows.

use std::sync::Arc;

use tattoy_wezterm_term::color::ColorPalette;
use tattoy_wezterm_term::{Terminal, TerminalConfiguration, TerminalSize};

pub mod cellview;
pub mod gate;

use cellview::{CellView, Color, Grid};

/// Minimal `TerminalConfiguration` for the oracle. `color_palette` is the only
/// trait method without a default; the trait defaults cover everything the grid
/// comparison needs.
#[derive(Debug)]
struct OracleConfig;

impl TerminalConfiguration for OracleConfig {
    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }
}

/// Builds a wezterm-term [`Terminal`] of `width × rows` and replays `bytes`
/// through it.
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

/// The bare-side grid for the equivalence gate: a wezterm-term screen exposed
/// through the shared [`Grid`] trait, so the gate diffs it uniformly against
/// gutter's vt100 grid.
pub struct WeztermGrid {
    /// One owned wezterm `Line` per visible row, read off the settled screen.
    lines: Vec<tattoy_wezterm_term::Line>,
    cols: u16,
}

impl WeztermGrid {
    /// Replays `bytes` through the oracle at `width × rows` and snapshots the
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

/// Maps a wezterm `ColorAttribute` onto the gate's normalised [`Color`].
/// True-colour-with-fallback collapses to its RGB; a palette index stays
/// indexed; default stays default — the same three buckets vt100's `Color` uses,
/// so the two emulators line up.
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