//! The input modes vt100 absorbs into its own screen state, mirrored out to the
//! real terminal (ADR-022).
//!
//! Application cursor keys (DECCKM), application keypad and bracketed paste change
//! how the *terminal* encodes the keys it sends, but the child's request terminates
//! in gutter's parser: vt100 implements all three, so they never surface through
//! `unhandled_csi` and the keyboard-mode relay (ADR-021) never sees them. There is
//! no callback to hook either, so the render loop polls the live screen once a frame
//! and emits only on an edge — the same shape ADR-012 uses for the alt screen.
//!
//! `vt100::Screen::input_mode_diff` looks like exactly this helper and is a trap: it
//! emits the mouse protocol mode and encoding alongside the three modes here, and a
//! mouse mode reaching the real terminal would set up a second authority over state
//! ADR-005 owns. The diff below is hand-rolled for that reason and covers three
//! modes, permanently.
//!
//! The byte forms are vt100's own (`vt100-0.16.2/src/term.rs`), so anything gutter
//! emits its parser would accept back. Note the keypad pair are plain escapes —
//! DECKPAM is `ESC =`, not a DECSET.

const APPLICATION_CURSOR_ON: &[u8] = b"\x1b[?1h";
const APPLICATION_CURSOR_OFF: &[u8] = b"\x1b[?1l";
const APPLICATION_KEYPAD_ON: &[u8] = b"\x1b=";
const APPLICATION_KEYPAD_OFF: &[u8] = b"\x1b>";
const BRACKETED_PASTE_ON: &[u8] = b"\x1b[?2004h";
const BRACKETED_PASTE_OFF: &[u8] = b"\x1b[?2004l";

/// What gutter has told the outer terminal, for the three modes it mirrors.
///
/// Held on the renderer beside `cursor_visible` and `outer_alt_active` rather than
/// diffed against the `prev` parser: `contents_formatted` — what `sync_prev` replays
/// — deliberately excludes the input modes, so `prev`'s flags sit at their defaults
/// for ever and a diff against it would re-emit every frame.
#[derive(Debug, Default, Clone)]
pub struct ModeMirror {
    application_cursor: bool,
    application_keypad: bool,
    bracketed_paste: bool,
}

impl ModeMirror {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether gutter has told the outer terminal to bracket its pastes. The input
    /// scanner gates its paste state on this rather than on the child's live mode: it
    /// answers "could the terminal have produced these guard bytes?", which is the
    /// question a guard-shaped byte run actually poses.
    #[must_use]
    pub fn bracketed_paste(&self) -> bool {
        self.bracketed_paste
    }

    /// The bytes to emit for whatever changed since the last poll, marking the new
    /// state mirrored. Empty on a frame that changed nothing, which is nearly all of
    /// them.
    #[must_use]
    pub fn take_pending(&mut self, screen: &vt100::Screen) -> Vec<u8> {
        let mut out = Vec::new();
        edge(
            &mut self.application_cursor,
            screen.application_cursor(),
            APPLICATION_CURSOR_ON,
            APPLICATION_CURSOR_OFF,
            &mut out,
        );
        edge(
            &mut self.application_keypad,
            screen.application_keypad(),
            APPLICATION_KEYPAD_ON,
            APPLICATION_KEYPAD_OFF,
            &mut out,
        );
        edge(
            &mut self.bracketed_paste,
            screen.bracketed_paste(),
            BRACKETED_PASTE_ON,
            BRACKETED_PASTE_OFF,
            &mut out,
        );
        out
    }

    /// The bytes that hand the terminal back at its defaults: an off form for each
    /// mode currently mirrored on, and nothing for the ones gutter never set
    /// (ADR-010). gutter restores to the default rather than to whatever the terminal
    /// had before it launched — reading that would need a `DECRQM` round trip on the
    /// fd the input thread owns.
    #[must_use]
    pub fn reset_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        if self.application_cursor {
            out.extend_from_slice(APPLICATION_CURSOR_OFF);
        }
        if self.application_keypad {
            out.extend_from_slice(APPLICATION_KEYPAD_OFF);
        }
        if self.bracketed_paste {
            out.extend_from_slice(BRACKETED_PASTE_OFF);
        }
        out
    }

    /// Forget everything mirrored, after a park has emitted the off forms.
    ///
    /// This is where the poll-diff earns its keep: resume needs no replay list and no
    /// `rearm` counterpart to [`crate::cursor::CursorShape`]'s, because the child's
    /// live modes are still on the screen and the next frame's poll finds them
    /// disagreeing with a cleared mirror.
    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

/// Emit one mode's transition, if there is one.
fn edge(mirrored: &mut bool, live: bool, on: &[u8], off: &[u8], out: &mut Vec<u8>) {
    if live == *mirrored {
        return;
    }
    out.extend_from_slice(if live { on } else { off });
    *mirrored = live;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A parser fed `bytes`, to poll as the render loop polls the child's screen.
    fn screen_after(bytes: &[u8]) -> vt100::Parser {
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(bytes);
        parser
    }

    #[test]
    fn each_mode_emits_on_its_edge_and_never_repeats() {
        for (set, on, clear, off) in [
            (&b"\x1b[?1h"[..], &b"\x1b[?1h"[..], &b"\x1b[?1l"[..], &b"\x1b[?1l"[..]),
            (b"\x1b=", b"\x1b=", b"\x1b>", b"\x1b>"),
            (b"\x1b[?2004h", b"\x1b[?2004h", b"\x1b[?2004l", b"\x1b[?2004l"),
        ] {
            let mut mirror = ModeMirror::new();
            let mut parser = screen_after(set);
            assert_eq!(mirror.take_pending(parser.screen()), on);
            assert!(
                mirror.take_pending(parser.screen()).is_empty(),
                "an unchanged mode emits nothing further"
            );

            parser.process(clear);
            assert_eq!(mirror.take_pending(parser.screen()), off);
            assert!(mirror.take_pending(parser.screen()).is_empty());
        }
    }

    /// The block's order, pinned: cursor, keypad, paste. Nothing depends on it, but a
    /// reordering should be a visible test change rather than a silent one.
    #[test]
    fn all_three_in_one_frame_emit_in_a_fixed_order() {
        let mut mirror = ModeMirror::new();
        let parser = screen_after(b"\x1b[?2004h\x1b=\x1b[?1h");
        assert_eq!(
            mirror.take_pending(parser.screen()),
            b"\x1b[?1h\x1b=\x1b[?2004h"
        );
    }

    /// `ESC c` replaces vt100's screen wholesale, clearing all three — so the next
    /// poll sees three off edges with no special case for RIS anywhere.
    #[test]
    fn ris_clears_all_three_on_the_next_poll() {
        let mut mirror = ModeMirror::new();
        let mut parser = screen_after(b"\x1b[?1h\x1b=\x1b[?2004h");
        assert!(!mirror.take_pending(parser.screen()).is_empty());

        parser.process(b"\x1bc");
        assert_eq!(
            mirror.take_pending(parser.screen()),
            b"\x1b[?1l\x1b>\x1b[?2004l"
        );
    }

    /// A mouse mode is not this mirror's business, in any parameter combination —
    /// the invariant `input_mode_diff` would break.
    #[test]
    fn mouse_modes_are_not_mirrored() {
        let mut mirror = ModeMirror::new();
        let parser = screen_after(b"\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h\x1b[?9h");
        assert!(mirror.take_pending(parser.screen()).is_empty());
    }

    #[test]
    fn reset_covers_only_what_was_mirrored_on() {
        let mut mirror = ModeMirror::new();
        assert!(
            mirror.reset_bytes().is_empty(),
            "a child that set nothing leaves nothing to undo"
        );

        let parser = screen_after(b"\x1b[?1h\x1b[?2004h");
        let _ = mirror.take_pending(parser.screen());
        assert_eq!(
            mirror.reset_bytes(),
            b"\x1b[?1l\x1b[?2004l",
            "the keypad was never set, so it is never reset"
        );
    }

    /// The scanner's paste gate reads what was mirrored, not what the child asked
    /// for, so it tracks the emitted edges exactly.
    #[test]
    fn the_paste_flag_follows_what_was_mirrored() {
        let mut mirror = ModeMirror::new();
        assert!(!mirror.bracketed_paste());

        let mut parser = screen_after(b"\x1b[?2004h");
        let _ = mirror.take_pending(parser.screen());
        assert!(mirror.bracketed_paste());

        parser.process(b"\x1b[?2004l");
        let _ = mirror.take_pending(parser.screen());
        assert!(!mirror.bracketed_paste());
    }

    /// Park clears the mirror after emitting the offs; resume re-derives from the
    /// child's live screen, which is why there is no replay list.
    #[test]
    fn clear_makes_the_next_poll_re_emit_the_childs_live_modes() {
        let mut mirror = ModeMirror::new();
        let parser = screen_after(b"\x1b[?1h\x1b=");
        let _ = mirror.take_pending(parser.screen());

        mirror.clear();
        assert!(mirror.reset_bytes().is_empty());
        assert_eq!(mirror.take_pending(parser.screen()), b"\x1b[?1h\x1b=");
    }
}
