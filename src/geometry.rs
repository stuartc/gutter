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
    fn default_width_absolute_hundred_clamped() {
        // Wide terminal: the default 100-col band fits untouched.
        assert_eq!(resolve_width(Width::Cols(100), 200), 100);
        // Narrower than 100: clamp to the terminal, no panic, no narrowing below it.
        assert_eq!(resolve_width(Width::Cols(100), 80), 80);
    }

    #[test]
    fn full_alias_is_full_width() {
        // `--width full` resolves to Percent(100), which is always the whole
        // terminal — including below MIN_W, where Cols(100) would instead clamp.
        for n in [10, 20, 80, 300] {
            assert_eq!(resolve_width(Width::Percent(100), n), n);
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

        /// `full` (`Percent(100)`) is exact passthrough for every terminal width,
        /// including below `MIN_W` — the `.min(real_cols)` cap wins over the floor.
        #[test]
        fn full_alias_is_passthrough_for_any_real_cols(real_cols in 1u16..=2000) {
            prop_assert_eq!(resolve_width(Width::Percent(100), real_cols), real_cols);
        }
    }
}
