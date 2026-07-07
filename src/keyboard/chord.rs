//! The reserved resize-mode chord: parsed from `--resize-key`, matched against a
//! decoded crossterm `KeyEvent`. gutter reserves exactly one chord from the child
//! (PRD 0001, Feature 2); everything else is only interpreted while in the mode.
//!
//! On a legacy (non-kitty) outer terminal a few Ctrl chords are indistinguishable
//! from other keys because they share a byte — `ctrl-i` arrives as `Tab`, `ctrl-m`
//! as `Enter`, `ctrl-[` as `Esc` — so those make poor `--resize-key` choices:
//! `parse_chord` accepts them, but they only work reliably on a kitty-capable
//! outer terminal.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// A single key chord: a modifier set plus a key. `Eq` so it can live in `Config`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyChord {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

impl Default for KeyChord {
    /// Ctrl-\ — rare in TUIs, encodes distinctly on legacy terminals, not a tmux
    /// prefix (PRD 0001, Feature 2 "Entering").
    fn default() -> Self {
        Self { code: KeyCode::Char('\\'), mods: KeyModifiers::CONTROL }
    }
}

impl KeyChord {
    /// Whether a decoded key event is this chord. Matches on code + the three
    /// interpreted modifier bits (CONTROL/ALT/SHIFT); ignores the kitty-only
    /// SUPER/HYPER/META bits. **Kind is NOT checked here** — `matches` is pure
    /// chord identity; `classify_key` filters `KeyEventKind::Release` before
    /// calling it (a chord release must never toggle the mode).
    ///
    /// Control-letter fold: crossterm may deliver Ctrl-<letter> as the lowercase
    /// `Char` + CONTROL. We compare case-insensitively for `Char` so a chord typed
    /// as `ctrl-\` matches regardless of the reported case of the base glyph.
    ///
    /// Legacy control-byte fold: on a NON-kitty outer terminal crossterm's byte
    /// parser decodes the raw control bytes 0x1C..=0x1F as
    /// `Char('4'|'5'|'6'|'7') + CONTROL` — so Ctrl-\ (0x1C) arrives as Ctrl-4.
    /// Under kitty disambiguation (gutter pushes the flags when the outer
    /// supports them) the same key arrives as `Char('\\') + CONTROL` via
    /// `CSI 92;5u`. A CONTROL chord therefore accepts its legacy alias:
    /// `\`≡`4`, `]`≡`5`, `^`≡`6`, `_`≡`7` (both directions, so `--resize-key
    /// ctrl-4` also matches under kitty — the two keys share a byte on legacy
    /// terminals and cannot be told apart there anyway).
    pub fn matches(&self, ev: &KeyEvent) -> bool {
        let mods_eq = ev.modifiers.intersection(
            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT,
        ) == self.mods;
        if !mods_eq {
            return false;
        }
        match (self.code, ev.code) {
            (KeyCode::Char(a), KeyCode::Char(b)) => {
                a.eq_ignore_ascii_case(&b)
                    || (self.mods.contains(KeyModifiers::CONTROL)
                        && legacy_ctrl_alias(a).is_some_and(|alias| alias == b))
                    || (self.mods.contains(KeyModifiers::CONTROL)
                        && legacy_ctrl_alias(b).is_some_and(|alias| alias == a))
            }
            (a, b) => a == b,
        }
    }
}

/// The character that shares a legacy control byte with `c` (0x1C..=0x1F pairs).
/// crossterm decodes those bytes as Ctrl-4..Ctrl-7 on a non-kitty terminal.
fn legacy_ctrl_alias(c: char) -> Option<char> {
    match c {
        '\\' => Some('4'),
        ']' => Some('5'),
        '^' => Some('6'),
        '_' => Some('7'),
        '4' => Some('\\'),
        '5' => Some(']'),
        '6' => Some('^'),
        '7' => Some('_'),
        _ => None,
    }
}

/// Parse a `--resize-key` value into a [`KeyChord`].
///
/// Grammar (case-insensitive modifiers): `[<mod>-]... <key>` where `<mod>` is
/// `ctrl`|`c`, `alt`|`meta`|`m`, `shift`|`s`, and `<key>` is a single character,
/// `esc`, `tab`, `enter`, `space`, or `f1`..`f12`. Examples: `ctrl-\`, `c-g`,
/// `alt-r`, `shift-f5`, `\`.
///
/// Peels a recognised modifier word off the front, one `-`-delimited segment at
/// a time; the first segment that isn't a modifier word (including the whole
/// remainder once no `-` is left) is the key. That is what makes `"-"` parse as
/// the bare key `Char('-')` (no leading segment matches) and `"ctrl--"` parse as
/// `ctrl` + key `"-"` (the second segment, empty before its own `-`, doesn't
/// match a modifier word either, so the loop stops and hands `"-"` to the key
/// parser) — rather than a naive full split, which would drop the literal `-`.
pub fn parse_chord(s: &str) -> Result<KeyChord, String> {
    let err = || format!("gutter: invalid --resize-key '{s}'");

    let mut mods = KeyModifiers::NONE;
    let mut rest = s;
    while let Some((word, tail)) = rest.split_once('-') {
        let bit = match word.to_ascii_lowercase().as_str() {
            "ctrl" | "c" => KeyModifiers::CONTROL,
            "alt" | "meta" | "m" => KeyModifiers::ALT,
            "shift" | "s" => KeyModifiers::SHIFT,
            _ => break,
        };
        mods |= bit;
        rest = tail;
    }

    let code = parse_key(rest).ok_or_else(err)?;
    Ok(KeyChord { code, mods })
}

/// Parse the key portion of a `--resize-key` chord (everything after the last
/// modifier segment).
fn parse_key(s: &str) -> Option<KeyCode> {
    let mut chars = s.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return Some(KeyCode::Char(c));
    }
    match s.to_ascii_lowercase().as_str() {
        "esc" => Some(KeyCode::Esc),
        "tab" => Some(KeyCode::Tab),
        "enter" => Some(KeyCode::Enter),
        "space" => Some(KeyCode::Char(' ')),
        _ => {
            let rest = s.strip_prefix(['f', 'F'])?;
            let n: u8 = rest.parse().ok()?;
            (1..=12).contains(&n).then_some(KeyCode::F(n))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState};

    fn press(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent { code, modifiers: mods, kind: KeyEventKind::Press, state: KeyEventState::NONE }
    }

    #[test]
    fn default_is_ctrl_backslash() {
        let d = KeyChord::default();
        assert_eq!(d.code, KeyCode::Char('\\'));
        assert_eq!(d.mods, KeyModifiers::CONTROL);
    }

    #[test]
    fn parse_roundtrip() {
        assert_eq!(
            parse_chord("ctrl-\\").unwrap(),
            KeyChord { code: KeyCode::Char('\\'), mods: KeyModifiers::CONTROL }
        );
        assert_eq!(
            parse_chord("c-g").unwrap(),
            KeyChord { code: KeyCode::Char('g'), mods: KeyModifiers::CONTROL }
        );
        assert_eq!(
            parse_chord("alt-r").unwrap(),
            KeyChord { code: KeyCode::Char('r'), mods: KeyModifiers::ALT }
        );
        assert_eq!(
            parse_chord("shift-f5").unwrap(),
            KeyChord { code: KeyCode::F(5), mods: KeyModifiers::SHIFT }
        );
        assert_eq!(
            parse_chord("esc").unwrap(),
            KeyChord { code: KeyCode::Esc, mods: KeyModifiers::NONE }
        );
        assert_eq!(
            parse_chord("\\").unwrap(),
            KeyChord { code: KeyCode::Char('\\'), mods: KeyModifiers::NONE }
        );
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse_chord("ctrl-").is_err());
        assert!(parse_chord("wat-x").is_err());
        assert!(parse_chord("f99").is_err());
        assert!(parse_chord("").is_err());
    }

    #[test]
    fn matches_case_insensitive_char() {
        let chord = KeyChord { code: KeyCode::Char('g'), mods: KeyModifiers::CONTROL };
        assert!(chord.matches(&press(KeyCode::Char('G'), KeyModifiers::CONTROL)));
        assert!(chord.matches(&press(KeyCode::Char('g'), KeyModifiers::CONTROL)));
    }

    #[test]
    fn matches_ignores_kitty_only_mods() {
        let chord = KeyChord { code: KeyCode::Char('\\'), mods: KeyModifiers::CONTROL };
        let ev = press(
            KeyCode::Char('\\'),
            KeyModifiers::CONTROL | KeyModifiers::SUPER,
        );
        assert!(chord.matches(&ev), "SUPER must be ignored, not part of the compared bits");
    }

    #[test]
    fn matches_legacy_ctrl_backslash_alias() {
        let chord = KeyChord::default(); // Ctrl-\
        assert!(chord.matches(&press(KeyCode::Char('\\'), KeyModifiers::CONTROL)));
        assert!(chord.matches(&press(KeyCode::Char('4'), KeyModifiers::CONTROL)));
        assert!(
            !chord.matches(&press(KeyCode::Char('4'), KeyModifiers::NONE)),
            "a plain '4' with no CONTROL must not match"
        );
    }
}
