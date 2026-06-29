//! The child's negotiated kitty keyboard level: a push/pop stack of enhancement
//! flags, clamped to the outer terminal's capability (ADR-003).
//!
//! The child drives this on its output — `CSI > N u` pushes a level, `CSI < u`
//! pops back to the previous one. The current level is the top of the stack; an
//! empty stack is [`KittyLevel::Legacy`]. Recognition of the CSI family lives in
//! [`is_kitty_csi`]; this module is only the state it feeds.

use super::encode::KittyLevel;

/// The child's kitty negotiation: a push/pop stack of enhancement-flag levels,
/// clamped to the outer terminal's capability.
#[derive(Debug)]
pub struct KittyState {
    /// Pushed levels, each the `N` from a `CSI > N u`. Top is current; empty is legacy.
    stack: Vec<u16>,
    /// The startup `supports_keyboard_enhancement()` probe. When `false`, `push` is
    /// a no-op, so we never offer the child a level the real terminal can't source.
    outer_supports: bool,
}

impl KittyState {
    /// A fresh state clamped to the outer terminal's capability. `outer_supports`
    /// is injectable so the forced non-kitty path is testable without a non-kitty
    /// terminal.
    pub fn new(outer_supports: bool) -> Self {
        Self {
            stack: Vec::new(),
            outer_supports,
        }
    }

    /// The child's current level: top of the stack, or `Legacy` when empty. Read
    /// lock-free at encode time — state and parser both live on the render thread
    /// (ADR-003).
    pub fn current(&self) -> KittyLevel {
        match self.stack.last() {
            Some(&flags) => KittyLevel::Kitty(flags),
            None => KittyLevel::Legacy,
        }
    }

    /// Feed one recognised CSI to the stack. `CSI > N u` pushes level `N` (first
    /// param, default `0`), clamped to a no-op when the outer terminal can't source
    /// kitty (ADR-003). `CSI < u` pops, returning to the previous level rather than
    /// zeroing; a pop on an empty stack is harmless. The set/query forms (`CSI = u`,
    /// `CSI ? u`) are recognised but inert — we track only the enable/disable the
    /// child drives.
    pub fn apply_csi(&mut self, i1: Option<u8>, params: &[&[u16]], c: char) {
        if c != 'u' {
            return;
        }
        match i1 {
            Some(b'>') if self.outer_supports => {
                let flags = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
                self.stack.push(flags);
            }
            Some(b'<') => {
                self.stack.pop();
            }
            _ => {}
        }
    }
}

/// Recognise the kitty keyboard CSI family: `c == 'u'` with an intermediate in
/// `{ '>', '=', '<', '?' }` (ADR-003). Kept separate from the state so the watcher
/// stays a thin adapter.
pub fn is_kitty_csi(i1: Option<u8>, c: char) -> bool {
    c == 'u' && matches!(i1, Some(b'>') | Some(b'=') | Some(b'<') | Some(b'?'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_the_kitty_family() {
        assert!(is_kitty_csi(Some(b'>'), 'u'));
        assert!(is_kitty_csi(Some(b'<'), 'u'));
        assert!(is_kitty_csi(Some(b'='), 'u'));
        assert!(is_kitty_csi(Some(b'?'), 'u'));
        // Not the family:
        assert!(!is_kitty_csi(None, 'u'), "bare CSI u is not the kitty enable");
        assert!(!is_kitty_csi(Some(b'>'), 'm'), "CSI > m is not kitty");
        assert!(!is_kitty_csi(Some(b'!'), 'u'), "wrong intermediate");
    }

    #[test]
    fn push_pop_stack_tracks_level() {
        let mut s = KittyState::new(true);
        assert_eq!(s.current(), KittyLevel::Legacy, "empty stack = legacy");

        s.apply_csi(Some(b'>'), &[&[1]], 'u');
        assert_eq!(s.current(), KittyLevel::Kitty(1));

        s.apply_csi(Some(b'>'), &[&[5]], 'u');
        assert_eq!(s.current(), KittyLevel::Kitty(5), "top of stack is current");

        s.apply_csi(Some(b'<'), &[], 'u');
        assert_eq!(s.current(), KittyLevel::Kitty(1), "pop returns to previous, not zero");

        s.apply_csi(Some(b'<'), &[], 'u');
        assert_eq!(s.current(), KittyLevel::Legacy);

        // An extra pop on an empty stack is harmless.
        s.apply_csi(Some(b'<'), &[], 'u');
        assert_eq!(s.current(), KittyLevel::Legacy);
    }

    #[test]
    fn missing_param_defaults_to_zero_flags() {
        let mut s = KittyState::new(true);
        s.apply_csi(Some(b'>'), &[], 'u');
        assert_eq!(s.current(), KittyLevel::Kitty(0));
    }

    #[test]
    fn clamp_makes_push_a_noop_when_outer_unsupported() {
        // Case B: outer terminal can't source kitty → every push is a no-op.
        let mut s = KittyState::new(false);
        s.apply_csi(Some(b'>'), &[&[1]], 'u');
        assert_eq!(s.current(), KittyLevel::Legacy, "child stays legacy under the clamp");
        s.apply_csi(Some(b'>'), &[&[5]], 'u');
        assert_eq!(s.current(), KittyLevel::Legacy);
        // Pops are still harmless.
        s.apply_csi(Some(b'<'), &[], 'u');
        assert_eq!(s.current(), KittyLevel::Legacy);
    }

    #[test]
    fn non_u_csi_is_ignored() {
        let mut s = KittyState::new(true);
        s.apply_csi(Some(b'>'), &[&[1]], 'm');
        assert_eq!(s.current(), KittyLevel::Legacy, "non-u CSI must not change the stack");
    }
}
