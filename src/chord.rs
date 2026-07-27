//! The reserved `--resize-key` chord and the byte forms it matches.
//!
//! gutter reserves exactly one chord from the child (PRD 0001, Feature 2);
//! everything else is only interpreted while resize mode is active. The input
//! path is raw bytes (ADR-020), so a chord is a base key plus a modifier mask
//! that knows how to render itself as the byte forms a terminal can send it in.
//!
//! One physical key has several encodings depending on what the child has asked
//! the terminal for (ADR-021): the legacy control byte, kitty's
//! `CSI <code> ; <mods> u`, and xterm's modifyOtherKeys
//! `CSI 27 ; <mods> ; <code> ~`. All three are matched; only the press forms,
//! because a release must not toggle the mode back out (ADR-016).

/// Modifier bits, kitty's convention: the wire parameter is `1 + mask`.
const SHIFT: u8 = 1;
const ALT: u8 = 2;
const CTRL: u8 = 4;

/// The base key of a chord.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChordKey {
    Char(char),
    Esc,
    Tab,
    Enter,
    /// F1–F12. Only ever unmodified — [`parse_chord`] rejects a modified
    /// spelling, because terminals stop agreeing on the byte forms there.
    F(u8),
}

/// A single key chord. `Eq` so it can live in `Config`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chord {
    key: ChordKey,
    /// `SHIFT | ALT | CTRL`.
    mods: u8,
}

impl Default for Chord {
    /// Ctrl-\ — rare in TUIs, a distinct byte on legacy terminals, not a tmux
    /// prefix (PRD 0001, Feature 2 "Entering").
    fn default() -> Self {
        Self {
            key: ChordKey::Char('\\'),
            mods: CTRL,
        }
    }
}

impl Chord {
    /// A bare Escape, in every form a terminal can report it. The in-mode exit
    /// key (ADR-016), which is not the configurable chord.
    pub const ESC: Chord = Chord { key: ChordKey::Esc, mods: 0 };

    /// The single legacy byte this chord produces, when it has one. The
    /// classifier scans an ordinary byte run for exactly this.
    pub fn single_byte(&self) -> Option<u8> {
        match self.legacy_bytes()?.as_slice() {
            [b] => Some(*b),
            _ => None,
        }
    }

    /// Whether one scanned unit is this chord.
    ///
    /// Press only: a kitty report carrying an event sub-parameter of `2`
    /// (repeat) or `3` (release) is not the chord, or a held key would toggle
    /// the mode on every repeat and its own key-up would undo the press that
    /// entered it (ADR-016).
    pub fn matches(&self, unit: &[u8]) -> bool {
        // The Unicode codepoint the CSI-u forms report this key as.
        let code = match self.key {
            ChordKey::F(n) => return f_key_forms(n).contains(&unit),
            ChordKey::Char(c) => c.to_ascii_lowercase() as u32,
            ChordKey::Esc => 27,
            ChordKey::Tab => 9,
            ChordKey::Enter => 13,
        };
        if self.legacy_bytes().is_some_and(|b| b == unit) {
            return true;
        }
        matches_kitty(unit, code, self.mods) || matches_modify_other_keys(unit, code, self.mods)
    }

    /// The classic byte form, when the modifier set has one. A Shift chord has
    /// none: the legacy tables cannot express it.
    fn legacy_bytes(&self) -> Option<Vec<u8>> {
        if self.mods & SHIFT != 0 {
            return None;
        }
        let ctrl = self.mods & CTRL != 0;
        let base: Vec<u8> = match self.key {
            // Esc, Tab and Enter are already control bytes; Ctrl adds nothing
            // the legacy tables can express.
            ChordKey::Esc if !ctrl => vec![0x1b],
            ChordKey::Tab if !ctrl => vec![0x09],
            ChordKey::Enter if !ctrl => vec![0x0d],
            ChordKey::Char(c) if ctrl => vec![c0_byte(c)?],
            ChordKey::Char(c) => {
                let mut buf = [0u8; 4];
                c.encode_utf8(&mut buf).as_bytes().to_vec()
            }
            _ => return None,
        };
        Some(if self.mods & ALT != 0 {
            let mut out = vec![0x1b];
            out.extend_from_slice(&base);
            out
        } else {
            base
        })
    }
}

/// The C0 control byte `Ctrl` plus `c` produces. The digit spellings xterm also
/// accepts fall out of the same table, which is why `--resize-key ctrl-4` and
/// `--resize-key ctrl-\` match each other for free: on a legacy terminal they
/// genuinely are the same key.
fn c0_byte(c: char) -> Option<u8> {
    Some(match c.to_ascii_lowercase() {
        'a'..='z' => c.to_ascii_lowercase() as u8 - b'a' + 1,
        '@' | '2' => 0x00,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' => 0x1e,
        '_' | '7' => 0x1f,
        '?' | '8' => 0x7f,
        _ => return None,
    })
}

/// kitty's `CSI <code> [; <mods+1> [: <event>]] u`.
fn matches_kitty(unit: &[u8], code: u32, mods: u8) -> bool {
    let Some(body) = csi_body(unit, b'u') else {
        return false;
    };
    let mut fields = body.split(|&b| b == b';');
    if fields.next().and_then(number) != Some(code) {
        return false;
    }
    match fields.next() {
        None => mods == 0,
        Some(mod_field) => {
            if fields.next().is_some() {
                return false;
            }
            let mut parts = mod_field.split(|&b| b == b':');
            let mods_ok = parts.next().and_then(number) == Some(mods as u32 + 1);
            // Absent or `1` is a press; `2` (repeat) and `3` (release) are not.
            let press = match parts.next() {
                None => true,
                Some(ev) => number(ev) == Some(1),
            };
            mods_ok && press && parts.next().is_none()
        }
    }
}

/// xterm modifyOtherKeys: `CSI 27 ; <mods+1> ; <code> ~`.
fn matches_modify_other_keys(unit: &[u8], code: u32, mods: u8) -> bool {
    let Some(body) = csi_body(unit, b'~') else {
        return false;
    };
    let fields: Vec<&[u8]> = body.split(|&b| b == b';').collect();
    fields.len() == 3
        && number(fields[0]) == Some(27)
        && number(fields[1]) == Some(mods as u32 + 1)
        && number(fields[2]) == Some(code)
}

/// The parameter body of a CSI sequence ending in `final_byte`.
fn csi_body(unit: &[u8], final_byte: u8) -> Option<&[u8]> {
    unit.strip_prefix(b"\x1b[")?.strip_suffix(&[final_byte])
}

/// A non-empty run of ASCII digits.
fn number(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() || !bytes.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

/// The byte forms an unmodified F-key arrives in. The xterm/`xterm-256color`
/// table, which iTerm2, kitty, Ghostty, WezTerm, Alacritty, foot and
/// Terminal.app all follow. F1–F4 have two historical spellings and terminals
/// differ on which they send, so both are accepted. The gaps at 16 and 22 are
/// real — getting them wrong is the classic F-key bug, so the numbers are
/// written out rather than computed.
fn f_key_forms(n: u8) -> &'static [&'static [u8]] {
    match n {
        1 => &[b"\x1bOP", b"\x1b[11~"],
        2 => &[b"\x1bOQ", b"\x1b[12~"],
        3 => &[b"\x1bOR", b"\x1b[13~"],
        4 => &[b"\x1bOS", b"\x1b[14~"],
        5 => &[b"\x1b[15~"],
        6 => &[b"\x1b[17~"],
        7 => &[b"\x1b[18~"],
        8 => &[b"\x1b[19~"],
        9 => &[b"\x1b[20~"],
        10 => &[b"\x1b[21~"],
        11 => &[b"\x1b[23~"],
        12 => &[b"\x1b[24~"],
        _ => &[],
    }
}

/// Parse a `--resize-key` value into a [`Chord`].
///
/// Grammar (case-insensitive modifiers): `[<mod>-]... <key>` where `<mod>` is
/// `ctrl`|`c`, `alt`|`meta`|`m`, `shift`|`s`, and `<key>` is a single character,
/// `esc`, `tab`, `enter`, `space`, or `f1`..`f12`. Examples: `ctrl-\`, `c-g`,
/// `alt-r`, `\`, `f5`.
///
/// Peels a recognised modifier word off the front, one `-`-delimited segment at
/// a time; the first segment that isn't a modifier word (including the whole
/// remainder once no `-` is left) is the key. That is what makes `"-"` parse as
/// the bare key `Char('-')` (no leading segment matches) and `"ctrl--"` parse as
/// `ctrl` + key `"-"` (the second segment, empty before its own `-`, doesn't
/// match a modifier word either, so the loop stops and hands `"-"` to the key
/// parser) — rather than a naive full split, which would drop the literal `-`.
pub fn parse_chord(s: &str) -> Result<Chord, String> {
    let err = || format!("gutter: invalid --resize-key '{s}'");

    let mut mods = 0u8;
    let mut rest = s;
    while let Some((word, tail)) = rest.split_once('-') {
        let bit = match word.to_ascii_lowercase().as_str() {
            "ctrl" | "c" => CTRL,
            "alt" | "meta" | "m" => ALT,
            "shift" | "s" => SHIFT,
            _ => break,
        };
        mods |= bit;
        rest = tail;
    }

    let key = parse_key(rest).ok_or_else(err)?;
    // A modified F-key spelling would parse and then never match: the byte forms
    // are exactly where terminals stop agreeing. Reject it at startup instead.
    if mods != 0 && matches!(key, ChordKey::F(_)) {
        return Err(err());
    }
    Ok(Chord { key, mods })
}

/// Parse the key portion of a `--resize-key` chord (everything after the last
/// modifier segment).
fn parse_key(s: &str) -> Option<ChordKey> {
    let mut chars = s.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return Some(ChordKey::Char(c));
    }
    match s.to_ascii_lowercase().as_str() {
        "esc" => Some(ChordKey::Esc),
        "tab" => Some(ChordKey::Tab),
        "enter" => Some(ChordKey::Enter),
        "space" => Some(ChordKey::Char(' ')),
        _ => {
            let rest = s.strip_prefix(['f', 'F'])?;
            let n: u8 = rest.parse().ok()?;
            (1..=12).contains(&n).then_some(ChordKey::F(n))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_ctrl_backslash_and_its_byte_is_0x1c() {
        let d = Chord::default();
        assert_eq!(d.single_byte(), Some(0x1c));
        assert!(d.matches(&[0x1c]));
    }

    #[test]
    fn parse_roundtrip() {
        assert_eq!(
            parse_chord("ctrl-\\").unwrap(),
            Chord { key: ChordKey::Char('\\'), mods: CTRL }
        );
        assert_eq!(
            parse_chord("c-g").unwrap(),
            Chord { key: ChordKey::Char('g'), mods: CTRL }
        );
        assert_eq!(
            parse_chord("alt-r").unwrap(),
            Chord { key: ChordKey::Char('r'), mods: ALT }
        );
        assert_eq!(
            parse_chord("esc").unwrap(),
            Chord { key: ChordKey::Esc, mods: 0 }
        );
        assert_eq!(
            parse_chord("\\").unwrap(),
            Chord { key: ChordKey::Char('\\'), mods: 0 }
        );
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse_chord("ctrl-").is_err());
        assert!(parse_chord("wat-x").is_err());
        assert!(parse_chord("f99").is_err());
        assert!(parse_chord("").is_err());
        // A modified F-key would parse and then never match reliably.
        assert!(parse_chord("shift-f5").is_err());
    }

    #[test]
    fn ctrl_backslash_and_ctrl_4_are_the_same_key() {
        for spelling in ["ctrl-\\", "ctrl-4"] {
            let c = parse_chord(spelling).unwrap();
            assert!(c.matches(&[0x1c]), "{spelling} must match the FS byte");
            assert!(!c.matches(b"4"), "{spelling} must not match a plain '4'");
        }
    }

    #[test]
    fn ctrl_letter_is_its_control_byte() {
        assert!(parse_chord("ctrl-o").unwrap().matches(&[0x0f]));
        assert!(parse_chord("c-g").unwrap().matches(&[0x07]));
        assert!(!parse_chord("ctrl-o").unwrap().matches(b"o"));
    }

    #[test]
    fn alt_char_carries_the_esc_prefix() {
        let c = parse_chord("alt-r").unwrap();
        assert!(c.matches(b"\x1br"));
        assert!(!c.matches(b"r"));
    }

    #[test]
    fn ctrl_backslash_matches_the_csi_u_forms() {
        let c = Chord::default();
        // kitty: codepoint 92, modifier parameter 1 + ctrl(4).
        assert!(c.matches(b"\x1b[92;5u"));
        assert!(c.matches(b"\x1b[92;5:1u"), "an explicit press event still fires");
        assert!(!c.matches(b"\x1b[92;5:2u"), "a repeat must not toggle the mode");
        assert!(!c.matches(b"\x1b[92;5:3u"), "a release must not toggle the mode");
        // modifyOtherKeys.
        assert!(c.matches(b"\x1b[27;5;92~"));
        // Neighbours that must not match.
        assert!(!c.matches(b"\x1b[92;2u"), "wrong modifier");
        assert!(!c.matches(b"\x1b[93;5u"), "wrong codepoint");
        assert!(!c.matches(b"\x1b[92u"), "no modifier reported");
    }

    #[test]
    fn unmodified_chord_matches_the_bare_csi_u_form() {
        let c = parse_chord("g").unwrap();
        assert!(c.matches(b"g"));
        assert!(c.matches(b"\x1b[103u"));
        assert!(!c.matches(b"\x1b[103;5u"));
    }

    #[test]
    fn f_key_forms_are_the_xterm_table() {
        assert!(parse_chord("f5").unwrap().matches(b"\x1b[15~"));
        let f1 = parse_chord("f1").unwrap();
        assert!(f1.matches(b"\x1bOP"));
        assert!(f1.matches(b"\x1b[11~"));
        let f12 = parse_chord("f12").unwrap();
        assert!(f12.matches(b"\x1b[24~"));
        assert!(!f12.matches(b"\x1b[22~"), "22 is a gap in the xterm table");
        assert!(!parse_chord("f6").unwrap().matches(b"\x1b[16~"));
    }

    #[test]
    fn esc_chord_is_the_bare_escape_byte() {
        let c = parse_chord("esc").unwrap();
        assert_eq!(c.single_byte(), Some(0x1b));
        assert!(c.matches(&[0x1b]));
        assert!(c.matches(b"\x1b[27u"), "kitty reports Escape as codepoint 27");
    }

    #[test]
    fn a_multi_byte_chord_has_no_single_byte_form() {
        assert_eq!(parse_chord("alt-r").unwrap().single_byte(), None);
        assert_eq!(parse_chord("f5").unwrap().single_byte(), None);
    }
}
