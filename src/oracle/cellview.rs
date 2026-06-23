//! The comparison surface for the equivalence gate (ADR-001).
//!
//! [`CellView`] is *exactly* the gate's contract — the character plus the seven
//! SGR fields the PRD names (`contents`, `fgcolor`, `bgcolor`, `bold`, `italic`,
//! `underline`, `inverse`) and nothing else. Both emulators' native cell types
//! project onto it, so [`crate::oracle::gate`]'s diff code never branches on
//! which emulator a cell came from.
//!
//! [`Grid`] is the uniform view `align`/`diff_cells` read through: a `cell(row,
//! col) -> CellView` plus `dims()`. gutter's `vt100::Screen` and the
//! wezterm-term [`crate::oracle::WeztermGrid`] both implement it, which is what
//! lets the gate hold two *different* emulators behind one interface (story 2 —
//! the two-emulator invariant the harness asserts at construction).

/// A normalised colour, the common denominator of vt100's `Color` and
/// wezterm's `ColorAttribute`. The gate compares colour by this projection, so
/// the two emulators' different native representations of the *same* colour
/// (e.g. a default vs an explicit palette-0) line up — or, when they genuinely
/// differ, surface as a divergence to classify.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    /// The terminal default (no explicit colour).
    Default,
    /// A palette index (0..=255), the 16-colour and 256-colour space.
    Indexed(u8),
    /// A 24-bit true colour.
    Rgb(u8, u8, u8),
}

/// The gate's per-cell comparison surface: the character plus the seven SGR
/// fields, normalised so the two emulators are diffable. Nothing else — the
/// comparison surface IS the gate's contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellView {
    /// The cell's glyph(s). Empty for a blank cell and for a wide glyph's
    /// continuation half (so a continuation never diverges against a blank).
    pub contents: String,
    pub fgcolor: Color,
    pub bgcolor: Color,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
}

/// Normalise a cell's raw contents so a blank reads the same on both emulators.
///
/// vt100 reports an untouched cell as a single space `" "`; wezterm reports it
/// as `""`. Both mean "blank", so they must not diverge. Collapsing `" "` to
/// `""` on **both** sides aligns blanks; a genuine space still compares equal
/// (both sides collapse it identically), so no real content is lost.
#[must_use]
pub fn normalise_blank(raw: &str) -> String {
    if raw == " " {
        String::new()
    } else {
        raw.to_string()
    }
}

impl CellView {
    /// A blank default cell — the value both emulators report for an untouched
    /// cell, so an unwritten region never spuriously diverges.
    #[must_use]
    pub fn blank() -> Self {
        Self {
            contents: String::new(),
            fgcolor: Color::Default,
            bgcolor: Color::Default,
            bold: false,
            italic: false,
            underline: false,
            inverse: false,
        }
    }
}

/// A uniform view over an emulator's settled grid: read any cell as a
/// [`CellView`], and ask the grid's dimensions. The gate's `align` /
/// `diff_cells` read only through this, so they never branch on the emulator.
pub trait Grid {
    /// The settled grid's `(rows, cols)`.
    fn dims(&self) -> (u16, u16);
    /// The normalised view of cell `(row, col)`. Out-of-range cells read as
    /// [`CellView::blank`] so alignment never panics on the band-edge.
    fn cell(&self, row: u16, col: u16) -> CellView;
}

/// One diverging cell: the bare (wezterm) and wrapped (vt100) views at the same
/// aligned `(row, col)`. The gate classifies each into a [`Verdict`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellDivergence {
    pub row: u16,
    pub col: u16,
    /// The bare side (wezterm oracle).
    pub bare_cell: CellView,
    /// The wrapped side (gutter's vt100 at margin 0).
    pub wrapped_cell: CellView,
}

/// The classification of a divergence (ADR-001).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// vt100 dropped or mangled what wezterm rendered — the `vt100` bet failed
    /// for this cell. One or more of these fails the gate.
    Corrupting,
    /// Both emulators represent the cell the same way; a benign convention
    /// difference, tolerated only when allowlisted.
    Benign,
}
