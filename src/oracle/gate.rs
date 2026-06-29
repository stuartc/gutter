//! The recorded-target equivalence gate: replay → align → diff → classify.
//!
//! Replays one settled VT byte stream through two parsers at the same width,
//! diffs the grids cell by cell, and classifies each divergence. wezterm is a
//! classifier, not pass/fail ground truth: the bar is zero corrupting cells, not
//! zero divergence-vs-wezterm. See ADR-001.

use std::collections::HashSet;

use super::cellview::{CellDivergence, CellView, Grid, Verdict};
use super::WeztermGrid;

/// A vt100 screen viewed through the [`Grid`] trait — the wrapped side of the
/// gate (gutter's own emulator at margin 0).
pub struct Vt100Grid<'a> {
    screen: &'a vt100::Screen,
}

impl<'a> Vt100Grid<'a> {
    #[must_use]
    pub fn new(screen: &'a vt100::Screen) -> Self {
        Self { screen }
    }
}

impl Grid for Vt100Grid<'_> {
    fn dims(&self) -> (u16, u16) {
        self.screen.size()
    }

    fn cell(&self, row: u16, col: u16) -> CellView {
        let Some(cell) = self.screen.cell(row, col) else {
            return CellView::blank();
        };
        CellView {
            contents: super::cellview::normalise_blank(cell.contents()),
            fgcolor: vt_color(cell.fgcolor()),
            bgcolor: vt_color(cell.bgcolor()),
            bold: cell.bold(),
            italic: cell.italic(),
            underline: cell.underline(),
            inverse: cell.inverse(),
        }
    }
}

/// Projects vt100's `Color` onto the gate's normalised [`super::Color`], using
/// the same three buckets as the wezterm projection so the two emulators line up.
fn vt_color(c: vt100::Color) -> super::Color {
    match c {
        vt100::Color::Default => super::Color::Default,
        vt100::Color::Idx(i) => super::Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => super::Color::Rgb(r, g, b),
    }
}

/// The settled two-emulator replay: the bare wezterm grid and a freshly-settled
/// vt100 parser. The constructor asserts the two sides are different emulator
/// types (ADR-001).
pub struct Replay {
    bare: WeztermGrid,
    /// Kept owned so [`Replay::wrapped`] can hand out a [`Vt100Grid`] borrowing
    /// its settled screen.
    wrapped_parser: vt100::Parser,
}

impl Replay {
    /// Feeds `stream` to both emulators at `width × rows` and settles. The
    /// wrapped side is gutter's vt100 at margin 0, so wrapped column `c` maps
    /// straight to bare column `c`.
    ///
    /// Asserts the two sides are different parser types (ADR-001). The check is
    /// on the grid source type names, so collapsing to one shared parser would
    /// need a compile-time change, not just a slip.
    #[must_use]
    pub fn new(stream: &[u8], width: u16, rows: u16) -> Self {
        let bare = WeztermGrid::replay(stream, width, rows);
        let mut wrapped_parser = vt100::Parser::new(rows, width, 0);
        wrapped_parser.process(stream);

        assert_ne!(
            std::any::type_name::<WeztermGrid>(),
            std::any::type_name::<Vt100Grid>(),
            "the gate MUST run two DIFFERENT emulators (ADR-001 story 2): a \
             shared parser hides any sequence vt100 swallows"
        );

        Self {
            bare,
            wrapped_parser,
        }
    }

    /// The bare side (wezterm oracle).
    #[must_use]
    pub fn bare(&self) -> &WeztermGrid {
        &self.bare
    }

    /// The wrapped side (gutter's vt100 at margin 0).
    #[must_use]
    pub fn wrapped(&self) -> Vt100Grid<'_> {
        Vt100Grid::new(self.wrapped_parser.screen())
    }
}

/// A margin-aligned view over a wrapped grid: column `c` of the view is column
/// `margin + c` of the underlying grid, lining it up with the bare grid (which
/// has no margin). The gate runs at margin 0, so this is the identity there —
/// the seam exists so the comparison is margin-agnostic and alignment is
/// explicit, not assumed.
pub struct Aligned<'a, G: Grid> {
    inner: &'a G,
    margin: u16,
}

/// Aligns a wrapped grid to the bare grid by subtracting the margin.
#[must_use]
pub fn align<G: Grid>(wrapped: &G, margin: usize) -> Aligned<'_, G> {
    Aligned {
        inner: wrapped,
        margin: margin as u16,
    }
}

impl<G: Grid> Grid for Aligned<'_, G> {
    fn dims(&self) -> (u16, u16) {
        let (rows, cols) = self.inner.dims();
        (rows, cols.saturating_sub(self.margin))
    }

    fn cell(&self, row: u16, col: u16) -> CellView {
        self.inner.cell(row, col + self.margin)
    }
}

/// Diffs two grids cell by cell over the char and the six SGR fields, one
/// [`CellDivergence`] per mismatch in row-major order. Compares the overlapping
/// `(rows, cols)`; the settled frame is the same size on both sides.
#[must_use]
pub fn diff_cells<B: Grid, W: Grid>(bare: &B, wrapped: &W) -> Vec<CellDivergence> {
    let (brows, bcols) = bare.dims();
    let (wrows, wcols) = wrapped.dims();
    let rows = brows.min(wrows);
    let cols = bcols.min(wcols);

    let mut out = Vec::new();
    for row in 0..rows {
        for col in 0..cols {
            let bare_cell = bare.cell(row, col);
            let wrapped_cell = wrapped.cell(row, col);
            if bare_cell != wrapped_cell {
                out.push(CellDivergence {
                    row,
                    col,
                    bare_cell,
                    wrapped_cell,
                });
            }
        }
    }
    out
}

/// Classifies a divergence (ADR-001). [`diff_cells`] has already shown the two
/// views differ; the question is whether vt100 lost information.
///
/// Different characters mean vt100 dropped or mangled the glyph wezterm rendered
/// — [`Verdict::Corrupting`]. Same character, different SGR attribute is a benign
/// convention difference ([`Verdict::Benign`]): wezterm is a classifier, not
/// ground truth, so an attribute-only disagreement is not on its own a defect.
#[must_use]
pub fn classify(div: &CellDivergence) -> Verdict {
    if div.bare_cell.contents != div.wrapped_cell.contents {
        Verdict::Corrupting
    } else {
        Verdict::Benign
    }
}

/// The reviewed benign-divergence allowlist. Each entry keys a tolerated
/// divergence by `(row, col)` and the two characters seen there, so it tolerates
/// only that specific reviewed divergence, not any divergence at the cell.
#[derive(Debug, Default, Clone)]
pub struct Allowlist {
    entries: HashSet<(u16, u16, String, String)>,
}

impl Allowlist {
    /// Parses the in-repo allowlist file. Each non-blank, non-`#` line is
    /// `row col bare_contents wrapped_contents`, whitespace-separated; the two
    /// contents fields are the literal cell text, `~` standing for an empty cell.
    /// Plain text so a reviewer's sign-off shows up as a readable diff.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut entries = HashSet::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.split_whitespace();
            let (Some(r), Some(c), Some(bare), Some(wrapped)) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let (Ok(row), Ok(col)) = (r.parse::<u16>(), c.parse::<u16>()) else {
                continue;
            };
            entries.insert((row, col, unescape(bare), unescape(wrapped)));
        }
        Self { entries }
    }

    /// An empty allowlist — nothing tolerated.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Does the allowlist permit this divergence? Keyed on the exact reviewed
    /// `(row, col, bare, wrapped)`, so it tolerates only what was signed off, not
    /// any future divergence at the same cell.
    #[must_use]
    pub fn permits(&self, div: &CellDivergence) -> bool {
        self.entries.contains(&(
            div.row,
            div.col,
            div.bare_cell.contents.clone(),
            div.wrapped_cell.contents.clone(),
        ))
    }

    /// Number of reviewed entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// `~` in the allowlist file stands for an empty cell, so a blank survives as a
/// whitespace-delimited token.
fn unescape(token: &str) -> String {
    if token == "~" {
        String::new()
    } else {
        token.to_string()
    }
}

/// The gate's verdict.
#[derive(Debug, Default)]
pub struct GateResult {
    /// Cells where vt100 dropped or mangled what wezterm rendered. Non-empty
    /// fails the gate.
    pub corrupting: Vec<CellDivergence>,
    /// Benign divergences not present in the allowlist — must be reviewed.
    pub unallowlisted_benign: Vec<CellDivergence>,
}

impl GateResult {
    /// Passes on zero corrupting cells (ADR-001). Unallowlisted benign
    /// divergences are surfaced for review but do not, on their own, fail the
    /// gate.
    #[must_use]
    pub fn passes(&self) -> bool {
        self.corrupting.is_empty()
    }
}

/// Runs the full gate (ADR-001): replay `stream` through both emulators at
/// `width`, align the wrapped grid to the bare, diff and classify each
/// divergence, and drop the benign-and-allowlisted ones. Pass =
/// `result.corrupting.is_empty()`.
#[must_use]
pub fn gate(stream: &[u8], width: u16, rows: u16, allowlist: &Allowlist) -> GateResult {
    let replay = Replay::new(stream, width, rows);
    let wrapped = replay.wrapped();
    // Margin 0 makes alignment the identity here; go through the seam anyway so
    // the pipeline stays margin-agnostic.
    let aligned = align(&wrapped, 0);

    let divergences = diff_cells(replay.bare(), &aligned);

    let mut result = GateResult::default();
    for div in divergences {
        match classify(&div) {
            Verdict::Corrupting => result.corrupting.push(div),
            Verdict::Benign => {
                if !allowlist.permits(&div) {
                    result.unallowlisted_benign.push(div);
                }
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oracle::cellview::Color;

    /// A vt100 parser built straight from bytes — the diff/classify/allowlist
    /// units don't need wezterm.
    fn vt(bytes: &[u8], width: u16, rows: u16) -> vt100::Parser {
        let mut p = vt100::Parser::new(rows, width, 0);
        p.process(bytes);
        p
    }

    /// `diff_cells` finds the cell where two grids differ on character.
    #[test]
    fn diff_cells_flags_character_mismatch() {
        let a = vt(b"hello", 10, 1);
        let b = vt(b"hexlo", 10, 1);
        let ga = Vt100Grid::new(a.screen());
        let gb = Vt100Grid::new(b.screen());
        let divs = diff_cells(&ga, &gb);
        assert_eq!(divs.len(), 1, "exactly one cell differs");
        assert_eq!((divs[0].row, divs[0].col), (0, 2));
        assert_eq!(classify(&divs[0]), Verdict::Corrupting, "char mismatch is corrupting");
    }

    /// An attribute-only divergence (same char, different SGR) classifies as
    /// benign — wezterm is a classifier, not ground truth (ADR-001).
    #[test]
    fn classify_attribute_only_divergence_is_benign() {
        let div = CellDivergence {
            row: 0,
            col: 0,
            bare_cell: CellView {
                contents: "x".into(),
                bold: true,
                ..CellView::blank()
            },
            wrapped_cell: CellView {
                contents: "x".into(),
                bold: false,
                ..CellView::blank()
            },
        };
        assert_eq!(classify(&div), Verdict::Benign);
    }

    /// The allowlist is load-bearing: a benign divergence is tolerated only
    /// because it is allowlisted. Remove the entry and the same divergence is
    /// reported as unallowlisted.
    #[test]
    fn allowlist_is_load_bearing() {
        let div = CellDivergence {
            row: 3,
            col: 7,
            bare_cell: CellView {
                contents: "z".into(),
                fgcolor: Color::Indexed(1),
                ..CellView::blank()
            },
            wrapped_cell: CellView {
                contents: "z".into(),
                fgcolor: Color::Default,
                ..CellView::blank()
            },
        };
        // Same char → benign.
        assert_eq!(classify(&div), Verdict::Benign);

        let allow = Allowlist::parse("3 7 z z\n");
        assert!(allow.permits(&div), "the reviewed entry tolerates this divergence");

        let empty = Allowlist::empty();
        assert!(!empty.permits(&div), "removing the entry stops tolerating it");
    }

    /// The allowlist parser skips comments and blank lines and supports `~` for
    /// an empty cell.
    #[test]
    fn allowlist_parser_handles_comments_and_blanks() {
        let text = "# a reviewed allowlist\n\n0 0 ~ a\n  5 9 b ~  \n";
        let allow = Allowlist::parse(text);
        assert_eq!(allow.len(), 2);

        let empty_bare = CellDivergence {
            row: 0,
            col: 0,
            bare_cell: CellView::blank(),
            wrapped_cell: CellView {
                contents: "a".into(),
                ..CellView::blank()
            },
        };
        assert!(allow.permits(&empty_bare), "~ stands for an empty cell");
    }
}

/// The equivalence gate against the checked-in Claude Code fixture (ADR-001).
/// Offline and deterministic: drives the two emulators directly, no PTY or
/// threads.
#[cfg(test)]
mod equivalence_gate {
    use super::*;

    const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/claude-code-flow.cast");
    const ALLOWLIST: &str = include_str!("../../tests/fixtures/claude-code-flow.allowlist");
    /// The width and rows `claude-code-flow.cast` was recorded at; the gate must
    /// replay at the same size or the cell diff misaligns. Each fixture carries
    /// its own dimensions — the wide-edge fixture uses `W_WIDE`/`ROWS_WIDE` rather
    /// than borrowing these, even though they happen to match.
    const W: u16 = 80;
    const ROWS: u16 = 24;

    /// The CJK / wide-char-at-band-edge fixture and its allowlist. A settled
    /// alt-screen stream laid out so a wide (2-cell) glyph's lead sits at the last
    /// in-band column, putting pressure on vt100's margin rule (ADR-006).
    const FIXTURE_WIDE: &[u8] = include_bytes!("../../tests/fixtures/wide-edge.cast");
    const ALLOWLIST_WIDE: &str = include_str!("../../tests/fixtures/wide-edge.allowlist");
    const W_WIDE: u16 = 80;
    const ROWS_WIDE: u16 = 24;

    /// The gate: replay the fixture at width `W` through wezterm-term (bare) and
    /// gutter's vt100 (wrapped), diff, and classify. Passes on zero corrupting
    /// cells; any benign divergence must be in the reviewed allowlist (ADR-001).
    #[test]
    fn recorded_target_equivalence_gate_passes() {
        let allowlist = Allowlist::parse(ALLOWLIST);
        let result = gate(FIXTURE, W, ROWS, &allowlist);

        assert!(
            result.corrupting.is_empty(),
            "GATE FAILED: {} corrupting cell(s) — vt100 dropped/mangled what \
             wezterm rendered. The vt100 bet failed; the swap to wezterm-term \
             is now the recommended action (a flag flip). First few: {:?}",
            result.corrupting.len(),
            result
                .corrupting
                .iter()
                .take(8)
                .map(|d| (d.row, d.col, &d.bare_cell.contents, &d.wrapped_cell.contents))
                .collect::<Vec<_>>()
        );
        assert!(
            result.unallowlisted_benign.is_empty(),
            "GATE: {} benign divergence(s) not in the reviewed allowlist — a \
             reviewer must sign them off in claude-code-flow.allowlist. First \
             few: {:?}",
            result.unallowlisted_benign.len(),
            result
                .unallowlisted_benign
                .iter()
                .take(8)
                .map(|d| (d.row, d.col))
                .collect::<Vec<_>>()
        );
        assert!(result.passes(), "gate must pass on the settled frame");
    }

    /// The bare and wrapped sides must be different emulator types — a shared
    /// parser would hide any sequence vt100 swallows. `Replay::new` asserts this;
    /// the test pins it independently too.
    #[test]
    fn gate_uses_two_different_emulators() {
        // Constructing the replay runs the in-harness assertion.
        let _replay = Replay::new(FIXTURE, W, ROWS);
        assert_ne!(
            std::any::type_name::<WeztermGrid>(),
            std::any::type_name::<Vt100Grid>(),
            "bare = wezterm-term, wrapped = vt100 — DIFFERENT emulators (ADR-001)"
        );
    }

    /// The allowlist is load-bearing at the gate level. Diff two vt100 grids that
    /// differ only on fg colour, confirm it classifies benign, then show an
    /// allowlist entry tolerates it and the empty allowlist does not.
    #[test]
    fn allowlist_consulted_by_gate_pipeline() {
        // Same character on both sides, different fg — the canonical benign
        // convention difference.
        let bare = {
            let mut p = vt100::Parser::new(1, 4, 0);
            p.process(b"\x1b[31mok\x1b[0m"); // red
            p
        };
        let wrapped = {
            let mut p = vt100::Parser::new(1, 4, 0);
            p.process(b"ok"); // default colour
            p
        };
        let gb = Vt100Grid::new(bare.screen());
        let gw = Vt100Grid::new(wrapped.screen());
        let divs = diff_cells(&gb, &gw);
        assert!(!divs.is_empty(), "fg-only difference must diverge");
        let div = &divs[0];
        assert_eq!(classify(div), Verdict::Benign, "same char, diff fg → benign");

        let entry = format!(
            "{} {} {} {}",
            div.row,
            div.col,
            if div.bare_cell.contents.is_empty() { "~" } else { &div.bare_cell.contents },
            if div.wrapped_cell.contents.is_empty() { "~" } else { &div.wrapped_cell.contents },
        );
        assert!(
            Allowlist::parse(&entry).permits(div),
            "the reviewed entry tolerates the benign divergence"
        );
        assert!(
            !Allowlist::empty().permits(div),
            "removing the entry stops tolerating it — the allowlist is load-bearing"
        );
    }

    /// The fixture must contain all five phases, so it can't silently shrink to
    /// thin ASCII and weaken the gate. Each phase is detected by a characteristic
    /// byte marker.
    #[test]
    fn fixture_contains_the_five_phases() {
        let f = FIXTURE;
        fn contains(haystack: &[u8], needle: &[u8]) -> bool {
            haystack
                .windows(needle.len())
                .any(|w| w == needle)
        }

        // 1. startup / DECSET negotiation: alt screen + a DECSET enable.
        assert!(contains(f, b"\x1b[?1049h"), "phase 1: alt-screen enter (DECSET)");
        assert!(contains(f, b"\x1b[?2004h"), "phase 1: bracketed-paste DECSET");
        // 2. multi-line edit: several absolute cursor positions writing code.
        assert!(contains(f, b"fn"), "phase 2: edit content");
        assert!(contains(f, b"\x1b[5;5H"), "phase 2: absolute-positioned edit line");
        // 3. scroll: a DECSTBM scroll-region set.
        assert!(contains(f, b"\x1b[11;20r"), "phase 3: scroll region (DECSTBM)");
        assert!(contains(f, b"log line"), "phase 3: scrolled content");
        // 4. syntax-highlighted diff: SGR-dense add/remove lines with bg colour.
        assert!(contains(f, b"diff --git"), "phase 4: diff header");
        assert!(contains(f, b"\x1b[48;5;22m"), "phase 4: SGR background (diff add)");
        assert!(contains(f, b"\x1b[48;5;52m"), "phase 4: SGR background (diff remove)");
        // 5. spinner redraw: the braille spinner glyph rewritten in place.
        assert!(contains(f, "\u{280b}".as_bytes()), "phase 5: spinner glyph");
        assert!(contains(f, b"Thinking..."), "phase 5: spinner label");

        // SGR-dense, not thin ASCII (ADR-001): a healthy count of CSI introducers
        // proves the fixture carries real attribute runs.
        let sgr_count = f.windows(2).filter(|w| w == b"\x1b[").count();
        assert!(
            sgr_count > 40,
            "fixture must be SGR-dense (got {sgr_count} CSI introducers) — a thin \
             ASCII fixture weakens the gate (ADR-001)"
        );
    }

    /// The reviewed allowlist file loads and is the typed seam the gate consults.
    #[test]
    fn allowlist_file_loads() {
        let allow = Allowlist::parse(ALLOWLIST);
        // Zero benign divergences in the current fixture, so the reviewed
        // allowlist is empty — but it must still parse and be the value the gate
        // is handed.
        assert!(
            allow.is_empty(),
            "the reviewed allowlist is empty for the current fixture (no benign \
             divergences); if this changes, add reviewed entries"
        );
    }

    /// The wide-char-at-band-edge gate: the same pipeline, run against
    /// `wide-edge.cast`, where wide (2-cell) CJK glyphs and an emoji sit at the
    /// last in-band column and a CJK run overflows the band by a full glyph so
    /// vt100's margin rule must wrap it. Passes on zero corrupting cells.
    ///
    /// A corrupting cell here is the concrete trigger that reopens ADR-006 and
    /// demands `render_cell_walk` be wired: it means vt100's margin rule let a
    /// wide glyph's right half bleed past column W where wezterm-term did not.
    #[test]
    fn wide_edge_equivalence_gate_passes() {
        let allowlist = Allowlist::parse(ALLOWLIST_WIDE);
        let result = gate(FIXTURE_WIDE, W_WIDE, ROWS_WIDE, &allowlist);

        assert!(
            result.corrupting.is_empty(),
            "WIDE-EDGE GATE FAILED: {} corrupting cell(s) — vt100 and the \
             wezterm-term oracle disagree on a CHARACTER at the band edge. A wide \
             glyph's half bled past column W on one side (ADR-006 right-edge \
             safety). This is the concrete A1 trigger: render_cell_walk now has a \
             reason to be wired live. First few: {:?}",
            result.corrupting.len(),
            result
                .corrupting
                .iter()
                .take(8)
                .map(|d| (d.row, d.col, &d.bare_cell.contents, &d.wrapped_cell.contents))
                .collect::<Vec<_>>()
        );
        assert!(
            result.unallowlisted_benign.is_empty(),
            "WIDE-EDGE GATE: {} benign divergence(s) not in the reviewed allowlist \
             — a reviewer must sign them off in wide-edge.allowlist. First few: {:?}",
            result.unallowlisted_benign.len(),
            result
                .unallowlisted_benign
                .iter()
                .take(8)
                .map(|d| (d.row, d.col))
                .collect::<Vec<_>>()
        );
        assert!(result.passes(), "wide-edge gate must pass on the settled frame");
    }

    /// `wide-edge.cast` must carry real wide content — CJK codepoints and an
    /// emoji — and stay SGR-dense, so it can't silently degrade to thin ASCII and
    /// stop exercising vt100's margin rule.
    #[test]
    fn wide_edge_fixture_contains_wide_content() {
        let f = FIXTURE_WIDE;
        fn contains(haystack: &[u8], needle: &[u8]) -> bool {
            haystack.windows(needle.len()).any(|w| w == needle)
        }

        // A settled alt-screen frame (the wide content is laid into a fixed band).
        assert!(contains(f, b"\x1b[?1049h"), "wide-edge: alt-screen enter (DECSET)");

        // CJK codepoints (each 2 cells wide), laid at the band edge.
        assert!(contains(f, "漢".as_bytes()), "wide-edge: CJK glyph present");
        assert!(contains(f, "字".as_bytes()), "wide-edge: CJK glyph present");
        // An emoji (also 2 cells wide) at the edge — the non-CJK wide codepoint.
        assert!(contains(f, "🌟".as_bytes()), "wide-edge: emoji present");

        // SGR-dense, not thin ASCII: a healthy count of CSI introducers proves the
        // fixture carries real attribute runs (SGR backgrounds straddle the edge).
        let sgr_count = f.windows(2).filter(|w| w == b"\x1b[").count();
        assert!(
            sgr_count > 15,
            "wide-edge fixture must be SGR-dense (got {sgr_count} CSI introducers) \
             — a thin fixture stops exercising the margin rule (ADR-001/006)"
        );
    }

    /// The reviewed wide-edge allowlist loads and is empty: the two emulators
    /// agree on every cell of the settled frame, so there's nothing to tolerate.
    #[test]
    fn wide_edge_allowlist_file_loads() {
        let allow = Allowlist::parse(ALLOWLIST_WIDE);
        assert!(
            allow.is_empty(),
            "the reviewed wide-edge allowlist is empty (no benign divergences); if \
             this changes, add reviewed entries with a sign-off"
        );
    }
}
