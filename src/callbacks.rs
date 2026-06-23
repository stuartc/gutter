//! The `vt100::Callbacks` impl carried by the render thread's parser.
//!
//! This is the **shared callbacks struct** (ADR-003/004): the single place
//! gutter hooks the parser. It is wired in by slice 02 as inert plumbing; slice
//! 04 (this slice) adds the kitty keyboard-level watcher, and slice 06 will add
//! the OSC-52 `copy_to_clipboard` override plus a `/dev/tty` handle to **this
//! same struct** — a sibling field/method, not a reshape. The kitty watcher and
//! the future clipboard concern must not reach into each other.
//!
//! Construct the parser with
//! `vt100::Parser::new_with_callbacks(rows, cols, scrollback, GutterCallbacks::new(..))`
//! — there is **no** `process_cb` method (that reference in the rough plan is
//! wrong). The callbacks fire on `parser.process(bytes)` on the render thread,
//! the only thread that touches the parser, so there is no shared mutable state
//! and `kitty_state.current()` is read lock-free at encode time (ADR-009).

use crate::keyboard::{is_kitty_csi, KittyState};

/// The single callbacks struct the parser owns.
///
/// Slice 04 gives it the kitty [`KittyState`] (the child's negotiated keyboard
/// level, clamped to the outer terminal's capability). Slice 06 adds the
/// clipboard concern alongside `kitty_state`, not inside it.
#[derive(Debug)]
pub struct GutterCallbacks {
    /// The child's negotiated kitty keyboard level — driven by the
    /// `unhandled_csi` watcher below, read by the encoder at keystroke time.
    pub kitty_state: KittyState,
}

impl GutterCallbacks {
    /// Build the callbacks with the outer terminal's kitty capability (the
    /// startup `supports_keyboard_enhancement()` probe). When `false`, the
    /// child's kitty enable is clamped to a no-op (case-B degradation).
    pub fn new(outer_supports: bool) -> Self {
        Self {
            kitty_state: KittyState::new(outer_supports),
        }
    }
}

impl vt100::Callbacks for GutterCallbacks {
    /// The kitty keyboard watcher (ADR-003). The child enables/disables kitty on
    /// its **output** via `CSI > N u` / `CSI < u`, which surface here as
    /// unhandled CSI sequences. Recognise the family, then hand the sequence to
    /// the push/pop stack — which applies the outer-capability clamp.
    ///
    /// This is the **only** concern this method has. Slice 06's clipboard lives
    /// in `copy_to_clipboard`, a sibling method, with no entanglement here.
    fn unhandled_csi(
        &mut self,
        _: &mut vt100::Screen,
        i1: Option<u8>,
        _i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        if is_kitty_csi(i1, c) {
            self.kitty_state.apply_csi(i1, params, c);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyboard::KittyLevel;

    /// The watcher tracks the child's kitty level **through the real `vt100`
    /// callback path** — drive `parser.process()` with enable/disable bytes and
    /// assert `kitty_state.current()` at each step (the AC: "negotiated level
    /// tracked as a push/pop stack via `unhandled_csi`").
    #[test]
    fn unhandled_csi_tracks_kitty_level_through_parser() {
        let mut parser =
            vt100::Parser::new_with_callbacks(24, 80, 0, GutterCallbacks::new(true));

        // Child enables kitty at level 1.
        parser.process(b"\x1b[>1u");
        assert_eq!(parser.callbacks().kitty_state.current(), KittyLevel::Kitty(1));

        // Nest a deeper level.
        parser.process(b"\x1b[>5u");
        assert_eq!(parser.callbacks().kitty_state.current(), KittyLevel::Kitty(5));

        // Pop returns to the previous level.
        parser.process(b"\x1b[<u");
        assert_eq!(parser.callbacks().kitty_state.current(), KittyLevel::Kitty(1));

        // Pop to empty → legacy.
        parser.process(b"\x1b[<u");
        assert_eq!(parser.callbacks().kitty_state.current(), KittyLevel::Legacy);
    }

    /// With the outer terminal unable to source kitty, the child's enable is
    /// neutralised through the real callback path — the case-B clamp.
    #[test]
    fn clamp_neutralises_enable_through_parser() {
        let mut parser =
            vt100::Parser::new_with_callbacks(24, 80, 0, GutterCallbacks::new(false));
        parser.process(b"\x1b[>1u");
        assert_eq!(
            parser.callbacks().kitty_state.current(),
            KittyLevel::Legacy,
            "outer can't source kitty → child stays legacy"
        );
    }

    /// A non-kitty unhandled CSI (e.g. a stray `CSI > 0 c` device attributes
    /// query, or a non-`u` final) must not touch the kitty stack.
    #[test]
    fn non_kitty_csi_leaves_stack_untouched() {
        let mut parser =
            vt100::Parser::new_with_callbacks(24, 80, 0, GutterCallbacks::new(true));
        // CSI > 0 c — secondary device attributes, NOT kitty.
        parser.process(b"\x1b[>0c");
        assert_eq!(parser.callbacks().kitty_state.current(), KittyLevel::Legacy);
    }
}
