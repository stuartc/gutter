//! The child's negotiated kitty level — a push/pop stack with the outer-capability
//! clamp (ADR-003).
//!
//! Kitty is negotiated by the child on its **output**: `CSI > N u` enables a
//! level, `CSI < u` disables (pops). Applications nest these, so the level is a
//! **stack**, not a single flag — a `CSI < u` returns to the previous level
//! rather than zeroing it. The current level is the top of the stack; an empty
//! stack is [`KittyLevel::Legacy`].
//!
//! The **clamp** is the whole point of case B (ADR-003): the outer terminal's
//! kitty grant flows inbound on the keyboard path, which `vt100` never parses, so
//! it can only be learned at startup via `supports_keyboard_enhancement()`. If
//! the outer terminal cannot source kitty, offering the child a kitty level would
//! wedge it waiting for kitty key bytes the user's terminal can never produce. So
//! when `outer_supports == false`, every `push` is a **no-op**: the child stays
//! legacy and Shift+Enter degrades predictably to the same byte as Enter.
//!
//! This is the **state**; the **recognition** of the kitty CSI family lives in
//! [`is_kitty_csi`] and is driven from the `Callbacks::unhandled_csi` watcher.
//! Keeping recognition and state separable lets the OSC-52 clipboard concern
//! (slice 06) sit alongside the kitty watcher on the shared callbacks struct
//! without the two reaching into each other.

use super::encode::KittyLevel;

/// The child's kitty negotiation, tracked as a push/pop stack of enhancement-flag
/// levels, clamped to the outer terminal's capability.
#[derive(Debug)]
pub struct KittyState {
    /// The stack of pushed levels (each the `N` from a `CSI > N u`). The top is
    /// the current level; empty means legacy.
    stack: Vec<u16>,
    /// Whether the outer terminal can source kitty (the startup
    /// `supports_keyboard_enhancement()` probe). When `false`, `push` is a no-op.
    outer_supports: bool,
}

impl KittyState {
    /// A fresh state with the outer-capability clamp set. `outer_supports` is the
    /// one bool the harness can inject so case B (forced non-kitty) is testable
    /// without owning a non-kitty terminal.
    pub fn new(outer_supports: bool) -> Self {
        Self {
            stack: Vec::new(),
            outer_supports,
        }
    }

    /// The child's current keyboard level — the top of the stack, or `Legacy`
    /// when empty. Read lock-free at encode time (state and parser both live on
    /// the render thread, ADR-009).
    pub fn current(&self) -> KittyLevel {
        match self.stack.last() {
            Some(&flags) => KittyLevel::Kitty(flags),
            None => KittyLevel::Legacy,
        }
    }

    /// Feed one recognised CSI to the stack. `CSI > N u` pushes level `N` (from
    /// the first param, default `0`) — **clamped:** when the outer terminal
    /// cannot source kitty the push is a no-op, so the child stays legacy (the
    /// case-B degradation contract). `CSI < u` pops, returning to the previous
    /// level rather than zeroing (a pop on an empty stack is harmless). `CSI = u`
    /// / `CSI ? u` (set/query forms) are observed-but-inert here — this slice
    /// tracks only the enable/disable the target program drives. Called by the
    /// `unhandled_csi` watcher after [`is_kitty_csi`] has matched.
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
/// `{ '>', '=', '<', '?' }` (ADR-003). The pure predicate, separated from the
/// state so the recognition can be unit-tested and so the watcher stays a thin
/// adapter.
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

        // Nest a second push.
        s.apply_csi(Some(b'>'), &[&[5]], 'u');
        assert_eq!(s.current(), KittyLevel::Kitty(5), "top of stack is current");

        // Pop back to the first level.
        s.apply_csi(Some(b'<'), &[], 'u');
        assert_eq!(s.current(), KittyLevel::Kitty(1), "pop returns to previous, not zero");

        // Pop to empty → legacy.
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
