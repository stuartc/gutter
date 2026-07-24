//! The child's keyboard-mode requests, forwarded out to the real terminal, and
//! the record of what has to be undone afterwards (ADR-021).
//!
//! gutter implements no keyboard protocol. It recognises a short, closed list of
//! sequences whose only effect is on how the terminal *encodes* the keys it sends,
//! passes them straight out, and keeps just enough to put the terminal back the way
//! it found it. Every other unhandled CSI is dropped, as it always was.
//!
//! Two rules here are load-bearing.
//!
//! **Canonical bytes, never a param join.** `unhandled_csi` hands over parsed
//! parameters rather than the bytes that produced them, and vte pushes a zero for a
//! parameter the child never wrote — so `CSI ? u` and `CSI ? 0 u` arrive
//! identically. Re-serialising the parameters would send the query out as
//! `CSI ? 0 u`, whose meaning is undefined, and a bare pop out as `CSI < 0 u`,
//! which pops nothing. Each matched shape therefore emits a fixed byte string for
//! that shape.
//!
//! **Default deny.** Relaying a sequence with a display effect writes outside the
//! band, where the repaint has no model of it; failing to relay one merely leaves a
//! feature unavailable. The two failure modes are not symmetric, so the list stays
//! short and widens only on evidence.

/// The kitty capability query, relayed verbatim. A question, not a mode change:
/// the terminal answers on gutter's input fd or (the protocol's "no") stays silent,
/// and either way the answer reaches the child through the raw passthrough.
const KITTY_QUERY: &[u8] = b"\x1b[?u";

/// The modifyOtherKeys reset. gutter turns the mode off rather than restoring the
/// terminal's configured default, because off is the state it can be sure of.
const MODIFY_OTHER_KEYS_OFF: &[u8] = b"\x1b[>4;0m";

/// The set form that returns the current level to no enhancement flags — the one
/// piece of kitty state a pop cannot restore, because there is no level beneath it.
const KITTY_SET_NONE: &[u8] = b"\x1b[=0;1u";

/// One relayed kitty mode change, kept so teardown can undo it and resume can
/// replay it. Stores the exact bytes that went out, so no protocol semantics — flag
/// defaults, or the set form's assign/or/and-not modes — ever need re-deriving.
#[derive(Debug, Clone, PartialEq, Eq)]
enum KittyOp {
    /// `CSI > flags u` — one level pushed onto the terminal's stack.
    Push(Vec<u8>),
    /// `CSI = flags ; mode u` — flags changed on the current level, stack untouched.
    Set(Vec<u8>),
}

impl KittyOp {
    fn bytes(&self) -> &[u8] {
        match self {
            KittyOp::Push(b) | KittyOp::Set(b) => b,
        }
    }
}

/// What the child has asked the real terminal for, and nothing else.
#[derive(Debug, Default)]
pub struct KeyModeRelay {
    /// Relayed kitty ops, oldest first. The `Push` count is the stack depth the
    /// child has open on the real terminal.
    kitty: Vec<KittyOp>,
    /// The last `CSI > 4 ; Pv m` relayed with a non-zero `Pv`; `None` once the child
    /// turns modifyOtherKeys off or restores the terminal's default itself, so there
    /// is nothing owed.
    modify_other_keys: Option<Vec<u8>>,
}

impl KeyModeRelay {
    pub fn new() -> Self {
        Self::default()
    }

    /// Match one unhandled CSI against the allowlist. On a match, records what the
    /// change implies for the undo log and returns the canonical bytes to relay;
    /// otherwise `None`, and the sequence is dropped.
    ///
    /// The parameters are matched, never re-serialised — see the module docs.
    pub fn observe(
        &mut self,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) -> Option<Vec<u8>> {
        // A second intermediate is not part of any shape on the list. Requiring an
        // intermediate at all is what excludes `CSI u` — the ANSI restore-cursor,
        // which would move the real cursor mid-frame and corrupt the band.
        if i2.is_some() {
            return None;
        }
        match (i1?, c) {
            // kitty push: CSI > flags u.
            (b'>', 'u') => {
                let bytes = format!("\x1b[>{}u", param(params, 0)).into_bytes();
                self.kitty.push(KittyOp::Push(bytes.clone()));
                Some(bytes)
            }
            // kitty pop: CSI < n u, clamped to the depth gutter itself relayed. A
            // level pushed before gutter started belongs to whoever pushed it.
            (b'<', 'u') => {
                let n = param(params, 0).max(1).min(self.depth());
                if n == 0 {
                    return None;
                }
                self.pop(n);
                Some(format!("\x1b[<{n}u").into_bytes())
            }
            // kitty set: CSI = flags ; mode u. A set replaces the previous set at the
            // same level rather than stacking with it.
            (b'=', 'u') => {
                let mode = match param(params, 1) {
                    0 => 1,
                    m => m,
                };
                let bytes = format!("\x1b[={};{}u", param(params, 0), mode).into_bytes();
                while matches!(self.kitty.last(), Some(KittyOp::Set(_))) {
                    self.kitty.pop();
                }
                self.kitty.push(KittyOp::Set(bytes.clone()));
                Some(bytes)
            }
            // kitty query: CSI ? u. Changes nothing, so records nothing.
            (b'?', 'u') => Some(KITTY_QUERY.to_vec()),
            // xterm modifyOtherKeys. Only `Pp == 4` is relayed; its siblings (0
            // modifyKeyboard, 1 modifyCursorKeys, 2 modifyFunctionKeys) each change a
            // different key class and nothing gutter wraps has been shown to need them.
            (b'>', 'm') if param(params, 0) == 4 => {
                // The one-parameter form is `CSI > 4 m`, "restore the terminal's own
                // default" — distinguishable from `CSI > 4 ; Pv m` because vte pushes
                // nothing for a second parameter that was never written.
                if params.len() < 2 {
                    self.modify_other_keys = None;
                    return Some(b"\x1b[>4m".to_vec());
                }
                let pv = param(params, 1);
                let bytes = format!("\x1b[>4;{pv}m").into_bytes();
                self.modify_other_keys = (pv != 0).then(|| bytes.clone());
                Some(bytes)
            }
            _ => None,
        }
    }

    /// The bytes that put the terminal back: pop the child's levels, clear a
    /// depth-zero set, then turn modifyOtherKeys off. Empty when the child asked for
    /// nothing — gutter never resets a mode it did not set (ADR-010).
    pub fn reset_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let depth = self.depth();
        if depth > 0 {
            out.extend_from_slice(format!("\x1b[<{depth}u").as_bytes());
        }
        // A set applied on top of a pushed level is already undone by the pop above,
        // which restores the level beneath it, flags and all. Only a set made at depth
        // zero has nothing to fall back to.
        if matches!(self.kitty.first(), Some(KittyOp::Set(_))) {
            out.extend_from_slice(KITTY_SET_NONE);
        }
        if self.modify_other_keys.is_some() {
            out.extend_from_slice(MODIFY_OTHER_KEYS_OFF);
        }
        out
    }

    /// The bytes that re-apply everything, oldest first. Order matters: a set mutates
    /// whichever level was on top when it was issued, so replaying out of order lands
    /// the flags on the wrong level.
    pub fn replay_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for op in &self.kitty {
            out.extend_from_slice(op.bytes());
        }
        if let Some(bytes) = &self.modify_other_keys {
            out.extend_from_slice(bytes);
        }
        out
    }

    /// The kitty stack depth the child has open on the real terminal.
    fn depth(&self) -> u16 {
        self.kitty
            .iter()
            .filter(|op| matches!(op, KittyOp::Push(_)))
            .count() as u16
    }

    /// Drop log entries from the end until `n` pushes have gone, taking any sets that
    /// were applied on top of them with them.
    fn pop(&mut self, n: u16) {
        let mut remaining = n;
        while remaining > 0 {
            match self.kitty.pop() {
                Some(KittyOp::Push(_)) => remaining -= 1,
                Some(KittyOp::Set(_)) => {}
                None => break,
            }
        }
    }
}

/// The parameter at `index`, or `0` when the child omitted it — vte stores an
/// omitted parameter as zero and keeps no "was it written" bit, so the two cases are
/// the same value here. Sub-parameters (`4:2`) are not part of any matched shape;
/// only the first is read.
fn param(params: &[&[u16]], index: usize) -> u16 {
    params
        .get(index)
        .and_then(|p| p.first())
        .copied()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One CSI in the shape `unhandled_csi` delivers it: a name for the failure
    /// message, the first intermediate, the parameter list and the final character.
    type Case<'a> = (&'a str, Option<u8>, &'a [&'a [u16]], char);

    /// Run one `(i1, params, c)` through a fresh relay.
    fn matched(i1: Option<u8>, params: &[&[u16]], c: char) -> Option<Vec<u8>> {
        KeyModeRelay::new().observe(i1, None, params, c)
    }

    fn bytes(v: Option<Vec<u8>>) -> Vec<u8> {
        v.expect("the sequence must be admitted")
    }

    /// The allowlist, both directions, with the exact bytes pinned. The paramless
    /// rows are the ones that would rot silently: vte delivers them as `[[0]]`, so a
    /// matcher expecting an empty parameter list would never fire, and an emitter
    /// that re-serialised would send `ESC[?0u` for the query and `ESC[<0u` for the
    /// pop — the first undefined, the second a no-op.
    #[test]
    fn allowlist_admits_the_six_shapes_with_canonical_bytes() {
        let admitted: &[(Case, &[u8])] = &[
            (("kitty push", Some(b'>'), &[&[1]], 'u'), b"\x1b[>1u"),
            (("kitty pop", Some(b'<'), &[&[3]], 'u'), b"\x1b[<3u"),
            (("kitty set", Some(b'='), &[&[5], &[2]], 'u'), b"\x1b[=5;2u"),
            (("kitty query", Some(b'?'), &[&[0]], 'u'), b"\x1b[?u"),
            (("modifyOtherKeys", Some(b'>'), &[&[4], &[2]], 'm'), b"\x1b[>4;2m"),
            (("modifyOtherKeys restore", Some(b'>'), &[&[4]], 'm'), b"\x1b[>4m"),
        ];
        for ((name, i1, params, c), want) in admitted {
            // A pop needs a level outstanding before it can be relayed.
            let mut relay = KeyModeRelay::new();
            if *i1 == Some(b'<') {
                for _ in 0..3 {
                    relay.observe(Some(b'>'), None, &[&[1]], 'u');
                }
            }
            assert_eq!(
                relay.observe(*i1, None, params, *c).as_deref(),
                Some(*want),
                "{name} must relay its canonical bytes"
            );
        }
    }

    /// The paramless forms, as vte actually delivers them.
    #[test]
    fn paramless_forms_recover_their_protocol_defaults() {
        assert_eq!(
            bytes(matched(Some(b'?'), &[&[0]], 'u')),
            b"\x1b[?u",
            "the query goes out as the spec's CSI ? u, never CSI ? 0 u"
        );

        let mut relay = KeyModeRelay::new();
        relay.observe(Some(b'>'), None, &[&[1]], 'u');
        assert_eq!(
            relay.observe(Some(b'<'), None, &[&[0]], 'u').as_deref(),
            Some(b"\x1b[<1u".as_slice()),
            "a bare pop is one level, never zero"
        );

        assert_eq!(
            bytes(matched(Some(b'='), &[&[5], &[0]], 'u')),
            b"\x1b[=5;1u",
            "the set form's mode defaults to 1"
        );
    }

    /// Everything else is dropped. The near misses are the point: `CSI u` is the
    /// ANSI restore-cursor and really does reach `unhandled_csi`, and `CSI > 1 ; 2 m`
    /// is a modifyOtherKeys sibling that looks equally safe and is still not relayed.
    #[test]
    fn allowlist_denies_everything_else() {
        let denied: &[Case] = &[
            ("CSI u (SCORC)", None, &[&[0]], 'u'),
            ("secondary DA", Some(b'>'), &[&[0]], 'c'),
            ("tertiary DA", Some(b'='), &[&[0]], 'c'),
            ("SGR", None, &[&[4]], 'm'),
            ("DECSCUSR", Some(b' '), &[&[6]], 'q'),
            ("focus reporting", Some(b'?'), &[&[1004]], 'h'),
            ("modifyCursorKeys", Some(b'>'), &[&[1], &[2]], 'm'),
            ("wrong final", Some(b'>'), &[&[4], &[2]], 'x'),
        ];
        for (name, i1, params, c) in denied {
            assert_eq!(matched(*i1, params, *c), None, "{name} must not be relayed");
        }

        assert_eq!(
            KeyModeRelay::new().observe(Some(b'>'), Some(b'$'), &[&[1]], 'u'),
            None,
            "a second intermediate is not a shape on the list"
        );
    }

    /// The log is the undo, so its depth accounting is what teardown depends on.
    #[test]
    fn pushes_and_pops_track_the_depth() {
        let mut relay = KeyModeRelay::new();
        relay.observe(Some(b'>'), None, &[&[1]], 'u');
        relay.observe(Some(b'>'), None, &[&[5]], 'u');
        assert_eq!(relay.depth(), 2);
        assert_eq!(relay.reset_bytes(), b"\x1b[<2u");
        assert_eq!(relay.replay_bytes(), b"\x1b[>1u\x1b[>5u");

        relay.observe(Some(b'<'), None, &[&[0]], 'u');
        relay.observe(Some(b'<'), None, &[&[0]], 'u');
        assert_eq!(relay.depth(), 0);
        assert!(relay.reset_bytes().is_empty(), "nothing pushed, nothing to undo");
    }

    /// A pop deeper than gutter's own depth is clamped: gutter undoes its own work
    /// and never reaches into a stack it did not build.
    #[test]
    fn pop_below_the_floor_is_clamped() {
        let mut relay = KeyModeRelay::new();
        relay.observe(Some(b'>'), None, &[&[1]], 'u');
        relay.observe(Some(b'>'), None, &[&[1]], 'u');
        assert_eq!(
            relay.observe(Some(b'<'), None, &[&[3]], 'u').as_deref(),
            Some(b"\x1b[<2u".as_slice()),
            "three asked for, two outstanding, two relayed"
        );
        assert_eq!(
            relay.observe(Some(b'<'), None, &[&[1]], 'u'),
            None,
            "at depth zero a pop relays nothing at all"
        );
    }

    /// A set at depth zero is the one kitty state a pop cannot restore, so the reset
    /// clears it explicitly. A set on top of a pushed level needs no such help.
    #[test]
    fn depth_zero_set_is_reset_explicitly() {
        let mut relay = KeyModeRelay::new();
        relay.observe(Some(b'='), None, &[&[5], &[1]], 'u');
        assert_eq!(relay.reset_bytes(), b"\x1b[=0;1u");

        let mut pushed = KeyModeRelay::new();
        pushed.observe(Some(b'>'), None, &[&[1]], 'u');
        pushed.observe(Some(b'='), None, &[&[5], &[1]], 'u');
        assert_eq!(
            pushed.reset_bytes(),
            b"\x1b[<1u",
            "the pop restores the level beneath, set flags and all"
        );
        assert_eq!(
            pushed.replay_bytes(),
            b"\x1b[>1u\x1b[=5;1u",
            "replay re-applies in the order the child issued them"
        );
    }

    /// A set replaces the previous set at the same level rather than accumulating,
    /// so a child that changes flags repeatedly does not grow the log.
    #[test]
    fn a_set_replaces_the_previous_set_at_the_same_level() {
        let mut relay = KeyModeRelay::new();
        relay.observe(Some(b'='), None, &[&[1], &[1]], 'u');
        relay.observe(Some(b'='), None, &[&[3], &[1]], 'u');
        assert_eq!(relay.replay_bytes(), b"\x1b[=3;1u");
    }

    /// modifyOtherKeys has no stack: gutter tracks the last non-zero value it
    /// relayed and owes nothing once the child turns it off itself.
    #[test]
    fn modify_other_keys_reset_is_owed_only_when_set() {
        let mut relay = KeyModeRelay::new();
        relay.observe(Some(b'>'), None, &[&[4], &[2]], 'm');
        assert_eq!(relay.reset_bytes(), b"\x1b[>4;0m");

        relay.observe(Some(b'>'), None, &[&[4], &[0]], 'm');
        assert!(relay.reset_bytes().is_empty(), "the child turned it off itself");

        relay.observe(Some(b'>'), None, &[&[4], &[2]], 'm');
        relay.observe(Some(b'>'), None, &[&[4]], 'm');
        assert!(
            relay.reset_bytes().is_empty(),
            "the child restored the terminal's own default"
        );
    }

    /// The reset order: kitty first, modifyOtherKeys last.
    #[test]
    fn reset_covers_both_protocols_in_order() {
        let mut relay = KeyModeRelay::new();
        relay.observe(Some(b'>'), None, &[&[1]], 'u');
        relay.observe(Some(b'>'), None, &[&[4], &[2]], 'm');
        assert_eq!(relay.reset_bytes(), b"\x1b[<1u\x1b[>4;0m");
        assert_eq!(relay.replay_bytes(), b"\x1b[>1u\x1b[>4;2m");
    }
}
