//! Mouse forwarding gate: turn one outer mouse report into the SGR bytes the
//! child expects, or a decision to drop it. See ADR-005.
//!
//! Pure — no I/O, no terminal, no channel. The render loop reads the child's live
//! `(mode, encoding)` from the screen each frame and passes them in; that poll is
//! the only mirror point, because the child's mode-set escapes are absorbed into
//! vt100's screen state and never reach a callback we can hook.
//!
//! The reports arrive from gutter's own scanner (ADR-020). gutter forces the
//! outer terminal into SGR-1006 itself, so there is exactly one wire shape to
//! recognise on the input side.

use vt100::{MouseProtocolEncoding, MouseProtocolMode};

use crate::scan::MouseReport;

/// Eager mouse capture (ADR-005): X10 compatibility, button-motion, any-motion,
/// then SGR-1006 for the extended coordinate encoding.
///
/// Written out rather than using crossterm's `EnableMouseCapture`, which also
/// sends `?1015h` (the urxvt encoding). What gutter asks the terminal for has to
/// stay inside what the scanner can extract, and the scanner recognises the SGR
/// shape only — a terminal that honoured `?1015h` would send reports gutter does
/// not extract and they would leak to the child as literal garbage (ADR-020).
pub const MOUSE_ENABLE: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h";
/// The matching reset forms, in the same order.
pub const MOUSE_DISABLE: &[u8] = b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l";

/// What the render loop should do with one decoded outer mouse event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MouseDecision {
    /// Re-encoded SGR bytes to write to the PTY master.
    Forward(Vec<u8>),
    /// Drop the event silently (gutter click, out-of-band, mode `None`, or a
    /// motion event filtered out by the child's granularity).
    Swallow,
    /// A reporting mode with a non-Sgr encoding — out of v1 scope. The render
    /// loop turns this into a loud abort rather than emitting malformed bytes.
    BailNonSgr,
}

/// What one report's button byte implies for the down-filter and the button-held
/// tracker. The byte itself and the press/release flag are forwarded straight from
/// the report, so nothing else needs deriving.
struct ButtonFacts {
    /// Whether this is a motion report, gated by the down-filter against the
    /// child's mode.
    is_motion: bool,
    /// `Some(true)` for a press (button now down), `Some(false)` for a release
    /// (button now up), `None` for motion and wheel reports, which are neither.
    press_transition: Option<bool>,
}

/// The forwarding gate. Carries one piece of state across events: whether a
/// button is held, which the `ButtonMotion` down-filter needs. The child's
/// `(mode, encoding)` is read live and passed into [`forward`] each call (ADR-005),
/// so there is nothing to cache.
///
/// A single "any button held" flag, not a per-button map — enough for the
/// `ButtonMotion` filter.
///
/// [`forward`]: MouseGate::forward
#[derive(Debug, Clone, Default)]
pub struct MouseGate {
    /// Whether any mouse button is held, tracked from decoded press/release
    /// events. Drives the `ButtonMotion` motion filter.
    button_held: bool,
}

impl MouseGate {
    /// Decide what to do with one outer mouse report, against the child's live
    /// `(mode, encoding)` (read from the screen by the caller this cycle) and
    /// `left_margin`/`w`.
    ///
    /// Order matters: gate on the child's reporting mode first (swallow `None`,
    /// bail on non-Sgr), then down-filter motion, then translate the coordinate
    /// (discarding gutter / out-of-band clicks), then re-encode SGR. The
    /// button-held tracker is updated from every press/release **even when the
    /// event is ultimately swallowed**, so the `ButtonMotion` filter stays honest
    /// across a mode change.
    pub fn forward(
        &mut self,
        report: &MouseReport,
        mode: MouseProtocolMode,
        encoding: MouseProtocolEncoding,
        left_margin: u16,
        w: u16,
    ) -> MouseDecision {
        let c = classify(report);

        // Track button-held on every press/release before any swallow path: a
        // press that lands while the child is in None still means the button is
        // down when a later ButtonMotion frame arrives.
        if let Some(now_held) = c.press_transition {
            self.button_held = now_held;
        }

        if mode == MouseProtocolMode::None {
            return MouseDecision::Swallow;
        }
        if encoding != MouseProtocolEncoding::Sgr {
            // Non-Sgr encoding is out of v1 scope. Forwarding a best-effort SGR
            // event would desync the child's mouse parser (ADR-005), so fail loud
            // upstream instead.
            return MouseDecision::BailNonSgr;
        }

        if c.is_motion && !should_forward_motion(mode, self.button_held) {
            return MouseDecision::Swallow;
        }

        // Rows pass through unchanged — the band spans the full height, only
        // columns carry the margin. translate_col discards gutter / out-of-band.
        match translate_col(report.col, left_margin, w) {
            Some(col0) => MouseDecision::Forward(encode_sgr(
                report.button,
                col0,
                report.row,
                report.release,
            )),
            None => MouseDecision::Swallow,
        }
    }
}

/// Translate a physical event column into a 0-based child column, or `None` when
/// the click is in the gutter (`column < left_margin`) or beyond the band
/// (`column - left_margin >= w`).
///
/// `checked_sub` is load-bearing (ADR-005): a click in the left gutter has
/// `column < left_margin`, and naive `u16` subtraction would wrap to a huge value
/// that then slips past the `< w` check. `None` rejects it; the proptest guards
/// this. The caller's `encode_sgr` adds the SGR 1-based `+1`.
fn translate_col(event_col: u16, left_margin: u16, w: u16) -> Option<u16> {
    let adjusted = event_col.checked_sub(left_margin)?;
    if adjusted < w {
        Some(adjusted)
    } else {
        None
    }
}

/// The down-filter decision: should a motion event be forwarded given the child's
/// mode and whether a button is held?
///
/// - `None` — unreachable here (the gate swallows `None` before motion filtering),
///   but total: no motion.
/// - `Press` (X10, mode 9) / `PressRelease` (mode 1000) — drop all motion.
/// - `ButtonMotion` (1002) — motion only while a button is held.
/// - `AnyMotion` (1003) — forward all motion.
fn should_forward_motion(mode: MouseProtocolMode, button_held: bool) -> bool {
    match mode {
        MouseProtocolMode::None
        | MouseProtocolMode::Press
        | MouseProtocolMode::PressRelease => false,
        MouseProtocolMode::ButtonMotion => button_held,
        MouseProtocolMode::AnyMotion => true,
    }
}

/// Read the down-filter and button-tracker facts off the SGR button byte.
///
/// The button byte: the low 2 bits select the button (0 left, 1 middle, 2 right,
/// 3 "no button"); bits 2–4 carry the shift/alt/ctrl modifiers and are passed
/// through untouched; bit 5 (value 32) is the motion bit; bit 6 (value 64) is the
/// wheel bit (wheel-up 64, wheel-down 65, wheel-left 66, wheel-right 67).
///
/// Only a real press or release moves the button-held tracker. Motion and wheel
/// reports are neither, and counting them would invert the `ButtonMotion`
/// down-filter (ADR-005).
fn classify(report: &MouseReport) -> ButtonFacts {
    const MOTION: u16 = 32;
    const WHEEL: u16 = 64;

    let is_motion = report.button & MOTION != 0;
    let is_wheel = report.button & WHEEL != 0;
    ButtonFacts {
        is_motion,
        press_transition: (!is_motion && !is_wheel).then_some(!report.release),
    }
}

/// Format an SGR 1006 mouse report: `CSI < b ; col+1 ; row+1 M|m`. `col0`/`row0`
/// are 0-based as the scanner reports them; SGR is 1-based, so each gets `+1`.
///
/// A release takes the lowercase `m`. Encoding one with `M` would tell the child
/// the button is still down — a stuck-button bug the golden-byte tests pin.
fn encode_sgr(button_byte: u16, col0: u16, row0: u16, release: bool) -> Vec<u8> {
    let final_byte = if release { 'm' } else { 'M' };
    format!(
        "\x1b[<{};{};{}{}",
        button_byte,
        col0 as u32 + 1,
        row0 as u32 + 1,
        final_byte
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// The SGR wire bits the tests build reports from.
    const MOTION: u16 = 32;
    const WHEEL: u16 = 64;
    const NO_BUTTON: u16 = 3;

    fn press(button: u16, col: u16, row: u16) -> MouseReport {
        MouseReport { button, col, row, release: false }
    }

    fn release(button: u16, col: u16, row: u16) -> MouseReport {
        MouseReport { button, col, row, release: true }
    }

    /// A drag: motion carrying the held button.
    fn drag(button: u16, col: u16, row: u16) -> MouseReport {
        press(button | MOTION, col, row)
    }

    /// A bare move: motion with no button held.
    fn moved(col: u16, row: u16) -> MouseReport {
        press(NO_BUTTON | MOTION, col, row)
    }

    // --- encode_sgr golden bytes ---

    #[test]
    fn encode_sgr_press_at_origin() {
        // A left press at child col 0 row 0 → CSI < 0 ; 1 ; 1 M.
        assert_eq!(encode_sgr(0, 0, 0, false), b"\x1b[<0;1;1M".to_vec());
    }

    #[test]
    fn encode_sgr_release_uses_lowercase_m() {
        // A release → ... m, never M (the stuck-button guard).
        assert_eq!(encode_sgr(0, 0, 0, true), b"\x1b[<0;1;1m".to_vec());
    }

    #[test]
    fn encode_sgr_adds_one_to_both_coords() {
        // col 9 row 4 (0-based) → 10 ; 5 (1-based).
        assert_eq!(encode_sgr(2, 9, 4, false), b"\x1b[<2;10;5M".to_vec());
    }

    // --- should_forward_motion truth table ---

    #[test]
    fn motion_filter_truth_table() {
        // PressRelease / Press → never forward motion.
        assert!(!should_forward_motion(MouseProtocolMode::PressRelease, true));
        assert!(!should_forward_motion(MouseProtocolMode::PressRelease, false));
        assert!(!should_forward_motion(MouseProtocolMode::Press, true));
        assert!(!should_forward_motion(MouseProtocolMode::Press, false));
        // ButtonMotion → follows button_held.
        assert!(should_forward_motion(MouseProtocolMode::ButtonMotion, true));
        assert!(!should_forward_motion(MouseProtocolMode::ButtonMotion, false));
        // AnyMotion → always.
        assert!(should_forward_motion(MouseProtocolMode::AnyMotion, true));
        assert!(should_forward_motion(MouseProtocolMode::AnyMotion, false));
    }

    // --- classify ---

    #[test]
    fn classify_drag_is_motion_with_button() {
        let c = classify(&drag(0, 0, 0));
        assert!(c.is_motion);
        assert_eq!(c.press_transition, None);
    }

    #[test]
    fn classify_moved_is_motion_without_button() {
        let c = classify(&moved(0, 0));
        assert!(c.is_motion);
        assert_eq!(c.press_transition, None);
    }

    #[test]
    fn classify_down_up_transitions() {
        let down = classify(&press(2, 0, 0));
        assert_eq!(down.press_transition, Some(true));
        assert!(!down.is_motion);

        assert_eq!(classify(&release(2, 0, 0)).press_transition, Some(false));
    }

    #[test]
    fn classify_wheel_is_neither_motion_nor_a_transition() {
        let up = classify(&press(WHEEL, 0, 0));
        assert!(!up.is_motion);
        assert_eq!(up.press_transition, None, "a wheel tick never holds a button");
        assert!(!classify(&press(WHEEL | 1, 0, 0)).is_motion);
    }

    #[test]
    fn classify_reads_a_modified_click_as_a_press() {
        // Shift-click: button 0 | shift 4. The modifier bits must not read as the
        // motion or wheel bit.
        let c = classify(&press(4, 0, 0));
        assert!(!c.is_motion);
        assert_eq!(c.press_transition, Some(true));
    }

    // --- MouseGate::forward branches ---

    /// Forward a single report at `(mode, Sgr)`.
    fn fwd_sgr(
        g: &mut MouseGate,
        mode: MouseProtocolMode,
        report: MouseReport,
        left_margin: u16,
        w: u16,
    ) -> MouseDecision {
        g.forward(&report, mode, MouseProtocolEncoding::Sgr, left_margin, w)
    }

    #[test]
    fn forward_swallows_when_mode_none() {
        let mut g = MouseGate::default();
        let d = fwd_sgr(&mut g, MouseProtocolMode::None, press(0, 5, 2), 0, 80);
        assert_eq!(d, MouseDecision::Swallow);
    }

    #[test]
    fn forward_bails_on_non_sgr_encoding() {
        for enc in [MouseProtocolEncoding::Default, MouseProtocolEncoding::Utf8] {
            let mut g = MouseGate::default();
            let d = g.forward(
                &press(0, 5, 2),
                MouseProtocolMode::PressRelease,
                enc,
                0,
                80,
            );
            assert_eq!(d, MouseDecision::BailNonSgr);
            // And crucially produced no SGR bytes.
            assert!(!matches!(d, MouseDecision::Forward(_)));
        }
    }

    #[test]
    fn forward_pressrelease_drops_motion_keeps_press_and_release() {
        let mut g = MouseGate::default();
        let m = MouseProtocolMode::PressRelease;
        // Press → forward.
        assert!(matches!(
            fwd_sgr(&mut g, m, press(0, 0, 0), 0, 80),
            MouseDecision::Forward(_)
        ));
        // Motion (drag) → swallow even though a button is now held.
        assert_eq!(
            fwd_sgr(&mut g, m, drag(0, 1, 0), 0, 80),
            MouseDecision::Swallow
        );
        // Release → forward.
        assert!(matches!(
            fwd_sgr(&mut g, m, release(0, 1, 0), 0, 80),
            MouseDecision::Forward(_)
        ));
    }

    #[test]
    fn forward_buttonmotion_tracks_button_held() {
        let mut g = MouseGate::default();
        let m = MouseProtocolMode::ButtonMotion;
        // press, motion, release, motion → forward, forward, forward, swallow.
        assert!(matches!(
            fwd_sgr(&mut g, m, press(0, 0, 0), 0, 80),
            MouseDecision::Forward(_)
        ));
        assert!(matches!(
            fwd_sgr(&mut g, m, drag(0, 1, 0), 0, 80),
            MouseDecision::Forward(_)
        ));
        assert!(matches!(
            fwd_sgr(&mut g, m, release(0, 1, 0), 0, 80),
            MouseDecision::Forward(_)
        ));
        // Motion with no button held → swallow.
        assert_eq!(
            fwd_sgr(&mut g, m, moved(2, 0), 0, 80),
            MouseDecision::Swallow
        );
    }

    #[test]
    fn forward_clean_click_translates_and_encodes() {
        let mut g = MouseGate::default();
        // margin 10, click at physical col 15 → child col 5 → SGR col 6.
        let d = fwd_sgr(&mut g, MouseProtocolMode::PressRelease, press(0, 15, 3), 10, 80);
        assert_eq!(d, MouseDecision::Forward(b"\x1b[<0;6;4M".to_vec()));
    }

    #[test]
    fn forward_keeps_the_modifier_bits_on_a_click() {
        let mut g = MouseGate::default();
        // Shift-click (button 0 | 4) at margin 10, physical col 15.
        let d = fwd_sgr(&mut g, MouseProtocolMode::PressRelease, press(4, 15, 3), 10, 80);
        assert_eq!(
            d,
            MouseDecision::Forward(b"\x1b[<4;6;4M".to_vec()),
            "the child must see the shift bit, not a plain click"
        );
    }

    #[test]
    fn forward_discards_gutter_click() {
        let mut g = MouseGate::default();
        let m = MouseProtocolMode::PressRelease;
        // margin 10, click at col 4 (left of band) → swallow.
        assert_eq!(
            fwd_sgr(&mut g, m, press(0, 4, 0), 10, 20),
            MouseDecision::Swallow
        );
        // click at col 30 (>= margin 10 + W 20) → swallow.
        assert_eq!(
            fwd_sgr(&mut g, m, press(0, 30, 0), 10, 20),
            MouseDecision::Swallow
        );
    }

    // --- the live mode/encoding poll drives the gate per-report ---

    #[test]
    fn gate_flips_with_polled_modes() {
        let mut g = MouseGate::default();
        let click = press(0, 5, 1);

        // None → swallow.
        assert_eq!(
            fwd_sgr(&mut g, MouseProtocolMode::None, click, 0, 80),
            MouseDecision::Swallow
        );

        // (PressRelease, Sgr) → forward.
        assert!(matches!(
            fwd_sgr(&mut g, MouseProtocolMode::PressRelease, click, 0, 80),
            MouseDecision::Forward(_)
        ));

        // Back to None → swallow again.
        assert_eq!(
            fwd_sgr(&mut g, MouseProtocolMode::None, click, 0, 80),
            MouseDecision::Swallow
        );
    }

    // --- proptest: translate_col is total, no off-by-one, no overflow ---

    proptest! {
        #[test]
        fn translate_col_is_correct_and_total(
            event_col in any::<u16>(),
            // left_margin spans both --left (0) and --center offsets.
            left_margin in 0u16..=400,
            // realistic band widths.
            w in 1u16..=400,
        ) {
            let out = translate_col(event_col, left_margin, w);
            // Reference, computed without wrapping.
            let expected = match event_col.checked_sub(left_margin) {
                Some(a) if a < w => Some(a),
                _ => None,
            };
            prop_assert_eq!(out, expected);
            // When Some, the 1-based forwarded column is in [1, W].
            if let Some(col0) = out {
                let one_based = col0 as u32 + 1;
                prop_assert!(one_based >= 1 && one_based <= w as u32);
            }
        }
    }
}
