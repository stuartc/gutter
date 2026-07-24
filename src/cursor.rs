//! Cursor-shape mirroring (DECSCUSR / `CSI Ps SP q`).
//!
//! vt100 doesn't implement DECSCUSR, so the child's request surfaces through
//! `unhandled_csi`. The watcher records the requested shape and the render loop
//! re-emits the matching `CSI Ps SP q` to the outer terminal. It lives on
//! [`crate::callbacks::GutterCallbacks`] beside the clipboard hook
//! and touches only its own field.

/// The intermediate byte of DECSCUSR (`CSI Ps SP q`) — a space (0x20).
pub const DECSCUSR_INTERMEDIATE: u8 = b' ';

/// Returns whether this unhandled CSI is a DECSCUSR cursor-shape request.
///
/// vt100 reports the space intermediate in `i1`, not `i2` (verified against
/// vt100 0.16.2) — that's the trap.
#[must_use]
pub fn is_decscusr(i1: Option<u8>, c: char) -> bool {
    i1 == Some(DECSCUSR_INTERMEDIATE) && c == 'q'
}

/// The child's requested cursor shape, tracked from the DECSCUSR `Ps` parameter.
///
/// Only the latest request is kept: the outer terminal's shape is a single value,
/// so the most recent `CSI Ps SP q` wins (DECSCUSR is not a stack like the kitty
/// flags). `None` means the child never set a shape, so gutter leaves the outer
/// terminal's default untouched.
#[derive(Debug, Default, Clone)]
pub struct CursorShape {
    /// The latest DECSCUSR `Ps` the child emitted, or `None` if it never did.
    /// `0`/`1` = blinking block, `2` = steady block, `3` = blinking underline,
    /// `4` = steady underline, `5` = blinking bar, `6` = steady bar.
    requested: Option<u16>,
    /// The `Ps` last mirrored to the outer terminal, so the render loop re-emits
    /// only when the requested shape actually changed.
    mirrored: Option<u16>,
}

impl CursorShape {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a DECSCUSR request. `Ps` is the first value in the parameter list;
    /// absent → `0`, the DECSCUSR default.
    pub fn apply_csi(&mut self, params: &[&[u16]]) {
        let ps = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        self.requested = Some(ps);
    }

    /// Forget what was last mirrored, so the next [`take_pending`] re-emits the
    /// child's last requested shape even though it did not change. Used on resume
    /// (ADR-0019): park reset the outer terminal's cursor to the default, so the
    /// mirrored state is stale and the child's shape must be re-asserted.
    ///
    /// [`take_pending`]: CursorShape::take_pending
    pub fn rearm(&mut self) {
        self.mirrored = None;
    }

    /// Returns the `CSI Ps SP q` bytes to emit when the requested shape differs
    /// from what was last mirrored, marking it mirrored. `None` when there's
    /// nothing new (no request yet, or unchanged), so the render loop emits a
    /// shape change on a real transition, not every frame.
    #[must_use]
    pub fn take_pending(&mut self) -> Option<Vec<u8>> {
        match self.requested {
            Some(ps) if self.mirrored != Some(ps) => {
                self.mirrored = Some(ps);
                Some(format!("\x1b[{ps} q").into_bytes())
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_decscusr_not_other_csi() {
        assert!(is_decscusr(Some(b' '), 'q'), "CSI Ps SP q is DECSCUSR");
        // A bare `CSI Ps q` (no SP intermediate) is a different sequence.
        assert!(!is_decscusr(None, 'q'));
        // A kitty `CSI > 1 u` is not DECSCUSR.
        assert!(!is_decscusr(Some(b'>'), 'u'));
        // The right intermediate, wrong final.
        assert!(!is_decscusr(Some(b' '), 'p'));
    }

    #[test]
    fn tracks_latest_request_and_emits_on_change() {
        let mut s = CursorShape::new();
        // No request yet → nothing to mirror.
        assert_eq!(s.take_pending(), None);

        // Child sets a steady bar (Ps = 6).
        s.apply_csi(&[&[6]]);
        assert_eq!(s.take_pending(), Some(b"\x1b[6 q".to_vec()));
        // Idempotent: same shape, nothing new to emit.
        assert_eq!(s.take_pending(), None);

        // Child changes to steady underline (Ps = 4).
        s.apply_csi(&[&[4]]);
        assert_eq!(s.take_pending(), Some(b"\x1b[4 q".to_vec()));
    }

    #[test]
    fn rearm_re_emits_last_shape_after_a_reset() {
        let mut s = CursorShape::new();
        s.apply_csi(&[&[6]]);
        assert_eq!(s.take_pending(), Some(b"\x1b[6 q".to_vec()));
        // Nothing changed → nothing to emit.
        assert_eq!(s.take_pending(), None);
        // Park reset the outer cursor to default; rearm makes resume re-assert it.
        s.rearm();
        assert_eq!(
            s.take_pending(),
            Some(b"\x1b[6 q".to_vec()),
            "rearm re-emits the child's last shape even though it did not change"
        );
        // And it is one-shot again.
        assert_eq!(s.take_pending(), None);
    }

    #[test]
    fn rearm_with_no_request_is_a_noop() {
        let mut s = CursorShape::new();
        s.rearm();
        assert_eq!(
            s.take_pending(),
            None,
            "a child that never set a shape has nothing to re-assert"
        );
    }

    #[test]
    fn absent_param_defaults_to_zero() {
        let mut s = CursorShape::new();
        // A bare `CSI SP q` carries an empty param → default Ps 0.
        s.apply_csi(&[]);
        assert_eq!(s.take_pending(), Some(b"\x1b[0 q".to_vec()));
    }
}
