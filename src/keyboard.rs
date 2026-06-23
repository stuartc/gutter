//! Keyboard re-encoding.
//!
//! crossterm 0.29 yields a decoded `KeyEvent` only (no raw-byte accessor), so
//! gutter ALWAYS re-encodes — never verbatim. The real encoder (legacy + kitty
//! `CSI ... u` at the child's negotiated level, clamped to the outer terminal's
//! capability) is slice 04 (ADR-002 / ADR-003).
//!
//! **THROWAWAY — DELETE IN SLICE 04.** What lives here now is a minimal legacy
//! encoding: just enough to type into the child so the binary is runnable (the
//! slice-02 input path stays this slice-01 placeholder). Encoding correctness is
//! NOT a criterion until slice 04, which replaces this wholesale.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// Encode a decoded `KeyEvent` into a minimal legacy byte form for the PTY.
///
/// Returns `None` for events that produce no bytes (key releases/repeats, keys
/// we do not handle in this throwaway encoder).
pub fn encode(event: &KeyEvent) -> Option<Vec<u8>> {
    // Only act on key *presses*. With no kitty flags pushed (slice 01),
    // releases/repeats generally do not arrive, but guard anyway.
    if event.kind == KeyEventKind::Release {
        return None;
    }

    let ctrl = event.modifiers.contains(KeyModifiers::CONTROL);

    let bytes = match event.code {
        KeyCode::Char(c) if ctrl => {
            // Ctrl-letter → control byte (Ctrl-A == 0x01, etc.).
            let upper = c.to_ascii_uppercase();
            if upper.is_ascii_alphabetic() {
                vec![(upper as u8) - b'A' + 1]
            } else {
                return None;
            }
        }
        KeyCode::Char(c) => {
            let mut s = [0u8; 4];
            c.encode_utf8(&mut s).as_bytes().to_vec()
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        _ => return None,
    };

    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke only — a letter and Enter produce *some* plausible legacy bytes.
    /// Correctness is NOT a slice-01 criterion (deleted with the encoder in
    /// slice 04); this just guards the placeholder compiles and runs.
    #[test]
    fn letter_and_enter_smoke() {
        let a = encode(&KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(a, Some(b"a".to_vec()));

        let enter = encode(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(enter, Some(b"\r".to_vec()));

        let ctrl_c = encode(&KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(ctrl_c, Some(vec![0x03]));
    }

    #[test]
    fn release_produces_nothing() {
        let ev = KeyEvent {
            code: KeyCode::Char('a'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: crossterm::event::KeyEventState::NONE,
        };
        assert_eq!(encode(&ev), None);
    }
}
