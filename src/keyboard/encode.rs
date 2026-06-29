//! Pure `KeyEvent` → bytes encoder. See ADR-002 (keyboard always re-encoded)
//! and ADR-003 (kitty levels).
//!
//! Two byte forms, chosen by the child's negotiated [`KittyLevel`]:
//!
//! - **Legacy** — classic VT byte tables: `\r` for Enter, `\x1b[A` for Up, a
//!   control byte for Ctrl-letter, raw UTF-8 for text. Plain Enter and
//!   Shift+Enter both collapse to `\r`; legacy cannot tell them apart, which is
//!   why a child needs kitty to distinguish them.
//! - **Kitty** — special keys take the `CSI codepoint [; modifier] u` form, so
//!   Enter is `CSI 13 u` and Shift+Enter is `CSI 13 ; 2 u`. Printable text still
//!   passes through as raw UTF-8 under `DISAMBIGUATE_ESCAPE_CODES`, so normal
//!   typing is level-independent.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// The keyboard protocol level the child currently expects — the top of the
/// kitty push/pop stack (see `kitty_state`). `Legacy` is the empty-stack /
/// no-kitty floor; `Kitty` carries the flag level the child enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KittyLevel {
    /// No kitty negotiated — classic VT byte encoding.
    Legacy,
    /// Kitty keyboard active at the child's enhancement-flag level (the `N` from
    /// `CSI > N u`). The encoder only distinguishes kitty on/off today, but the
    /// flag is carried so the negotiated level stays honest.
    Kitty(u16),
}

impl KittyLevel {
    /// Whether kitty `CSI ... u` encoding is in force.
    fn is_kitty(self) -> bool {
        matches!(self, KittyLevel::Kitty(_))
    }
}

/// The kitty modifier parameter: `1 + bitmask`, where the bitmask is
/// `shift(1) | alt(2) | ctrl(4) | super(8)`. A bare key (no modifiers) yields
/// `1`, which the encoder omits (`CSI 13 u`, not `CSI 13 ; 1 u`).
fn kitty_modifier(mods: KeyModifiers) -> u16 {
    let mut bits = 0u16;
    if mods.contains(KeyModifiers::SHIFT) {
        bits |= 1;
    }
    if mods.contains(KeyModifiers::ALT) {
        bits |= 2;
    }
    if mods.contains(KeyModifiers::CONTROL) {
        bits |= 4;
    }
    if mods.contains(KeyModifiers::SUPER) {
        bits |= 8;
    }
    1 + bits
}

/// `CSI codepoint [; modifier] u` — the kitty functional-key form. The modifier
/// is omitted when it is `1` (no modifiers), matching the protocol.
fn kitty_seq(codepoint: u16, mods: KeyModifiers) -> Vec<u8> {
    let m = kitty_modifier(mods);
    if m == 1 {
        format!("\x1b[{codepoint}u").into_bytes()
    } else {
        format!("\x1b[{codepoint};{m}u").into_bytes()
    }
}

/// The kitty functional-key codepoint for a special key. Returns `None` for keys
/// with no special codepoint (ordinary printable characters, handled separately).
fn special_codepoint(code: KeyCode) -> Option<u16> {
    Some(match code {
        KeyCode::Enter => 13,
        KeyCode::Tab => 9,
        KeyCode::Backspace => 127,
        KeyCode::Esc => 27,
        _ => return None,
    })
}

/// Encodes a decoded `KeyEvent` into the bytes to write to the PTY master at the
/// child's current [`KittyLevel`].
///
/// Returns an empty `Vec` for events that produce no bytes (key releases, and
/// keys with no byte form at either level). Callers treat an empty result as
/// "nothing to send". The encoder never panics for any `KeyCode`/`KeyModifiers`
/// combination (proptest-enforced).
pub fn encode_key(event: &KeyEvent, level: KittyLevel) -> Vec<u8> {
    // Only key *presses* (and repeats) produce bytes. Releases are dropped — the
    // kitty REPORT_EVENT_TYPES flag makes releases arrive, but gutter only
    // forwards the press/repeat as input to the child.
    if event.kind == KeyEventKind::Release {
        return Vec::new();
    }

    if level.is_kitty() {
        encode_kitty(event)
    } else {
        encode_legacy(event)
    }
}

/// Kitty `CSI ... u` encoding for special keys; raw UTF-8 (or a control byte)
/// for ordinary text, so normal typing is level-independent under
/// `DISAMBIGUATE_ESCAPE_CODES`.
fn encode_kitty(event: &KeyEvent) -> Vec<u8> {
    let mods = event.modifiers;
    // Special keys take the functional `CSI codepoint [; mod] u` form — this is
    // what lets Shift+Enter (`CSI 13;2u`) differ from Enter (`CSI 13u`).
    if let Some(codepoint) = special_codepoint(event.code) {
        return kitty_seq(codepoint, mods);
    }
    match event.code {
        // Arrows keep their legacy CSI form; the child needs no `u` disambiguation
        // for them.
        KeyCode::Up | KeyCode::Down | KeyCode::Right | KeyCode::Left => arrow_bytes(event.code),
        // Ordinary printable characters: a Ctrl-letter still maps to its control
        // byte (so Ctrl-C reaches the child as 0x03); otherwise raw UTF-8.
        KeyCode::Char(c) => char_bytes(c, mods),
        // Anything else has no byte form.
        _ => Vec::new(),
    }
}

/// Legacy VT encoding — the classic byte tables. Shift+Enter and plain Enter
/// both collapse to `\r` here (legacy cannot disambiguate them).
fn encode_legacy(event: &KeyEvent) -> Vec<u8> {
    let mods = event.modifiers;
    match event.code {
        KeyCode::Char(c) => char_bytes(c, mods),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up | KeyCode::Down | KeyCode::Right | KeyCode::Left => arrow_bytes(event.code),
        _ => Vec::new(),
    }
}

/// Bytes for a printable character: a Ctrl-letter folds to its control byte
/// (Ctrl-A = 0x01 … Ctrl-Z = 0x1a), otherwise the character's raw UTF-8.
/// Shared by both levels so ordinary typing is encoded identically.
fn char_bytes(c: char, mods: KeyModifiers) -> Vec<u8> {
    if mods.contains(KeyModifiers::CONTROL) {
        let upper = c.to_ascii_uppercase();
        if upper.is_ascii_alphabetic() {
            return vec![(upper as u8) - b'A' + 1];
        }
        // Ctrl with a non-letter (e.g. Ctrl-Space) has no simple control byte;
        // fall through to the raw character.
    }
    let mut buf = [0u8; 4];
    c.encode_utf8(&mut buf).as_bytes().to_vec()
}

/// The legacy CSI arrow bytes, used at both levels.
fn arrow_bytes(code: KeyCode) -> Vec<u8> {
    match code {
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventState;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    fn release(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: KeyEventState::NONE,
        }
    }

    /// The full encoder table: Enter, Shift+Enter, ASCII, arrows, Esc, Tab,
    /// Backspace, Ctrl-C/Ctrl-D, under both `Legacy` and `Kitty` levels.
    #[test]
    fn encoder_table() {
        let kitty = KittyLevel::Kitty(1);
        let legacy = KittyLevel::Legacy;

        // --- The headline disambiguation (kitty) ---
        assert_eq!(
            encode_key(&key(KeyCode::Enter, KeyModifiers::NONE), kitty),
            b"\x1b[13u",
            "plain Enter under kitty → CSI 13 u"
        );
        assert_eq!(
            encode_key(&key(KeyCode::Enter, KeyModifiers::SHIFT), kitty),
            b"\x1b[13;2u",
            "Shift+Enter under kitty → CSI 13 ; 2 u"
        );
        assert_ne!(
            encode_key(&key(KeyCode::Enter, KeyModifiers::NONE), kitty),
            encode_key(&key(KeyCode::Enter, KeyModifiers::SHIFT), kitty),
            "under kitty the two Enters MUST be distinct"
        );

        // --- The degradation (legacy): both collapse to \r ---
        assert_eq!(
            encode_key(&key(KeyCode::Enter, KeyModifiers::NONE), legacy),
            b"\r"
        );
        assert_eq!(
            encode_key(&key(KeyCode::Enter, KeyModifiers::SHIFT), legacy),
            b"\r"
        );
        assert_eq!(
            encode_key(&key(KeyCode::Enter, KeyModifiers::NONE), legacy),
            encode_key(&key(KeyCode::Enter, KeyModifiers::SHIFT), legacy),
            "under legacy the two Enters MUST be the same byte"
        );

        // --- Ordinary typing (level-independent) ---
        for level in [legacy, kitty] {
            assert_eq!(encode_key(&key(KeyCode::Char('a'), KeyModifiers::NONE), level), b"a");
            assert_eq!(encode_key(&key(KeyCode::Up, KeyModifiers::NONE), level), b"\x1b[A");
            assert_eq!(encode_key(&key(KeyCode::Down, KeyModifiers::NONE), level), b"\x1b[B");
            assert_eq!(encode_key(&key(KeyCode::Right, KeyModifiers::NONE), level), b"\x1b[C");
            assert_eq!(encode_key(&key(KeyCode::Left, KeyModifiers::NONE), level), b"\x1b[D");
            assert_eq!(
                encode_key(&key(KeyCode::Char('c'), KeyModifiers::CONTROL), level),
                vec![0x03],
                "Ctrl-C → 0x03"
            );
            assert_eq!(
                encode_key(&key(KeyCode::Char('d'), KeyModifiers::CONTROL), level),
                vec![0x04],
                "Ctrl-D → 0x04"
            );
        }

        // --- Tab/Backspace/Esc: legacy bytes vs kitty u-form ---
        assert_eq!(encode_key(&key(KeyCode::Tab, KeyModifiers::NONE), legacy), b"\t");
        assert_eq!(encode_key(&key(KeyCode::Backspace, KeyModifiers::NONE), legacy), vec![0x7f]);
        assert_eq!(encode_key(&key(KeyCode::Esc, KeyModifiers::NONE), legacy), vec![0x1b]);
        assert_eq!(encode_key(&key(KeyCode::Tab, KeyModifiers::NONE), kitty), b"\x1b[9u");
        assert_eq!(encode_key(&key(KeyCode::Backspace, KeyModifiers::NONE), kitty), b"\x1b[127u");
        assert_eq!(encode_key(&key(KeyCode::Esc, KeyModifiers::NONE), kitty), b"\x1b[27u");
    }

    /// Releases produce no bytes at any level (kitty REPORT_EVENT_TYPES makes
    /// them arrive; gutter forwards only presses/repeats to the child).
    #[test]
    fn releases_produce_nothing() {
        assert!(encode_key(&release(KeyCode::Char('a')), KittyLevel::Legacy).is_empty());
        assert!(encode_key(&release(KeyCode::Enter), KittyLevel::Kitty(1)).is_empty());
    }

    /// The kitty modifier parameter maths: 1 + shift|alt|ctrl|super.
    #[test]
    fn modifier_param_maths() {
        assert_eq!(kitty_modifier(KeyModifiers::NONE), 1);
        assert_eq!(kitty_modifier(KeyModifiers::SHIFT), 2);
        assert_eq!(kitty_modifier(KeyModifiers::ALT), 3);
        assert_eq!(kitty_modifier(KeyModifiers::CONTROL), 5);
        assert_eq!(
            kitty_modifier(KeyModifiers::SHIFT | KeyModifiers::CONTROL),
            6
        );
    }

    mod prop {
        use super::*;
        use proptest::prelude::*;

        /// A strategy over the `KeyCode`s we encode, plus an arbitrary printable
        /// char.
        fn key_code() -> impl Strategy<Value = KeyCode> {
            prop_oneof![
                Just(KeyCode::Enter),
                Just(KeyCode::Tab),
                Just(KeyCode::Backspace),
                Just(KeyCode::Esc),
                Just(KeyCode::Up),
                Just(KeyCode::Down),
                Just(KeyCode::Left),
                Just(KeyCode::Right),
                any::<char>().prop_map(KeyCode::Char),
            ]
        }

        /// A strategy over arbitrary modifier combinations (the four kitty bits).
        fn modifiers() -> impl Strategy<Value = KeyModifiers> {
            any::<u8>().prop_map(|bits| {
                let mut m = KeyModifiers::NONE;
                if bits & 1 != 0 {
                    m |= KeyModifiers::SHIFT;
                }
                if bits & 2 != 0 {
                    m |= KeyModifiers::ALT;
                }
                if bits & 4 != 0 {
                    m |= KeyModifiers::CONTROL;
                }
                if bits & 8 != 0 {
                    m |= KeyModifiers::SUPER;
                }
                m
            })
        }

        proptest! {
            /// `encode_key` never panics for any code/modifier/level.
            #[test]
            fn never_panics(code in key_code(), mods in modifiers(), kitty in any::<bool>()) {
                let level = if kitty { KittyLevel::Kitty(1) } else { KittyLevel::Legacy };
                let ev = KeyEvent::new(code, mods);
                let _ = encode_key(&ev, level);
            }

            /// A printable char (no modifiers) always yields non-empty bytes at
            /// either level.
            #[test]
            fn printable_char_is_non_empty(c in any::<char>(), kitty in any::<bool>()) {
                let level = if kitty { KittyLevel::Kitty(1) } else { KittyLevel::Legacy };
                let ev = KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
                prop_assert!(!encode_key(&ev, level).is_empty());
            }

            /// THE load-bearing property: under kitty, Shift+Enter and plain
            /// Enter map to DIFFERENT bytes; under legacy, to the SAME bytes.
            #[test]
            fn enter_disambiguation_holds(flags in any::<u16>()) {
                let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
                let shift_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);

                // Kitty: distinct.
                let k = KittyLevel::Kitty(flags);
                prop_assert_ne!(encode_key(&enter, k), encode_key(&shift_enter, k));

                // Legacy: identical.
                let l = KittyLevel::Legacy;
                prop_assert_eq!(encode_key(&enter, l), encode_key(&shift_enter, l));
            }
        }
    }
}
