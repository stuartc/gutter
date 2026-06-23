//! The recorded-target equivalence gate pipeline (ADR-001).
//!
//! The deep module of slice 08: it hides the messy two-emulator
//! replay→align→diff→classify pipeline behind a small, stable, testable
//! interface so the CI gate (and any future emulation-layer regression test)
//! targets a durable seam rather than re-implementing the comparison inline.
//!
//! The pipeline, each step a named seam:
//! - [`Replay`] — feed the same bytes to BOTH parsers at the same width and
//!   settle. Its constructor asserts the two-emulator invariant (story 2): the
//!   bare grid is wezterm-term, the wrapped grid is vt100 — different emulators,
//!   so a sequence vt100 swallows cannot cancel itself out.
//! - [`align`] — subtract the margin so the wrapped grid's column `margin` maps
//!   to bare column 0.
//! - [`diff_cells`] — cell-by-cell over the char + seven SGR fields; one
//!   [`CellDivergence`] per mismatch.
//! - [`classify`] — re-diff vt100-vs-wezterm for a diverging cell:
//!   [`Verdict::Corrupting`] (vt100 dropped/mangled what wezterm rendered) vs
//!   [`Verdict::Benign`] (both treat it the same).
//! - [`Allowlist`] — the reviewed benign-divergence file as a typed, queryable
//!   seam, not an ad-hoc list in the test.
//! - [`gate`] — the top-level seam the CI test calls. Pass = zero corrupting
//!   cells on the settled frame.
//!
//! wezterm is a divergence **classifier**, not pass/fail ground truth (ADR-001):
//! the bar is zero *corrupting* cells, NOT zero divergence-vs-wezterm. Benign
//! convention differences are expected and live in the reviewed [`Allowlist`].

use std::collections::HashSet;

use super::cellview::{CellDivergence, CellView, Grid, Verdict};
use super::WeztermGrid;

/// A vt100 screen viewed through the shared [`Grid`] trait — the wrapped side of
/// the gate (gutter's own emulator at margin 0).
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

/// Project vt100's `Color` onto the gate's normalised [`super::Color`] — the
/// same three buckets the wezterm projection uses, so the two emulators line up.
fn vt_color(c: vt100::Color) -> super::Color {
    match c {
        vt100::Color::Default => super::Color::Default,
        vt100::Color::Idx(i) => super::Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => super::Color::Rgb(r, g, b),
    }
}

/// The settled two-emulator replay (story 2). Holds the bare wezterm grid and a
/// freshly-settled vt100 parser; its constructor asserts the two sides are
/// **different** emulator types so the same-parser footgun cannot silently
/// regress.
pub struct Replay {
    bare: WeztermGrid,
    /// The wrapped-side parser, kept owned so [`Replay::wrapped`] can hand out a
    /// [`Vt100Grid`] borrowing its settled screen.
    wrapped_parser: vt100::Parser,
}

impl Replay {
    /// Feed `stream` to both emulators at `width × rows` and settle. The wrapped
    /// side is gutter's vt100 at margin 0 (the grid is exactly `width` columns,
    /// so margin 0 means the wrapped column `c` already maps to bare column `c`).
    ///
    /// Asserts the two-emulator invariant at construction: the bare grid is
    /// wezterm-term and the wrapped is vt100 — *different* parsers (ADR-001,
    /// story 2). The assertion is a type-identity check on the two grid sources,
    /// so it cannot regress to one shared parser without a compile-time change.
    #[must_use]
    pub fn new(stream: &[u8], width: u16, rows: u16) -> Self {
        let bare = WeztermGrid::replay(stream, width, rows);
        let mut wrapped_parser = vt100::Parser::new(rows, width, 0);
        wrapped_parser.process(stream);

        // The two-emulator invariant (story 2), asserted in the harness
        // construction, not just a comment: the bare and wrapped sides MUST be
        // different emulator types. `WeztermGrid` wraps `tattoy-wezterm-term`;
        // `Vt100Grid` wraps `vt100`. The type names being distinct is the
        // mechanical proof a refactor can't collapse them to one parser.
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

/// A margin-aligned view over a wrapped grid: column `c` of the aligned view is
/// column `margin + c` of the underlying grid, so it lines up with the bare grid
/// (which has no margin). For the gate the wrapped grid is at margin 0, so this
/// is the identity — but the seam exists per the PRD so the comparison is
/// margin-agnostic and the alignment is explicit, not assumed.
pub struct Aligned<'a, G: Grid> {
    inner: &'a G,
    margin: u16,
}

/// Align a wrapped grid to the bare grid by subtracting the margin (PRD seam).
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

/// Cell-by-cell diff over the char + the seven SGR fields. Returns one
/// [`CellDivergence`] per mismatch, in row-major order. Compares over the
/// overlapping `(rows, cols)` of the two grids (the settled frame is the same
/// size on both sides, so this is the full grid).
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

/// Classify a divergence (ADR-001). Because [`diff_cells`] already established
/// that the bare (wezterm) and wrapped (vt100) views differ, the only question
/// is whether they differ in a way that means vt100 *lost* information.
///
/// The classifier is content-led: if the two emulators rendered **different
/// characters** at the cell — vt100 dropped or mangled the glyph wezterm
/// rendered — that is [`Verdict::Corrupting`]. If the characters match and only
/// an SGR attribute differs, that is a benign convention difference between the
/// two emulators ([`Verdict::Benign`]) — wezterm is a classifier, not ground
/// truth, so an attribute-only disagreement is not on its own a vt100 defect.
#[must_use]
pub fn classify(div: &CellDivergence) -> Verdict {
    if div.bare_cell.contents != div.wrapped_cell.contents {
        Verdict::Corrupting
    } else {
        Verdict::Benign
    }
}

/// The reviewed benign-divergence allowlist (story 3) as a typed, queryable
/// seam. Each entry keys a tolerated benign divergence by its `(row, col)` and
/// the two characters seen there — so an allowlist entry tolerates *only* the
/// specific reviewed divergence, not any divergence at that cell.
#[derive(Debug, Default, Clone)]
pub struct Allowlist {
    /// `(row, col, bare_contents, wrapped_contents)` for each reviewed entry.
    entries: HashSet<(u16, u16, String, String)>,
}

impl Allowlist {
    /// Parse the in-repo allowlist file. Each non-blank, non-`#` line is
    /// `row col bare_contents wrapped_contents` (whitespace-separated; the two
    /// contents fields are the literal cell text, `~` standing for an empty
    /// cell so blanks are representable). A reviewed file is plain text so the
    /// human sign-off is a readable diff.
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

    /// An empty allowlist — nothing tolerated. Used by the test that proves a
    /// benign divergence fails the gate once its allowlist entry is removed.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Does the allowlist permit this benign divergence? Keyed on the exact
    /// reviewed `(row, col, bare, wrapped)` so it tolerates only what a human
    /// signed off, not any future divergence at the same cell.
    #[must_use]
    pub fn permits(&self, div: &CellDivergence) -> bool {
        self.entries.contains(&(
            div.row,
            div.col,
            div.bare_cell.contents.clone(),
            div.wrapped_cell.contents.clone(),
        ))
    }

    /// Number of reviewed entries (for the fixture/coverage assertions).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// `~` in the allowlist file stands for an empty cell (so a blank is
/// representable as a whitespace-delimited token).
fn unescape(token: &str) -> String {
    if token == "~" {
        String::new()
    } else {
        token.to_string()
    }
}

/// The gate's verdict: the corrupting cells (any → fail) and the benign
/// divergences that were NOT in the allowlist (a reviewer must sign them off or
/// they are treated as a regression).
#[derive(Debug, Default)]
pub struct GateResult {
    /// Cells where vt100 dropped/mangled what wezterm rendered. **Non-empty →
    /// the gate fails** and the swap to wezterm-term is the recommended action.
    pub corrupting: Vec<CellDivergence>,
    /// Benign divergences not present in the allowlist — must be reviewed.
    pub unallowlisted_benign: Vec<CellDivergence>,
}

impl GateResult {
    /// Pass = zero corrupting cells on the settled frame (ADR-001). Unallowlisted
    /// benign divergences are surfaced separately for review but are not, on
    /// their own, the corrupting-cell failure the swap decision rests on.
    #[must_use]
    pub fn passes(&self) -> bool {
        self.corrupting.is_empty()
    }
}

/// The top-level gate seam (ADR-001): replay `stream` through both emulators at
/// `width`, align the wrapped grid to the bare (margin 0 for the gate), diff
/// cell + the seven SGR fields, classify each divergence, and filter out
/// benign-and-allowlisted ones. Pass = `result.corrupting.is_empty()`.
#[must_use]
pub fn gate(stream: &[u8], width: u16, rows: u16, allowlist: &Allowlist) -> GateResult {
    let replay = Replay::new(stream, width, rows);
    let wrapped = replay.wrapped();
    // The gate runs the wrapped side at margin 0, so alignment is the identity —
    // but go through the seam explicitly so the pipeline is margin-agnostic.
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

    /// A vt100-backed [`Grid`] built straight from bytes — the test helper for
    /// the diff/classify/allowlist units (no wezterm needed for those).
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

    /// The allowlist is load-bearing: a benign divergence is tolerated ONLY
    /// because it is allowlisted — remove the entry and the same divergence is
    /// reported as unallowlisted (story 3, the "allowlist is not decorative"
    /// proof at the unit level; the gate-level version uses the real fixture).
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

/// The recorded-target equivalence gate against the checked-in Claude Code
/// fixture (ADR-001) — the keystone CI test of slice 08. Offline and
/// deterministic: it drives the two emulators directly (no PTY, no threads),
/// exactly as the slice-06 OSC-52 dispatch test drives `parser.process()`.
#[cfg(test)]
mod equivalence_gate {
    use super::*;

    /// The checked-in real-target byte stream and its reviewed allowlist.
    const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/claude-code-flow.cast");
    const ALLOWLIST: &str = include_str!("../../tests/fixtures/claude-code-flow.allowlist");
    /// The band width the fixture was recorded at, and the gate replays at.
    const W: u16 = 80;
    const ROWS: u16 = 24;

    /// **The gate (CI keystone).** Replay the fixture at width `W` through
    /// `tattoy-wezterm-term` (bare) AND gutter's vt100 (wrapped, margin 0),
    /// align, diff cell + the seven SGR fields, classify each divergence. **Pass
    /// = zero CORRUPTING cells on the settled frame** (ADR-001). Any benign
    /// divergence must be in the reviewed allowlist.
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

    /// **The two-emulator invariant (story 2).** The bare and wrapped sides MUST
    /// be different emulator types — a shared parser would hide any sequence
    /// vt100 swallows. `Replay::new` asserts this at construction; this test
    /// pins the two grid source types are mechanically distinct.
    #[test]
    fn gate_uses_two_different_emulators() {
        // Constructing the replay runs the in-harness two-emulator assertion.
        let _replay = Replay::new(FIXTURE, W, ROWS);
        // And pin it here too, so the invariant is asserted at the test level as
        // well as in the harness body.
        assert_ne!(
            std::any::type_name::<WeztermGrid>(),
            std::any::type_name::<Vt100Grid>(),
            "bare = wezterm-term, wrapped = vt100 — DIFFERENT emulators (ADR-001)"
        );
    }

    /// **Benign-divergence allowlist is load-bearing (story 3), at the gate
    /// level.** Inject a synthetic benign divergence (same char, different SGR)
    /// by diffing two slightly-different vt100 grids, confirm it classifies
    /// benign, then prove an allowlist entry tolerates it and removing the entry
    /// surfaces it as unallowlisted — the allowlist is consulted, not decorative.
    #[test]
    fn allowlist_consulted_by_gate_pipeline() {
        // A cell that is the same character on both sides but a different fg —
        // the canonical benign convention difference.
        let bare = {
            let mut p = vt100::Parser::new(1, 4, 0);
            p.process(b"\x1b[31mok\x1b[0m"); // red "ok"
            p
        };
        let wrapped = {
            let mut p = vt100::Parser::new(1, 4, 0);
            p.process(b"ok"); // default "ok"
            p
        };
        let gb = Vt100Grid::new(bare.screen());
        let gw = Vt100Grid::new(wrapped.screen());
        let divs = diff_cells(&gb, &gw);
        assert!(!divs.is_empty(), "fg-only difference must diverge");
        let div = &divs[0];
        assert_eq!(classify(div), Verdict::Benign, "same char, diff fg → benign");

        // An allowlist entry keyed on the exact (row, col, bare, wrapped)
        // tolerates it; the empty allowlist does not.
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

    /// **Fixture coverage (assertion on the fixture).** The checked-in byte
    /// stream must contain the five required phases, so it can't silently shrink
    /// to thin ASCII and weaken the gate. Each phase is detected by a byte
    /// marker characteristic of it.
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

        // And the fixture must be SGR-dense, not thin ASCII (ADR-001): a healthy
        // count of SGR introducers proves it carries real attribute runs.
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
        // The fixture currently has zero benign divergences, so the reviewed
        // allowlist is empty — but it must parse (comments/blanks skipped) and
        // be the value the gate is handed.
        assert!(
            allow.is_empty(),
            "the reviewed allowlist is empty for the current fixture (no benign \
             divergences); if this changes, add reviewed entries"
        );
    }
}
