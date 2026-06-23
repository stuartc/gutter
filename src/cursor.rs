//! Cursor-shape mirroring (DECSCUSR / `CSI Ps SP q`) — slice 08.
//!
//! ADR-001/003/004 are explicit that `vt100`'s only `Callbacks` hooks are
//! `unhandled_csi` and `copy_to_clipboard`, and that `process_cb` does not
//! exist — so before building a watcher the slice confirmed against `vt100`
//! source whether cursor shape is observable at all. **It is:** `vt100` does
//! not implement DECSCUSR, so the child's `CSI Ps SP q` surfaces through
//! `unhandled_csi` with the intermediate `SP` (0x20) reported as `i1` and the
//! final byte `q` — verified by probing the pinned `vt100 0.16.2`. (The
//! intermediate lands in `i1`, not `i2`, which is the trap.)
//!
//! So cursor shape **is in scope** (not the documented-gap path): the watcher
//! records the child's requested shape, and the render loop re-emits the matching
//! `CSI Ps SP q` to the outer terminal alongside the existing cursor reposition
//! (slice 02). The watcher lives on the shared [`crate::callbacks::GutterCallbacks`]
//! struct beside the kitty watcher and the clipboard hook; it touches only its
//! own field.

/// The intermediate byte of DECSCUSR (`CSI Ps SP q`) — a space (0x20). `vt100`
/// surfaces it as `unhandled_csi`'s `i1`.
pub const DECSCUSR_INTERMEDIATE: u8 = b' ';

/// Is this unhandled CSI a DECSCUSR cursor-shape request? `i1 == Some(' ')` and
/// final byte `q` (verified against `vt100 0.16.2` — the space intermediate is
/// reported in `i1`, not `i2`).
#[must_use]
pub fn is_decscusr(i1: Option<u8>, c: char) -> bool {
    i1 == Some(DECSCUSR_INTERMEDIATE) && c == 'q'
}

/// The child's requested cursor shape, tracked from the DECSCUSR `Ps` parameter.
///
/// Only the latest request is kept — the outer terminal's shape is a single
/// piece of state, so the most recent `CSI Ps SP q` wins (DECSCUSR is not a
/// stack like the kitty flags). `None` means the child has never set a shape, so
/// gutter leaves the outer terminal's default untouched.
#[derive(Debug, Default, Clone)]
pub struct CursorShape {
    /// The latest DECSCUSR `Ps` the child emitted, or `None` if it never did.
    /// `0`/`1` = blinking block, `2` = steady block, `3` = blinking underline,
    /// `4` = steady underline, `5` = blinking bar, `6` = steady bar.
    requested: Option<u16>,
    /// The `Ps` last mirrored to the outer terminal, so the render loop only
    /// re-emits a `CSI Ps SP q` when the requested shape actually changed.
    mirrored: Option<u16>,
}

impl CursorShape {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a DECSCUSR request. `params` is the `unhandled_csi` parameter list;
    /// the `Ps` is its first value (absent → `0`, the default per DECSCUSR).
    pub fn apply_csi(&mut self, params: &[&[u16]]) {
        let ps = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        self.requested = Some(ps);
    }

    /// If the requested shape differs from what was last mirrored, return the
    /// `CSI Ps SP q` bytes to emit to the outer terminal and mark it mirrored.
    /// Returns `None` when there is nothing new to mirror (no request yet, or the
    /// shape is unchanged) — so the render loop emits a shape change only on a
    /// real transition, never every frame.
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
    fn absent_param_defaults_to_zero() {
        let mut s = CursorShape::new();
        // A bare `CSI SP q` carries an empty param → default Ps 0.
        s.apply_csi(&[]);
        assert_eq!(s.take_pending(), Some(b"\x1b[0 q".to_vec()));
    }
}
