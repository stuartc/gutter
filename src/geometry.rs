//! Pure offset / coordinate maths for the offset repaint.
//!
//! These are free functions with no I/O so they can be property-tested directly
//! (the re-scope checkpoint explicitly calls out "margin maths scattered across
//! files" as a smell — this module is the one home for it). The render loop and
//! the channel code never compute a margin themselves; they call in here.
//!
//! Two band alignments exist in the design:
//! - **left-aligned** (`left_margin == 0`): the only alignment wired to a flag
//!   in this slice — content starts at physical column 0.
//! - **centred** (`centred_margin`): the formula is implemented and
//!   property-tested now, but the `--center`/`--left` flag that selects it
//!   lands in slice 05 (ADR-011).

/// The left margin for a centred band: half the slack between the real terminal
/// width and the band width `W`, rounded down. `saturating_sub` pins the slack
/// to zero when `W >= real_cols`, so the margin is never negative and a band as
/// wide as (or wider than) the terminal sits flush at column 0.
///
/// Implemented and property-tested in this slice (the ADR-006 invariant), but
/// the `--center` flag that selects it over left-alignment lands in slice 05 —
/// hence it has no production call site yet.
#[must_use]
#[allow(dead_code)]
pub fn centred_margin(real_cols: u16, width: u16) -> u16 {
    real_cols.saturating_sub(width) / 2
}

/// The physical column a child cell at in-band column `col` maps to, given the
/// band's left margin. Saturating so an out-of-range column can never wrap.
#[must_use]
pub fn physical_col(left_margin: u16, col: u16) -> u16 {
    left_margin.saturating_add(col)
}

/// Resolve the effective band width from the parsed `--width` (absolute, or
/// `None` for "use the real width") against the current real terminal width.
///
/// The band is never wider than the terminal — a `--width` larger than the
/// terminal is clamped to the terminal width so the band can't overflow the
/// screen. (The proportional/recompute-on-resize behaviour is slice 05; here
/// `W` is fixed for the session.)
#[must_use]
pub fn resolve_width(requested: Option<u16>, real_cols: u16) -> u16 {
    match requested {
        Some(w) => w.min(real_cols).max(1),
        None => real_cols.max(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn centred_margin_basic() {
        // 20 slack over a 100-wide band in a 120 terminal → 10 each side.
        assert_eq!(centred_margin(120, 100), 10);
        // Odd slack rounds down.
        assert_eq!(centred_margin(121, 100), 10);
        // Band as wide as the terminal sits flush.
        assert_eq!(centred_margin(100, 100), 0);
        // Band wider than terminal: saturating, flush at 0.
        assert_eq!(centred_margin(80, 100), 0);
    }

    #[test]
    fn resolve_width_clamps_and_defaults() {
        assert_eq!(resolve_width(Some(100), 120), 100);
        assert_eq!(resolve_width(Some(200), 120), 120); // clamp to terminal
        assert_eq!(resolve_width(None, 120), 120); // default = real width
        assert_eq!(resolve_width(Some(0), 120), 1); // never zero
    }

    proptest! {
        /// The core ADR-006 invariant for the centred formula (proven now even
        /// though the flag lands in slice 05): for any terminal width, band
        /// width and in-band column, the physical column the repaint targets is
        /// never negative (u16 can't be, but the saturating maths must not
        /// wrap), never lands a content cell at or past `real_cols`, and the
        /// in-band content range stays within `[margin, margin + W)`.
        #[test]
        fn centred_margin_never_overflows_band(
            real_cols in 1u16..=1000,
            width in 1u16..=1000,
        ) {
            // The band is always clamped to fit the terminal.
            let w = resolve_width(Some(width), real_cols);
            let margin = centred_margin(real_cols, w);

            // The band fits: margin + W never exceeds the terminal width.
            prop_assert!(margin as u32 + w as u32 <= real_cols as u32,
                "margin {margin} + W {w} must fit in real_cols {real_cols}");

            // Every in-band column maps inside [margin, margin + W) and so is a
            // valid physical column < real_cols.
            for col in 0..w {
                let phys = physical_col(margin, col);
                prop_assert!(phys >= margin, "phys {phys} < margin {margin}");
                prop_assert!(phys < margin + w, "phys {phys} >= margin+W {}", margin + w);
                prop_assert!(phys < real_cols, "phys {phys} >= real_cols {real_cols}");
            }
        }

        /// Left-aligned is the trivial margin-zero case: the physical column
        /// equals the in-band column and never escapes `[0, W)`.
        #[test]
        fn left_aligned_maps_identity(
            real_cols in 1u16..=1000,
            width in 1u16..=1000,
        ) {
            let w = resolve_width(Some(width), real_cols);
            let margin = 0u16;
            for col in 0..w {
                let phys = physical_col(margin, col);
                prop_assert_eq!(phys, col);
                prop_assert!(phys < w);
                prop_assert!((phys as u32) < real_cols as u32);
            }
        }
    }
}
