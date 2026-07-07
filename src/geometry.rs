//! Pure layout maths: band width and left margin. No I/O, so it is
//! property-testable directly. The render path and resize handler call in here
//! rather than computing offsets themselves.
//!
//! See ADR-011 for width resolution and ADR-006 for the band-fit margin rule.

/// Minimum band width. A proportional band on a tiny terminal floors here rather
/// than collapsing toward zero — but capped at `real_cols`, so on a terminal
/// narrower than `MIN_W` the band caps at the terminal width.
pub const MIN_W: u16 = 20;

/// The band's horizontal alignment, selected by `--center` / `--left`. The
/// offset each produces is computed by [`margin`], not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layout {
    /// Centre the band: equal gutters either side, slack rounded down.
    #[default]
    Center,
    /// Left-align the band: gutter on the right only (margin 0).
    Left,
}

/// The requested band width from `--width`, before [`resolve_width`] resolves it
/// against the real terminal. See ADR-011.
///
/// - `Cols(n)` — absolute: exactly `n` columns, fixed for the session.
/// - `Percent(p)` — proportional: `p`% of `real_cols`, recomputed on resize.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Width {
    Cols(u16),
    Percent(u8),
}

/// The band's left margin for the given layout, real terminal width and band
/// width `W`. See ADR-011.
///
/// `Center` is half the slack, rounded down; `saturating_sub` pins the slack to
/// zero when `W >= real_cols`, so the margin never wraps negative. `Left` is `0`.
#[must_use]
pub fn margin(layout: Layout, real_cols: u16, width: u16) -> u16 {
    match layout {
        Layout::Center => real_cols.saturating_sub(width) / 2,
        Layout::Left => 0,
    }
}

/// The physical column an in-band column `col` maps to, given the band's left
/// margin. Saturating, so an out-of-range column can never wrap.
#[must_use]
pub fn physical_col(left_margin: u16, col: u16) -> u16 {
    left_margin.saturating_add(col)
}

/// Resolve the effective band width `W` from the requested [`Width`] against the
/// current real terminal width. See ADR-011.
///
/// `Cols(n)` is the identity on `n`, clamped to `[1, real_cols]`. `Percent(p)` is
/// `real_cols * p / 100`, floored at [`MIN_W`] and capped at `real_cols` — so a
/// tiny percentage or terminal never collapses the band to `0`.
#[must_use]
pub fn resolve_width(width: Width, real_cols: u16) -> u16 {
    let real_cols = real_cols.max(1);
    match width {
        Width::Cols(n) => n.max(1).min(real_cols),
        Width::Percent(p) => {
            let raw = (real_cols as u32 * p as u32 / 100) as u16;
            // Floor at MIN_W, but never exceed the terminal: on a terminal
            // narrower than MIN_W the floor itself is capped to real_cols.
            raw.max(MIN_W).min(real_cols)
        }
    }
}

/// Apply one resize-mode nudge to a [`Width`], preserving its unit and clamping to
/// the manual-resize bounds. `delta` is in the width's own unit — columns for
/// `Cols`, percent for `Percent` — signed (negative shrinks). See PRD 0001,
/// Feature 2 ("Step unit follows the width's own unit and never converts").
///
/// - `Cols(n)` steps from the **effective** width `n.min(real_cols)` (a config
///   wider than the terminal steps from the terminal edge, not the stored figure,
///   so the first shrink press moves the band). The floor is `MIN_W` (20), but
///   never above `real_cols` (a tiny terminal caps the band at the terminal width,
///   never inverts the clamp range) and never above the current width — note
///   `resolve_width(Cols)` has NO `MIN_W` floor (above), so `--width 10` is a
///   legal 10-column band; a shrink press on it must be a silent no-op, NOT snap
///   the band UP to 20.
/// - `Percent(p)` → `Percent(clamp(p + delta, 1, 100))`. The resolved column
///   count is floored at `MIN_W` and capped at `real_cols` by [`resolve_width`],
///   so a `1%` band on a narrow terminal may not visibly move (the accepted
///   sub-column wart); `H`/`L` (±10) always moves.
#[must_use]
pub fn step_width(width: Width, delta: i32, real_cols: u16) -> Width {
    match width {
        Width::Cols(n) => {
            let real = real_cols.max(1) as i32;
            // Step from the effective width, not the raw config.
            let cur = (n.max(1) as i32).min(real);
            // Floor at MIN_W, but a band already below it (or a terminal narrower
            // than it) only pins where it is — shrinking never grows the band.
            let lo = (MIN_W as i32).min(real).min(cur);
            Width::Cols((cur + delta).clamp(lo, real) as u16)
        }
        Width::Percent(p) => {
            let v = (p as i32 + delta).clamp(1, 100);
            Width::Percent(v as u8)
        }
    }
}

/// Where the resize rails and readout land, computed from the band geometry.
/// Pure data handed to `OuterTerminal::draw_rails`; the terminal only emits it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rails {
    /// Physical column for the left `▏` rail, or `None` where there is no left
    /// gutter (full width, or a `--left` band).
    pub left_col: Option<u16>,
    /// Physical column for the right `▕` rail, or `None` where the band reaches
    /// the terminal's right edge.
    pub right_col: Option<u16>,
    /// Physical row span the rails cover: `[row_start, row_end)`.
    pub row_start: u16,
    pub row_end: u16,
    /// The width readout, or `None` when there are no rows to draw into.
    pub readout: Option<Readout>,
}

/// The dim width readout's placement and text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Readout {
    pub col: u16,
    pub row: u16,
    pub text: String,
}

/// The readout text: a bare column count for an absolute width, a `%`-suffixed
/// percentage for a proportional one. `width` is the resolved (clamped) `W`, so the
/// absolute readout shows what is actually on screen, not the pre-clamp request.
#[must_use]
pub fn readout_text(width_config: Width, width: u16) -> String {
    match width_config {
        Width::Cols(_) => format!("{width}"),
        Width::Percent(p) => format!("{p}%"),
    }
}

/// Lay out the rails + readout for a band of width `W` at `margin`, spanning physical
/// rows `[offset, rows)` of a `real_cols`-wide terminal. `offset` is 0 on the alt
/// screen and `base_row` on the primary screen, so the rails never reach into shell
/// history above the inline band.
#[must_use]
pub fn rail_layout(
    real_cols: u16,
    margin: u16,
    width: u16,
    offset: u16,
    rows: u16,
    readout: &str,
) -> Rails {
    let band_end = margin.saturating_add(width).min(real_cols);
    // `.then(...)` (lazy), not `.then_some(margin - 1)`: the latter evaluates
    // `margin - 1` unconditionally and underflows when `margin == 0`.
    let left_col = (margin > 0).then(|| margin - 1);
    let right_col = (band_end < real_cols).then_some(band_end);

    // The readout: prefer the right gutter; fall back to just inside the band's
    // bottom-right; HIDE it when it fits neither (a sliver band on a tiny terminal).
    // The fit checks are what make the bounds proptest sound: on the gutter path
    // `col + len == real_cols`; on the fallback path `col >= margin` and
    // `col + len == band_end <= real_cols` both hold by the guard. Note the fallback
    // guard uses `band_end - margin` (the on-screen band width), not `width` —
    // `width` can exceed the on-screen span when the band is clamped at the right
    // edge (`margin + width > real_cols`).
    let readout_placed = if rows == 0 || offset >= rows {
        None
    } else {
        let len = readout.chars().count() as u16;
        let right_gutter = real_cols.saturating_sub(band_end);
        let col = if len > 0 && right_gutter >= len {
            // Right-align in the right gutter.
            Some(real_cols - len)
        } else if len > 0 && len <= band_end.saturating_sub(margin) {
            // No gutter room: just inside the band's bottom-right.
            Some(band_end - len)
        } else {
            None // fits neither the gutter nor the band: hide it
        };
        col.map(|col| Readout { col, row: rows - 1, text: readout.to_string() })
    };

    Rails { left_col, right_col, row_start: offset, row_end: rows, readout: readout_placed }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn margin_centred_basic() {
        // 20 slack over a 100-wide band in a 120 terminal → 10 each side.
        assert_eq!(margin(Layout::Center, 120, 100), 10);
        // Odd slack rounds down.
        assert_eq!(margin(Layout::Center, 121, 100), 10);
        // Band as wide as the terminal sits flush.
        assert_eq!(margin(Layout::Center, 100, 100), 0);
        // Band wider than terminal: saturating, flush at 0 (no underflow panic).
        assert_eq!(margin(Layout::Center, 80, 100), 0);
    }

    #[test]
    fn margin_left_is_always_zero() {
        assert_eq!(margin(Layout::Left, 120, 100), 0);
        assert_eq!(margin(Layout::Left, 40, 100), 0);
    }

    #[test]
    fn resolve_width_absolute_is_identity_clamped() {
        assert_eq!(resolve_width(Width::Cols(100), 120), 100);
        assert_eq!(resolve_width(Width::Cols(200), 120), 120); // cap at terminal
        assert_eq!(resolve_width(Width::Cols(0), 120), 1); // never zero
    }

    #[test]
    fn resolve_width_percent_floor_and_cap() {
        // 50% of 200 → 100.
        assert_eq!(resolve_width(Width::Percent(50), 200), 100);
        // 50% of 160 → 80.
        assert_eq!(resolve_width(Width::Percent(50), 160), 80);
        // Tiny percentage floors at MIN_W.
        assert_eq!(resolve_width(Width::Percent(1), 200), MIN_W);
        // Tiny terminal narrower than MIN_W: cap at real_cols, never > it.
        assert_eq!(resolve_width(Width::Percent(50), 10), 10);
        // 100% is the whole terminal.
        assert_eq!(resolve_width(Width::Percent(100), 200), 200);
    }

    #[test]
    fn step_width_preserves_unit() {
        assert!(matches!(step_width(Width::Cols(50), 5, 200), Width::Cols(_)));
        assert!(matches!(step_width(Width::Cols(50), -5, 200), Width::Cols(_)));
        assert!(matches!(step_width(Width::Percent(50), 5, 200), Width::Percent(_)));
        assert!(matches!(step_width(Width::Percent(50), -5, 200), Width::Percent(_)));
    }

    #[test]
    fn step_width_absolute_clamps() {
        assert_eq!(step_width(Width::Cols(20), -1, 200), Width::Cols(20));
        assert_eq!(step_width(Width::Cols(200), 1, 200), Width::Cols(200));
        assert_eq!(step_width(Width::Cols(100), 1, 200), Width::Cols(101));
        assert_eq!(step_width(Width::Cols(100), -1, 200), Width::Cols(99));
    }

    #[test]
    fn step_width_sub_min_start_never_grows_on_shrink() {
        // Cols(10) is legal via `--width 10` (resolve_width(Cols) has no MIN_W
        // floor); a shrink press must stay at 10, never snap up to MIN_W (20).
        assert_eq!(step_width(Width::Cols(10), -1, 200), Width::Cols(10));
        assert_eq!(step_width(Width::Cols(10), 1, 200), Width::Cols(11));
    }

    #[test]
    fn step_width_overwide_config_steps_from_effective() {
        // Cols(200) with real_cols = 120 steps from the effective 120, not 200.
        assert_eq!(step_width(Width::Cols(200), -1, 120), Width::Cols(119));
        assert_eq!(step_width(Width::Cols(200), 1, 120), Width::Cols(120));
    }

    #[test]
    fn step_width_percent_clamps() {
        assert_eq!(step_width(Width::Percent(1), -1, 200), Width::Percent(1));
        assert_eq!(step_width(Width::Percent(100), 1, 200), Width::Percent(100));
    }

    proptest! {
        /// `step_width` never panics and stays within `[MIN_W.min(real_cols,
        /// effective-start), real_cols]` for `Cols`, `[1, 100]` for `Percent`; a
        /// negative delta never yields a wider resolved band and a positive delta
        /// never a narrower one (monotonicity).
        #[test]
        fn step_width_stays_in_bounds(
            n in 1u16..=2000,
            pct in 1u8..=100,
            delta in -50i32..=50,
            real_cols in 1u16..=2000,
        ) {
            let cols_before = Width::Cols(n);
            let cols_w = resolve_width(cols_before, real_cols);
            let cols_after = step_width(cols_before, delta, real_cols);
            let cols_w2 = resolve_width(cols_after, real_cols);
            let effective_start = (n.max(1)).min(real_cols.max(1));
            let lo = MIN_W.min(real_cols.max(1)).min(effective_start);
            prop_assert!(cols_w2 <= real_cols, "Cols result {cols_w2} exceeds real_cols {real_cols}");
            prop_assert!(cols_w2 >= lo, "Cols result {cols_w2} below floor {lo}");
            if delta < 0 {
                prop_assert!(cols_w2 <= cols_w, "negative delta must not widen: {cols_w2} > {cols_w}");
            } else if delta > 0 {
                prop_assert!(cols_w2 >= cols_w, "positive delta must not narrow: {cols_w2} < {cols_w}");
            }

            let pct_before = Width::Percent(pct);
            let pct_w = resolve_width(pct_before, real_cols);
            let pct_after = step_width(pct_before, delta, real_cols);
            if let Width::Percent(p2) = pct_after {
                prop_assert!((1..=100).contains(&p2), "Percent result {p2} out of [1,100]");
                let pct_w2 = resolve_width(pct_after, real_cols);
                prop_assert!(pct_w2 <= real_cols, "Percent result {pct_w2} exceeds real_cols {real_cols}");
                if delta < 0 {
                    prop_assert!(pct_w2 <= pct_w, "negative delta must not widen: {pct_w2} > {pct_w}");
                } else if delta > 0 {
                    prop_assert!(pct_w2 >= pct_w, "positive delta must not narrow: {pct_w2} < {pct_w}");
                }
            } else {
                prop_assert!(false, "Percent must stay Percent");
            }
        }
    }

    proptest! {
        /// The ADR-006 band-fit invariant: for any terminal/band/column the
        /// physical column never wraps, never lands at or past `real_cols`, and
        /// stays within `[margin, margin + W)`.
        #[test]
        fn centred_margin_never_overflows_band(
            real_cols in 1u16..=1000,
            width in 1u16..=1000,
        ) {
            let w = resolve_width(Width::Cols(width), real_cols);
            let m = margin(Layout::Center, real_cols, w);

            prop_assert!(m as u32 + w as u32 <= real_cols as u32,
                "margin {m} + W {w} must fit in real_cols {real_cols}");

            for col in 0..w {
                let phys = physical_col(m, col);
                prop_assert!(phys >= m, "phys {phys} < margin {m}");
                prop_assert!(phys < m + w, "phys {phys} >= margin+W {}", m + w);
                prop_assert!(phys < real_cols, "phys {phys} >= real_cols {real_cols}");
            }
        }

        /// Left-aligned is the trivial margin-zero case: physical column equals
        /// in-band column and never escapes `[0, W)`.
        #[test]
        fn left_aligned_maps_identity(
            real_cols in 1u16..=1000,
            width in 1u16..=1000,
        ) {
            let w = resolve_width(Width::Cols(width), real_cols);
            let m = margin(Layout::Left, real_cols, w);
            prop_assert_eq!(m, 0);
            for col in 0..w {
                prop_assert_eq!(physical_col(m, col), col);
                prop_assert!((col as u32) < real_cols as u32);
            }
        }

        /// Recompute on resize: the centred margin computed at one terminal width
        /// then at another yields the correct offset for each.
        #[test]
        fn centred_margin_recomputes_on_resize(
            old_cols in 21u16..=1000,
            new_cols in 21u16..=1000,
            band in 1u16..=1000,
        ) {
            let w_old = resolve_width(Width::Cols(band), old_cols);
            let w_new = resolve_width(Width::Cols(band), new_cols);
            let m_old = margin(Layout::Center, old_cols, w_old);
            let m_new = margin(Layout::Center, new_cols, w_new);
            prop_assert_eq!(m_old, old_cols.saturating_sub(w_old) / 2);
            prop_assert_eq!(m_new, new_cols.saturating_sub(w_new) / 2);
        }

        /// `resolve_width(Percent)` floor/cap (ADR-011): for any terminal and
        /// percentage `1..=100` the band is `>= MIN_W.min(real_cols)`, `<=
        /// real_cols`, never `0`, and monotonic non-decreasing in `real_cols`.
        #[test]
        fn percent_width_floor_cap_monotonic(
            real_cols in 1u16..=2000,
            pct in 1u8..=100,
        ) {
            let w = resolve_width(Width::Percent(pct), real_cols);
            prop_assert!(w >= MIN_W.min(real_cols), "W {w} below floor");
            prop_assert!(w <= real_cols, "W {w} exceeds real_cols {real_cols}");
            prop_assert!(w >= 1, "W must never be zero");

            // Monotonic non-decreasing in real_cols.
            if real_cols < 2000 {
                let wider = resolve_width(Width::Percent(pct), real_cols + 1);
                prop_assert!(wider >= w,
                    "wider terminal {} yielded narrower band {wider} < {w}", real_cols + 1);
            }
        }

        /// `resolve_width(Cols)` is the identity on `n` (clamped), independent of
        /// `real_cols` whenever `n <= real_cols`.
        #[test]
        fn absolute_width_is_independent_of_real_cols(
            n in 1u16..=500,
            real_cols in 500u16..=2000,
        ) {
            prop_assert_eq!(resolve_width(Width::Cols(n), real_cols), n);
        }
    }

    #[test]
    fn readout_text_cols_is_bare_number() {
        assert_eq!(readout_text(Width::Cols(80), 80), "80");
    }

    #[test]
    fn readout_text_percent_has_suffix() {
        assert_eq!(readout_text(Width::Percent(55), 110), "55%");
    }

    #[test]
    fn rail_layout_center_has_both_rails() {
        // real_cols 120, margin 20, width 80 → band_end 100, right gutter 20.
        let rails = rail_layout(120, 20, 80, 0, 30, "80");
        assert_eq!(rails.left_col, Some(19));
        assert_eq!(rails.right_col, Some(100));
        let readout = rails.readout.expect("readout placed");
        assert_eq!(readout.col, 120 - 2, "right-aligned in the right gutter");
        assert_eq!(readout.row, 29);
    }

    #[test]
    fn rail_layout_left_suppresses_left_rail() {
        let rails = rail_layout(120, 0, 100, 0, 30, "100");
        assert_eq!(rails.left_col, None);
        assert_eq!(rails.right_col, Some(100));
    }

    #[test]
    fn rail_layout_full_width_suppresses_both() {
        let rails = rail_layout(100, 0, 100, 0, 30, "100");
        assert_eq!(rails.left_col, None);
        assert_eq!(rails.right_col, None);
        let readout = rails.readout.expect("readout falls back into the band");
        assert!(readout.col as u32 + readout.text.chars().count() as u32 <= 100);
    }

    #[test]
    fn rail_layout_hides_readout_when_it_fits_nowhere() {
        // real_cols 2, band [0, 2) fills the terminal: no gutter, and "55%" (len 3)
        // doesn't fit inside the 2-wide band either.
        let rails = rail_layout(2, 0, 2, 0, 5, "55%");
        assert_eq!(rails.readout, None);
    }

    #[test]
    fn rail_layout_zero_rows_hides_readout() {
        let rails = rail_layout(120, 20, 80, 0, 0, "80");
        assert_eq!(rails.readout, None);
        assert_eq!(rails.row_start, 0);
        assert_eq!(rails.row_end, 0);
    }

    proptest! {
        /// The rails/readout bounds hold for any geometry production can actually
        /// construct: `margin`/`W` are derived via `resolve_width`/`margin`, not
        /// generated free — a free margin would produce off-screen geometries no
        /// caller can construct.
        #[test]
        fn rails_stay_within_bounds(
            real_cols in 1u16..=1000,
            raw_width in 1u16..=1000,
            left_layout in any::<bool>(),
            rows in 0u16..=100,
            offset_frac in 0u16..=100,
            readout_len in 0usize..=6,
        ) {
            let layout = if left_layout { Layout::Left } else { Layout::Center };
            let w = resolve_width(Width::Cols(raw_width), real_cols);
            let m = margin(layout, real_cols, w);
            let offset = if rows == 0 { 0 } else { offset_frac % rows };
            let readout: String = "5".repeat(readout_len);

            let rails = rail_layout(real_cols, m, w, offset, rows, &readout);
            let band_end = m.saturating_add(w).min(real_cols);

            if let Some(left_col) = rails.left_col {
                prop_assert_eq!(left_col, m - 1);
                prop_assert!(left_col < m);
            }
            if let Some(right_col) = rails.right_col {
                prop_assert!(right_col >= band_end);
                prop_assert!(right_col < real_cols);
            }
            if let Some(r) = &rails.readout {
                let len = r.text.chars().count() as u16;
                prop_assert!(r.col.saturating_add(len) <= real_cols,
                    "readout col {} + len {} exceeds real_cols {}", r.col, len, real_cols);
                prop_assert!(rails.row_start <= r.row && r.row < rails.row_end,
                    "readout row {} outside span [{}, {})", r.row, rails.row_start, rails.row_end);
                // In-gutter placement never lands inside the band; the documented
                // in-band fallback is the only exception, and it only fires when the
                // right gutter is too narrow for the text.
                let right_gutter = real_cols.saturating_sub(band_end);
                if right_gutter >= len && len > 0 {
                    prop_assert!(r.col >= band_end, "gutter placement must not land in the band");
                }
            }
        }
    }
}
