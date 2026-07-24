//! The input scanner: raw outer-terminal bytes in, [`Token`]s out. See ADR-020.
//!
//! gutter reads the outer tty itself and forwards what it reads to the child
//! untouched. The scanner exists only to find the few things gutter cannot be
//! transparent about: SGR-1006 mouse reports (whose coordinates need margin
//! translation, ADR-005), the reserved resize chord and the in-mode resize keys
//! (ADR-016), and the bracketed-paste guards that switch all of that off.
//! Everything else is forwarded byte-identically.
//!
//! Pure: no I/O, no clock, no terminal. The scanner knows neither the chord nor
//! whether resize mode is active — the render thread's classifier owns all
//! splitting of an ordinary byte run, because it is the thing that knows the
//! mode and can re-read it mid-run.
//!
//! An escape sequence cut short by the end of a read is withheld in `pending`
//! and completed by the next chunk; the render loop arms the ESC-hold deadline
//! ([`ESC_HOLD`]) whenever [`Scanner::holding`] is true and calls
//! [`Scanner::flush`] when it expires.

use std::time::Duration;

/// How long an incomplete escape sequence is withheld before it is flushed
/// verbatim. The one added latency in the passthrough design: a lone Escape
/// keypress waits this long, because nothing else distinguishes it from the
/// start of a mouse report. Sits between tmux's 10 ms and Neovim's 50 ms; raise
/// it if split sequences over a slow link turn up.
pub const ESC_HOLD: Duration = Duration::from_millis(25);

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;

/// Upper bound on an accumulated CSI before it is abandoned and forwarded
/// verbatim. No real input CSI comes close; this only stops a wedged terminal
/// growing `pending` without bound.
const CSI_CAP: usize = 128;

/// One unit of scanned input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    /// A run of ordinary bytes containing no `ESC`. Splittable — the classifier
    /// walks it looking for the chord byte, and byte by byte while resize mode
    /// is active.
    Text(Vec<u8>),
    /// Exactly one complete escape sequence. Atomic — matched as a whole against
    /// the chord and in-mode tables, or forwarded whole. Never split.
    Seq(Vec<u8>),
    /// A bracketed paste: the guards and everything between them. Forwarded
    /// verbatim and never inspected — not for the chord, not for the in-mode
    /// table, not for mouse. The guards carry no meaning for the child that
    /// asked for them unless they arrive with their content, so they are
    /// forwarded on the same terms rather than offered to the classifier.
    Paste(Vec<u8>),
    /// Bytes of an OSC/DCS/APC/PM/SOS string sequence, guards included. Forwarded
    /// verbatim and never inspected: a payload is arbitrary data (base64, a
    /// colour spec) that can hold any byte, so matching it against the chord or
    /// the in-mode table would step the band on a terminal's reply.
    Str(Vec<u8>),
    /// A complete SGR-1006 report, coordinates still in physical columns.
    Mouse(MouseReport),
}

impl Token {
    /// The bytes this token carries, for the forwarding path and the
    /// byte-identity property. `Mouse` carries none — it is consumed by the gate.
    #[cfg(test)]
    pub fn payload(&self) -> &[u8] {
        match self {
            Token::Text(b) | Token::Seq(b) | Token::Paste(b) | Token::Str(b) => b,
            Token::Mouse(_) => &[],
        }
    }
}

/// One SGR-1006 mouse report, still in physical (outer-terminal) coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseReport {
    /// The SGR button byte verbatim, including the modifier bits.
    pub button: u16,
    /// 0-based physical column (the wire form is 1-based; the scanner subtracts).
    pub col: u16,
    /// 0-based physical row.
    pub row: u16,
    /// True when the final byte was `m` (release) rather than `M`.
    pub release: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Esc,
    Csi,
    Ss3,
    /// Inside an OSC/DCS/APC/PM/SOS payload. Deliberately does not buffer: a
    /// string sequence is never a mouse report and never a chord, so its bytes
    /// are forwarded as they arrive and a long reply is never held.
    Str,
    StrEsc,
}

/// The input state machine. See the module docs.
pub struct Scanner {
    state: State,
    /// Withheld bytes of an escape sequence that has not completed yet.
    pending: Vec<u8>,
    /// Ordinary bytes accumulated since the last emitted token.
    run: Vec<u8>,
    /// Whether gutter has told the outer terminal to bracket its pastes (ADR-022).
    /// Only a terminal that was sent `?2004h` can have produced the guards, so a
    /// guard-shaped run is ordinary input until it has been.
    paste_guards: bool,
    in_paste: bool,
}

impl Default for Scanner {
    fn default() -> Self {
        Self::new()
    }
}

impl Scanner {
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            pending: Vec::new(),
            run: Vec::new(),
            paste_guards: false,
            in_paste: false,
        }
    }

    /// Tell the scanner whether the mode mirror currently has bracketed paste on
    /// (ADR-022). The render loop sets this from its own mirror before every
    /// [`feed`], never from the child's live mode: gating on the child's would let a
    /// literal `ESC [ 200 ~` in ordinary input open a paste in the window between the
    /// child setting the mode and gutter mirroring it.
    ///
    /// Turning it off ends any open paste at once. Waiting for a closing guard the
    /// terminal may already have decided not to send risks a stuck state; the exposure
    /// this trades for is the tail of a paste the child stopped protecting mid-way.
    ///
    /// [`feed`]: Scanner::feed
    pub fn set_paste_guards(&mut self, enabled: bool) {
        self.paste_guards = enabled;
        if !enabled {
            self.in_paste = false;
        }
    }

    /// Whether an incomplete sequence is being withheld. The render loop arms
    /// the ESC-hold deadline on this after every [`feed`].
    ///
    /// [`feed`]: Scanner::feed
    pub fn holding(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Scan one chunk, appending tokens to `out`.
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<Token>) {
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            i += 1;
            match self.state {
                State::Ground => {
                    if b == ESC {
                        self.emit_run(out);
                        self.pending.push(ESC);
                        self.state = State::Esc;
                    } else {
                        self.run.push(b);
                    }
                }
                State::Esc => match b {
                    b'[' => {
                        self.pending.push(b);
                        self.state = State::Csi;
                    }
                    b'O' => {
                        self.pending.push(b);
                        self.state = State::Ss3;
                    }
                    b']' | b'P' | b'^' | b'_' | b'X' => {
                        self.run.append(&mut self.pending);
                        self.run.push(b);
                        self.state = State::Str;
                    }
                    ESC => {
                        // The held ESC was a bare Escape keypress after all; the
                        // new one starts a fresh sequence.
                        self.pending.clear();
                        self.emit_loose(vec![ESC], out);
                        self.pending.push(ESC);
                    }
                    _ => {
                        // ESC + byte: Alt+<key>, one atomic two-byte unit.
                        self.pending.push(b);
                        let unit = std::mem::take(&mut self.pending);
                        self.emit_seq(unit, out);
                        self.state = State::Ground;
                    }
                },
                State::Csi => {
                    if (0x20..=0x3f).contains(&b) {
                        self.pending.push(b);
                        if self.pending.len() > CSI_CAP {
                            let unit = std::mem::take(&mut self.pending);
                            self.emit_seq(unit, out);
                            self.state = State::Ground;
                        }
                    } else if (0x40..=0x7e).contains(&b) {
                        self.pending.push(b);
                        let unit = std::mem::take(&mut self.pending);
                        self.classify_csi(unit, out);
                        self.state = State::Ground;
                    } else {
                        // A C0 control (or DEL) aborts the sequence exactly as it
                        // would in the child's own parser. Forward the fragment
                        // and re-read this byte from Ground.
                        let unit = std::mem::take(&mut self.pending);
                        self.emit_seq(unit, out);
                        self.state = State::Ground;
                        i -= 1;
                    }
                }
                State::Ss3 => {
                    self.pending.push(b);
                    let unit = std::mem::take(&mut self.pending);
                    self.emit_seq(unit, out);
                    self.state = State::Ground;
                }
                State::Str => {
                    self.run.push(b);
                    if b == BEL {
                        self.emit_run(out);
                        self.state = State::Ground;
                    } else if b == ESC {
                        self.state = State::StrEsc;
                    }
                }
                State::StrEsc => {
                    self.run.push(b);
                    if b == b'\\' {
                        self.emit_run(out);
                        self.state = State::Ground;
                    } else {
                        self.state = State::Str;
                    }
                }
            }
        }
        self.emit_run(out);
    }

    /// The ESC-hold expired: emit whatever is withheld, verbatim and in order,
    /// as one unit, and return to `Ground`. Never split, never reinterpreted.
    pub fn flush(&mut self, out: &mut Vec<Token>) {
        self.emit_run(out);
        if !self.pending.is_empty() {
            let held = std::mem::take(&mut self.pending);
            self.emit_loose(held, out);
            self.state = State::Ground;
        }
    }

    /// Close off the accumulated run — ordinary bytes, or the part of a string
    /// sequence seen so far.
    fn emit_run(&mut self, out: &mut Vec<Token>) {
        if self.run.is_empty() {
            return;
        }
        let run = std::mem::take(&mut self.run);
        match self.state {
            State::Str | State::StrEsc => out.push(Token::Str(run)),
            _ => self.emit_loose(run, out),
        }
    }

    /// Emit bytes the classifier may walk byte by byte.
    fn emit_loose(&self, bytes: Vec<u8>, out: &mut Vec<Token>) {
        out.push(if self.in_paste {
            Token::Paste(bytes)
        } else {
            Token::Text(bytes)
        });
    }

    /// Emit one atomic escape-sequence unit.
    fn emit_seq(&self, bytes: Vec<u8>, out: &mut Vec<Token>) {
        out.push(if self.in_paste {
            Token::Paste(bytes)
        } else {
            Token::Seq(bytes)
        });
    }

    /// Decide what a completed CSI is. The paste gate comes first and is
    /// blanket: between the guards nothing is extracted and nothing is dropped,
    /// because pasted text is data, not input protocol. A paste can only open
    /// while the mirror has `?2004` on — otherwise the guards are just bytes.
    fn classify_csi(&mut self, seq: Vec<u8>, out: &mut Vec<Token>) {
        let last = seq.len() - 1;
        let final_byte = seq[last];
        let body = &seq[2..last];

        if self.in_paste {
            if final_byte == b'~' && body == b"201" {
                self.in_paste = false;
            }
            out.push(Token::Paste(seq));
            return;
        }
        if self.paste_guards && final_byte == b'~' && body == b"200" {
            self.in_paste = true;
            out.push(Token::Paste(seq));
            return;
        }
        if body.first() == Some(&b'<') && (final_byte == b'M' || final_byte == b'm') {
            // Anything shaped like an SGR report is gutter's to consume:
            // forwarding a half-parsed one would hand the child untranslated
            // physical coordinates, the exact bug the extraction prevents.
            if let Some(report) = parse_sgr(&body[1..], final_byte == b'm') {
                out.push(Token::Mouse(report));
            }
            return;
        }
        out.push(Token::Seq(seq));
    }
}

/// Parse the `b ; x ; y` parameters of an SGR-1006 report into a
/// [`MouseReport`], or `None` when they are not exactly three in-range decimals
/// with 1-based coordinates.
fn parse_sgr(params: &[u8], release: bool) -> Option<MouseReport> {
    let mut fields = params.split(|&b| b == b';');
    let button = decimal(fields.next()?)?;
    let x = decimal(fields.next()?)?;
    let y = decimal(fields.next()?)?;
    if fields.next().is_some() || x < 1 || y < 1 {
        return None;
    }
    Some(MouseReport {
        button,
        col: x - 1,
        row: y - 1,
        release,
    })
}

/// A non-empty run of ASCII digits as a `u16`, or `None`.
fn decimal(bytes: &[u8]) -> Option<u16> {
    if bytes.is_empty() {
        return None;
    }
    let mut n: u16 = 0;
    for &b in bytes {
        let d = (b as char).to_digit(10)? as u16;
        n = n.checked_mul(10)?.checked_add(d)?;
    }
    Some(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Scan one chunk from a fresh scanner.
    fn scan(bytes: &[u8]) -> Vec<Token> {
        let mut s = Scanner::new();
        let mut out = Vec::new();
        s.feed(bytes, &mut out);
        out
    }

    /// The concatenated payloads of a token stream — what actually reaches the
    /// child, mouse reports excluded.
    fn payloads(tokens: &[Token]) -> Vec<u8> {
        tokens.iter().flat_map(|t| t.payload().to_vec()).collect()
    }

    #[test]
    fn ordinary_text_is_one_run() {
        assert_eq!(scan(b"hello"), vec![Token::Text(b"hello".to_vec())]);
    }

    #[test]
    fn each_escape_family_is_one_seq() {
        // CSI, with and without parameters.
        assert_eq!(scan(b"\x1b[15~"), vec![Token::Seq(b"\x1b[15~".to_vec())]);
        assert_eq!(scan(b"\x1b[3~"), vec![Token::Seq(b"\x1b[3~".to_vec())]);
        assert_eq!(scan(b"\x1b[H"), vec![Token::Seq(b"\x1b[H".to_vec())]);
        assert_eq!(scan(b"\x1b[1;5C"), vec![Token::Seq(b"\x1b[1;5C".to_vec())]);
        // SS3.
        assert_eq!(scan(b"\x1bOP"), vec![Token::Seq(b"\x1bOP".to_vec())]);
        // ESC + char (Alt+r).
        assert_eq!(scan(b"\x1br"), vec![Token::Seq(b"\x1br".to_vec())]);
    }

    #[test]
    fn string_sequences_are_forwarded_not_held() {
        // OSC, BEL-terminated.
        let osc = b"\x1b]11;rgb:2e2e/3434/3e3e\x07";
        let mut s = Scanner::new();
        let mut out = Vec::new();
        s.feed(osc, &mut out);
        assert_eq!(payloads(&out), osc.to_vec());
        assert!(!s.holding(), "a string sequence is never withheld");

        // DCS, ST-terminated.
        let dcs = b"\x1bP1$r0m\x1b\\";
        let mut s = Scanner::new();
        let mut out = Vec::new();
        s.feed(dcs, &mut out);
        assert_eq!(payloads(&out), dcs.to_vec());
        assert!(!s.holding());
    }

    /// A string payload is arbitrary data. An OSC 52 answer's base64 routinely
    /// carries `h`, `l`, `+` and `=`, and nothing stops a terminal putting a
    /// `0x1c` in one — none of it may reach the classifier.
    #[test]
    fn string_payloads_are_never_offered_to_the_classifier() {
        let osc = b"\x1b]52;c;aGVsbG8rbGw=\x07";
        assert_eq!(scan(osc), vec![Token::Str(osc.to_vec())]);

        let with_chord = b"\x1bP\x1c\x1b\\";
        assert_eq!(scan(with_chord), vec![Token::Str(with_chord.to_vec())]);

        // Ground bytes on either side stay ordinary and matchable.
        assert_eq!(
            scan(b"a\x1b]0;t\x07b"),
            vec![
                Token::Text(b"a".to_vec()),
                Token::Str(b"\x1b]0;t\x07".to_vec()),
                Token::Text(b"b".to_vec()),
            ]
        );

        // A payload split across reads stays Str on both sides.
        let mut s = Scanner::new();
        let mut out = Vec::new();
        s.feed(b"\x1b]52;c;aGVs", &mut out);
        s.feed(b"bG8=\x07", &mut out);
        assert!(
            out.iter().all(|t| matches!(t, Token::Str(_))),
            "a split payload must not surface as Text: {out:?}"
        );
        assert_eq!(payloads(&out), b"\x1b]52;c;aGVsbG8=\x07".to_vec());
    }

    #[test]
    fn bare_esc_is_withheld_until_flush() {
        let mut s = Scanner::new();
        let mut out = Vec::new();
        s.feed(b"\x1b", &mut out);
        assert!(out.is_empty(), "a lone ESC is ambiguous and must be withheld");
        assert!(s.holding());
        s.flush(&mut out);
        assert_eq!(out, vec![Token::Text(vec![0x1b])]);
        assert!(!s.holding());
    }

    #[test]
    fn double_esc_releases_the_first() {
        let mut s = Scanner::new();
        let mut out = Vec::new();
        s.feed(b"\x1b\x1b", &mut out);
        assert_eq!(out, vec![Token::Text(vec![0x1b])]);
        assert!(s.holding(), "the second ESC is still ambiguous");
    }

    #[test]
    fn mouse_press_is_extracted_with_zero_based_coords() {
        assert_eq!(
            scan(b"\x1b[<0;42;7M"),
            vec![Token::Mouse(MouseReport {
                button: 0,
                col: 41,
                row: 6,
                release: false
            })]
        );
    }

    #[test]
    fn mouse_release_uses_lowercase_final() {
        assert_eq!(
            scan(b"\x1b[<0;42;7m"),
            vec![Token::Mouse(MouseReport {
                button: 0,
                col: 41,
                row: 6,
                release: true
            })]
        );
    }

    #[test]
    fn mouse_button_byte_keeps_modifier_bits() {
        // Shift-click: button 0 | shift 4.
        let Token::Mouse(r) = scan(b"\x1b[<4;10;5M")[0].clone() else {
            panic!("expected a mouse token")
        };
        assert_eq!(r.button, 4, "the modifier bits survive verbatim");
    }

    #[test]
    fn malformed_mouse_reports_are_dropped() {
        // Two parameters, not three.
        assert!(scan(b"\x1b[<99M").is_empty());
        // A zero coordinate (SGR is 1-based).
        assert!(scan(b"\x1b[<0;0;5M").is_empty());
        // Out of u16 range.
        assert!(scan(b"\x1b[<0;99999;5M").is_empty());
        // Four parameters.
        assert!(scan(b"\x1b[<0;1;2;3M").is_empty());
    }

    #[test]
    fn split_mouse_report_yields_one_token_at_every_boundary() {
        let seq = b"\x1b[<0;42;7M";
        for cut in 1..seq.len() {
            let mut s = Scanner::new();
            let mut out = Vec::new();
            s.feed(&seq[..cut], &mut out);
            s.feed(&seq[cut..], &mut out);
            assert_eq!(
                out,
                vec![Token::Mouse(MouseReport {
                    button: 0,
                    col: 41,
                    row: 6,
                    release: false
                })],
                "split at {cut} must still yield one report"
            );
        }
    }

    #[test]
    fn csi_abandoned_by_a_control_byte_forwards_the_prefix() {
        let out = scan(b"\x1b[1;5\x03");
        assert_eq!(payloads(&out), b"\x1b[1;5\x03".to_vec());
        assert_eq!(
            out.last(),
            Some(&Token::Text(vec![0x03])),
            "the offending byte is re-read from Ground"
        );
    }

    #[test]
    fn overlong_csi_is_abandoned_at_the_cap() {
        let mut seq = b"\x1b[".to_vec();
        seq.extend(std::iter::repeat_n(b'1', CSI_CAP + 4));
        let out = scan(&seq);
        assert_eq!(payloads(&out), seq, "the fragment is forwarded verbatim");
    }

    #[test]
    fn paste_guards_switch_extraction_off_and_back_on() {
        let mut s = Scanner::new();
        s.set_paste_guards(true);
        let mut out = Vec::new();
        s.feed(b"\x1b[200~", &mut out);
        // A well-formed mouse report inside a paste is forwarded, not extracted.
        s.feed(b"\x1b[<0;10;5M", &mut out);
        // A malformed one is forwarded too, not dropped.
        s.feed(b"\x1b[<99M", &mut out);
        // As is a literal chord byte.
        s.feed(b"a\x1cb", &mut out);
        s.feed(b"\x1b[201~", &mut out);
        // After the close guard, extraction resumes.
        s.feed(b"\x1b[<0;10;5M", &mut out);

        let (after, guarded) = out.split_last().unwrap();
        assert!(
            guarded.iter().all(|t| matches!(t, Token::Paste(_))),
            "the guards and their content are all Paste, classifier-invisible: {out:?}"
        );
        assert!(matches!(after, Token::Mouse(_)), "{out:?}");
        assert_eq!(
            payloads(&out),
            b"\x1b[200~\x1b[<0;10;5M\x1b[<99Ma\x1cb\x1b[201~".to_vec(),
            "the pasted bytes reach the child verbatim and the guards survive"
        );
        assert_eq!(
            out.iter().filter(|t| matches!(t, Token::Mouse(_))).count(),
            1,
            "only the post-guard report is extracted"
        );
    }

    /// The gate is a gate: until gutter has mirrored `?2004h`, a guard-shaped run is
    /// input like any other, and a `0x1C` in it still reaches the classifier as the
    /// chord. Anything else would let a program that never asked for paste protection
    /// disable the reserved chord by sending the guard bytes itself.
    #[test]
    fn guards_are_ordinary_bytes_until_the_mirror_is_on() {
        let span = b"\x1b[200~a\x1cb\x1b[<0;10;5M\x1b[201~";
        let out = scan(span);
        assert!(
            !out.iter().any(|t| matches!(t, Token::Paste(_))),
            "no paste state without the mirror: {out:?}"
        );
        assert_eq!(
            out.iter().filter(|t| matches!(t, Token::Mouse(_))).count(),
            1,
            "the mouse report is extracted as usual"
        );
        assert!(
            payloads(&out).contains(&0x1c),
            "the chord byte reaches the classifier"
        );
    }

    /// The child turning paste mode off re-arms normal scanning at once, rather than
    /// waiting for a closing guard the terminal may never send.
    #[test]
    fn clearing_the_mirror_ends_an_open_paste() {
        let mut s = Scanner::new();
        s.set_paste_guards(true);
        let mut out = Vec::new();
        s.feed(b"\x1b[200~pasted", &mut out);
        assert!(out.iter().any(|t| matches!(t, Token::Paste(_))));

        out.clear();
        s.set_paste_guards(false);
        s.feed(b"\x1b[<0;10;5M", &mut out);
        assert_eq!(
            out.iter().filter(|t| matches!(t, Token::Mouse(_))).count(),
            1,
            "extraction resumes immediately: {out:?}"
        );
    }

    /// The dead keys this slice brings back: each must scan as one forwarded
    /// unit, never a mouse report and never anything gutter consumes.
    #[test]
    fn the_silently_dead_keys_scan_as_plain_sequences() {
        for seq in [
            &b"\x1b[3~"[..],  // Delete
            b"\x1b[H",        // Home
            b"\x1b[F",        // End
            b"\x1b[1~",       // Home (alternate)
            b"\x1b[4~",       // End (alternate)
            b"\x1b[5~",       // PageUp
            b"\x1b[6~",       // PageDown
            b"\x1b[2~",       // Insert
            b"\x1b[Z",        // Shift+Tab
            b"\x1bOP",        // F1
            b"\x1bOQ",        // F2
            b"\x1bOR",        // F3
            b"\x1bOS",        // F4
            b"\x1b[15~",      // F5
            b"\x1b[17~",      // F6
            b"\x1b[18~",      // F7
            b"\x1b[19~",      // F8
            b"\x1b[20~",      // F9
            b"\x1b[21~",      // F10
            b"\x1b[23~",      // F11
            b"\x1b[24~",      // F12
        ] {
            assert_eq!(
                scan(seq),
                vec![Token::Seq(seq.to_vec())],
                "{seq:?} must forward as one sequence"
            );
        }
    }

    /// Splitting a fixture at every index must not change what reaches the child.
    #[test]
    fn chunk_boundary_sweep_preserves_the_stream() {
        let fixtures: [&[u8]; 6] = [
            b"abc\x1b[15~def",
            b"\x1bOA\x1b[1;5C",
            b"\x1b]52;c;aGVsbG8=\x07x",
            b"\x1b[200~pasted \x1c text\x1b[201~",
            b"\x1bra\x1b[3~",
            b"hello\x1b[<0;9;2Mworld",
        ];
        for fixture in fixtures {
            let whole = scan(fixture);
            for cut in 0..=fixture.len() {
                let mut s = Scanner::new();
                let mut out = Vec::new();
                s.feed(&fixture[..cut], &mut out);
                s.feed(&fixture[cut..], &mut out);
                s.flush(&mut out);
                assert_eq!(
                    payloads(&out),
                    payloads(&whole),
                    "fixture {fixture:?} split at {cut} changed the forwarded bytes"
                );
                let mice: Vec<&Token> =
                    out.iter().filter(|t| matches!(t, Token::Mouse(_))).collect();
                let whole_mice: Vec<&Token> =
                    whole.iter().filter(|t| matches!(t, Token::Mouse(_))).collect();
                assert_eq!(mice, whole_mice, "fixture {fixture:?} split at {cut}");
            }
        }
    }

    /// The same sweep with the gate on. A fresh `Scanner` has the gate off, so the
    /// sweep above never opens a paste; this one does, and pins that a guard cut in
    /// two still opens and closes paste state — a split guard that failed to open one
    /// would leave the mouse report inside it extractable.
    #[test]
    fn chunk_boundary_sweep_holds_inside_a_paste() {
        // (fed, what reaches the child, how many reports are extracted — all of them
        // from outside the guards).
        let fixtures: [(&[u8], &[u8], usize); 3] = [
            (
                b"\x1b[200~pasted \x1c text\x1b[201~",
                b"\x1b[200~pasted \x1c text\x1b[201~",
                0,
            ),
            (
                b"\x1b[200~\x1b[<0;10;5M\x1b[<99M\x1b[201~",
                b"\x1b[200~\x1b[<0;10;5M\x1b[<99M\x1b[201~",
                0,
            ),
            (
                b"\x1b[200~in\x1b[201~\x1b[<0;9;2M",
                b"\x1b[200~in\x1b[201~",
                1,
            ),
        ];
        for (fixture, forwarded, mice) in fixtures {
            for cut in 0..=fixture.len() {
                let mut s = Scanner::new();
                s.set_paste_guards(true);
                let mut out = Vec::new();
                s.feed(&fixture[..cut], &mut out);
                s.feed(&fixture[cut..], &mut out);
                s.flush(&mut out);
                assert_eq!(
                    payloads(&out),
                    forwarded.to_vec(),
                    "fixture {fixture:?} split at {cut} changed the forwarded bytes"
                );
                assert_eq!(
                    out.iter().filter(|t| matches!(t, Token::Mouse(_))).count(),
                    mice,
                    "fixture {fixture:?} split at {cut} extracted the wrong reports: {out:?}"
                );
            }
        }
    }

    proptest! {
        /// Byte identity: with no SGR mouse report to extract, every byte fed in
        /// comes out, in order, however the input is chunked.
        #[test]
        fn arbitrary_bytes_survive_any_chunking(
            input in proptest::collection::vec(any::<u8>(), 0..256),
            chunk in 1usize..17,
        ) {
            // `ESC [ <` is the only prefix the scanner ever consumes; exclude it
            // so the property is about forwarding, not extraction.
            prop_assume!(!input.windows(3).any(|w| w == [0x1b, b'[', b'<']));

            let mut s = Scanner::new();
            let mut out = Vec::new();
            for part in input.chunks(chunk.max(1)) {
                s.feed(part, &mut out);
            }
            s.flush(&mut out);
            prop_assert_eq!(payloads(&out), input);
        }
    }
}
