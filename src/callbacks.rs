//! The `vt100::Callbacks` impl carried by the render thread's parser — the single
//! place gutter hooks the parser.
//!
//! Independent concerns live here and must not reach into each other:
//! `unhandled_csi` touches only `cursor_shape`/`key_modes`/`replies`,
//! `copy_to_clipboard` only `clipboard_out`. Callbacks fire inside
//! `parser.process()` on the render thread, the only thread that touches the
//! parser, so the OSC-52 write runs inline on that thread. See ADR-004, ADR-009.

use std::fmt;
use std::io::{self, Write};

use crate::clipboard::forward_osc52;
use crate::cursor::{is_decscusr, CursorShape};
use crate::relay::KeyModeRelay;

/// The fixed DA1 identity gutter answers `CSI c` with (VT100 with Advanced Video
/// Option). The exact identity doesn't matter — a child gating on the DA1
/// handshake only needs *an* answer to proceed — so we send a conventional one.
const DA1_REPLY: &[u8] = b"\x1b[?1;2c";

/// The single callbacks struct the parser owns.
///
/// Carries independent concerns — the cursor shape, the clipboard sink, the
/// keyboard-mode relay and buffered device-query replies — side by side; no method
/// reads another's field.
pub struct GutterCallbacks {
    /// The child's requested cursor shape (DECSCUSR / `CSI Ps SP q`) — driven by
    /// the same `unhandled_csi` watcher (vt100 surfaces DECSCUSR as an unhandled
    /// CSI), read by the render loop to mirror the shape on the outer terminal.
    pub cursor_shape: CursorShape,
    /// The clipboard write sink. Production injects the real `/dev/tty` handle
    /// ([`crate::clipboard::open_tty_read_write`]); tests inject a buffer they read
    /// back. Baseline (diff-only) parsers get `io::sink()` — they never run the
    /// live OSC-52 path. A `Box<dyn Write>` rather than a concrete `File` so the
    /// callback is testable without a real tty. See ADR-004.
    clipboard_out: Box<dyn Write + Send>,
    /// Replies buffered for the child's device queries. gutter is the child's
    /// emulator, so it answers `CSI c` / `CSI 5 n` / `CSI 6 n` itself rather than
    /// proxying them. `unhandled_csi` only *buffers* here — the render
    /// loop drains this to the PTY master (the one writer it owns) right after
    /// `parser.process()`, so no reply leaves callbacks.
    replies: Vec<u8>,
    /// The keyboard-mode relay (ADR-021), on the live parser only. The
    /// diff-baseline parsers are rebuilt from the live grid every frame and would
    /// relay the same request again each time, so they carry `None`.
    key_modes: Option<KeyModeRelay>,
    /// Mode requests matched for relaying, buffered exactly as `replies` is: the
    /// callback owns no terminal handle, so the render loop drains this to the
    /// outer terminal right after `parser.process()`.
    relay_out: Vec<u8>,
}

impl GutterCallbacks {
    /// The live parser's callbacks: a real clipboard sink and a relay. Production
    /// passes the `/dev/tty` handle; the OSC-52 dispatch and end-to-end tests pass a
    /// recording buffer.
    pub fn live(clipboard_out: Box<dyn Write + Send>) -> Self {
        Self {
            cursor_shape: CursorShape::new(),
            clipboard_out,
            replies: Vec::new(),
            key_modes: Some(KeyModeRelay::new()),
            relay_out: Vec::new(),
        }
    }

    /// The diff-baseline parsers' callbacks: a discarding clipboard sink and no
    /// relay. They replay formatted content, never carry the live clipboard fd, and
    /// must never speak to the outer terminal.
    pub fn baseline() -> Self {
        Self {
            cursor_shape: CursorShape::new(),
            clipboard_out: Box::new(io::sink()),
            replies: Vec::new(),
            key_modes: None,
            relay_out: Vec::new(),
        }
    }

    /// Takes the device-query replies buffered since the last drain. Called by the
    /// render loop in the `Msg::Pty` arm after `parser.process()`, which writes
    /// them to the PTY master with the same `write_all` + `flush` the key/mouse
    /// paths use. Empty when the child issued no query this frame.
    pub fn drain_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.replies)
    }

    /// Throws away the buffered device-query replies. A mirror parser fed the child's
    /// bytes buffers a reply per query like any other, but only the live parser's
    /// replies reach the PTY — a long-lived mirror empties its own here instead of
    /// growing one for the life of the process.
    pub fn discard_replies(&mut self) {
        self.replies.clear();
    }

    /// Takes the mode requests matched for relaying since the last drain. Drained in
    /// the same arm as [`Self::drain_replies`] and written to the outer terminal
    /// first, so a proxied query gets the longest possible head start on its round
    /// trip. Empty when the child negotiated nothing this frame.
    pub fn drain_relay(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.relay_out)
    }

    /// The relay's record of what the child asked the outer terminal for, read by
    /// teardown and the suspend cycle. `None` on a baseline parser.
    pub fn key_modes(&self) -> Option<&KeyModeRelay> {
        self.key_modes.as_ref()
    }
}

impl Default for GutterCallbacks {
    fn default() -> Self {
        Self::baseline()
    }
}

/// Returns the reply for a bare DSR / DA1 device query, or `None` when the final
/// is not one gutter answers. Only the `i1 == None` forms are answered: the
/// secondary/tertiary DA (`CSI > c` / `CSI = c`) and private DSR carry an
/// intermediate and are out of scope. The cursor-position reply reads
/// `screen.cursor_position()` — the child's **W-grid** coordinates — 1-based per
/// the DSR spec; the band's left-margin offset lives in the render thread and
/// never reaches here, so it cannot leak into the reply.
fn device_query_reply(
    i1: Option<u8>,
    params: &[&[u16]],
    c: char,
    screen: &vt100::Screen,
) -> Option<Vec<u8>> {
    if i1.is_some() {
        return None;
    }
    let ps = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
    match c {
        // DA1: CSI c / CSI 0 c → fixed identity.
        'c' if ps == 0 => Some(DA1_REPLY.to_vec()),
        // DSR status: CSI 5 n → "terminal OK".
        'n' if ps == 5 => Some(b"\x1b[0n".to_vec()),
        // DSR cursor position: CSI 6 n → CSI row ; col R, W-grid coords, 1-based.
        'n' if ps == 6 => {
            let (row, col) = screen.cursor_position();
            Some(format!("\x1b[{};{}R", row + 1, col + 1).into_bytes())
        }
        _ => None,
    }
}

impl fmt::Debug for GutterCallbacks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The clipboard sink is a `dyn Write` (no `Debug`); elide it.
        f.debug_struct("GutterCallbacks")
            .field("cursor_shape", &self.cursor_shape)
            .field("key_modes", &self.key_modes)
            .finish_non_exhaustive()
    }
}

impl vt100::Callbacks for GutterCallbacks {
    /// vt100's catch-all for CSI sequences it doesn't implement. Three of those
    /// matter to gutter: DECSCUSR cursor-shape requests, keyboard-mode requests to
    /// relay outward, and DA1/DSR device queries to answer inward. Each routes to
    /// its own field; the clipboard is untouched here.
    ///
    /// gutter implements no keyboard protocol (ADR-020) — the relay matches a
    /// closed list of shapes and forwards canonical bytes for them, so the child and
    /// the real terminal negotiate with each other (ADR-021). The child's `CSI ? u`
    /// is passed out as a question rather than answered here; a terminal that does
    /// not speak kitty stays silent, which is the protocol's "no".
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        if is_decscusr(i1, c) {
            // vt100 doesn't implement DECSCUSR, so it lands here with the SP
            // intermediate in `i1`. Record the shape for the render loop to mirror.
            self.cursor_shape.apply_csi(params);
            return;
        }
        if let Some(bytes) = self
            .key_modes
            .as_mut()
            .and_then(|relay| relay.observe(i1, i2, params, c))
        {
            self.relay_out.extend_from_slice(&bytes);
            return;
        }
        if let Some(reply) = device_query_reply(i1, params, c, screen) {
            // gutter answers device queries itself; buffer the reply for the
            // render loop to drain to the PTY master.
            self.replies.extend_from_slice(&reply);
        }
    }

    /// The OSC-52 clipboard write. vt100/vte hands us a complete, reassembled
    /// OSC 52 with `data` still base64-encoded; we reconstruct
    /// `ESC ] 52 ; ty ; data BEL` verbatim and write it to the injected sink. The
    /// error is swallowed, never propagated: a failed clipboard write must not
    /// unwind out of `process()` and take down the render loop. See ADR-004.
    fn copy_to_clipboard(&mut self, _: &mut vt100::Screen, ty: &[u8], data: &[u8]) {
        if let Err(e) = forward_osc52(&mut self.clipboard_out, ty, data) {
            eprintln!("gutter: clipboard write failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tracks the child's DECSCUSR through the real `vt100` callback path: drive
    /// `parser.process()` with a `CSI Ps SP q` and assert the callbacks recorded a
    /// pending shape to mirror — proving DECSCUSR is observable via `unhandled_csi`.
    #[test]
    fn decscusr_tracked_through_parser() {
        let mut parser =
            vt100::Parser::new_with_callbacks(24, 80, 0, GutterCallbacks::baseline());

        // Child requests a steady bar cursor (CSI 6 SP q).
        parser.process(b"\x1b[6 q");
        assert_eq!(
            parser.callbacks_mut().cursor_shape.take_pending(),
            Some(b"\x1b[6 q".to_vec()),
            "DECSCUSR surfaces via unhandled_csi and is mirrored verbatim"
        );

        // A non-DECSCUSR unhandled CSI must NOT touch the shape (the SP
        // intermediate is required). A kitty enable is the obvious neighbour.
        parser.process(b"\x1b[>1u");
        assert_eq!(
            parser.callbacks_mut().cursor_shape.take_pending(),
            None,
            "a kitty CSI must not register as a cursor-shape change"
        );
    }

    /// DA1 (`CSI c` / `CSI 0 c`) is answered with the fixed identity through the
    /// real callback path. The secondary DA (`CSI > c`) carries an intermediate
    /// and must stay unanswered (it is not DA1).
    #[test]
    fn da1_query_buffers_fixed_identity() {
        let mut parser =
            vt100::Parser::new_with_callbacks(24, 80, 0, GutterCallbacks::baseline());

        parser.process(b"\x1b[c");
        assert_eq!(parser.callbacks_mut().drain_replies(), b"\x1b[?1;2c");

        // The explicit `CSI 0 c` form is equivalent.
        parser.process(b"\x1b[0c");
        assert_eq!(parser.callbacks_mut().drain_replies(), b"\x1b[?1;2c");

        // Secondary DA (CSI > c) is NOT DA1 — nothing buffered.
        parser.process(b"\x1b[>c");
        assert!(parser.callbacks_mut().drain_replies().is_empty());
    }

    /// DSR status (`CSI 5 n`) is answered "terminal OK" (`CSI 0 n`).
    #[test]
    fn dsr_status_buffers_ok() {
        let mut parser =
            vt100::Parser::new_with_callbacks(24, 80, 0, GutterCallbacks::baseline());
        parser.process(b"\x1b[5n");
        assert_eq!(parser.callbacks_mut().drain_replies(), b"\x1b[0n");
    }

    /// DSR cursor-position (`CSI 6 n`) is answered `CSI row ; col R` in the child's
    /// **W-grid** coordinates, 1-based. The callbacks only ever see the parser's
    /// own grid — the band's left-margin offset lives in the render thread and
    /// never reaches here — so the reply is the child's true position, never
    /// shifted by the gutter.
    #[test]
    fn cursor_position_reply_uses_w_grid_coords() {
        let mut parser =
            vt100::Parser::new_with_callbacks(24, 80, 0, GutterCallbacks::baseline());

        // Move the cursor to row 3, col 7 (1-based), then query.
        parser.process(b"\x1b[3;7H\x1b[6n");
        assert_eq!(
            parser.callbacks_mut().drain_replies(),
            b"\x1b[3;7R",
            "cursor-position reply must report the W-grid position 1-based, \
             with no left-margin offset leaked in"
        );

        // Home the cursor and re-query: the reply tracks the live position.
        parser.process(b"\x1b[H\x1b[6n");
        assert_eq!(parser.callbacks_mut().drain_replies(), b"\x1b[1;1R");
    }

    /// A parser with the live callbacks — the only ones that relay.
    fn live_parser() -> vt100::Parser<GutterCallbacks> {
        vt100::Parser::new_with_callbacks(
            24,
            80,
            0,
            GutterCallbacks::live(Box::new(io::sink())),
        )
    }

    /// The kitty keyboard query (`CSI ? u`) is relayed to the real terminal and
    /// never answered here. gutter has no capability to report — any answer it
    /// invented would be a claim about a terminal it had not asked — so the question
    /// goes out and whatever comes back (including nothing, the protocol's "no")
    /// reaches the child through the raw input passthrough.
    ///
    /// The relayed bytes are the spec's paramless `CSI ? u`. vte delivers the
    /// child's query as `params == [[0]]`, so an emitter that re-serialised its
    /// parameters would send `CSI ? 0 u`, whose meaning is undefined.
    #[test]
    fn kitty_query_is_relayed_and_never_answered() {
        let mut parser = live_parser();

        parser.process(b"\x1b[?u");
        assert_eq!(parser.callbacks_mut().drain_relay(), b"\x1b[?u");
        assert!(
            parser.callbacks_mut().drain_replies().is_empty(),
            "gutter answers the capability query itself under no circumstances"
        );
    }

    /// A paramless pop is one level, never zero. vte cannot tell `CSI < u` from
    /// `CSI < 0 u`, and a relayed `CSI < 0 u` would pop nothing — leaving the
    /// terminal in the mode the child had just left, with gutter's log saying
    /// otherwise.
    #[test]
    fn paramless_pop_relays_exactly_one_level() {
        let mut parser = live_parser();

        parser.process(b"\x1b[>1u\x1b[<u");
        assert_eq!(parser.callbacks_mut().drain_relay(), b"\x1b[>1u\x1b[<1u");
        assert!(
            parser
                .callbacks()
                .key_modes()
                .expect("the live parser relays")
                .reset_bytes()
                .is_empty(),
            "the child popped what it pushed, so teardown owes nothing"
        );
    }

    /// xterm's modifyOtherKeys — the request that makes a terminal report Shift+Enter
    /// as something other than a plain `\r`, and the reason the relay exists.
    #[test]
    fn modify_other_keys_is_relayed_verbatim() {
        let mut parser = live_parser();
        parser.process(b"\x1b[>4;2m");
        assert_eq!(parser.callbacks_mut().drain_relay(), b"\x1b[>4;2m");
    }

    /// Device queries are not the relay's business: gutter answers DA1 itself and
    /// relays nothing for it.
    #[test]
    fn device_queries_are_answered_not_relayed() {
        let mut parser = live_parser();
        parser.process(b"\x1b[c");
        assert_eq!(parser.callbacks_mut().drain_replies(), b"\x1b[?1;2c");
        assert!(parser.callbacks_mut().drain_relay().is_empty());
    }

    /// The near misses, through the real parser. `CSI u` is the ANSI restore-cursor
    /// and genuinely arrives here; relaying it would move the outer terminal's cursor
    /// mid-frame and corrupt the band. The others are a secondary DA, an ordinary
    /// SGR, and a DECSET gutter has no model of.
    #[test]
    fn sequences_off_the_allowlist_are_not_relayed() {
        let mut parser = live_parser();
        parser.process(b"\x1b[u\x1b[>0c\x1b[4m\x1b[?1004h");
        assert!(
            parser.callbacks_mut().drain_relay().is_empty(),
            "only the allowlisted shapes reach the outer terminal"
        );
    }

    /// The diff-baseline parsers are rebuilt from the live grid every frame, so a
    /// relay on one would re-send the child's whole negotiation each repaint.
    #[test]
    fn a_baseline_parser_relays_nothing() {
        let mut parser =
            vt100::Parser::new_with_callbacks(24, 80, 0, GutterCallbacks::baseline());
        parser.process(b"\x1b[>1u\x1b[?u\x1b[>4;2m");
        assert!(parser.callbacks_mut().drain_relay().is_empty());
        assert!(parser.callbacks().key_modes().is_none());
    }

    /// A test double for the OSC-52 hook: `copy_to_clipboard` records each
    /// `(ty, data)` into a `Vec` instead of writing a tty. Lets the dispatch tests
    /// drive `parser.process()` directly and assert what fired, with no PTY, no
    /// `/dev/tty`, no threads.
    #[derive(Default)]
    struct RecordingCallbacks {
        recorded: Vec<(Vec<u8>, Vec<u8>)>,
    }

    impl RecordingCallbacks {
        fn new() -> Self {
            Self::default()
        }
    }

    impl vt100::Callbacks for RecordingCallbacks {
        fn copy_to_clipboard(&mut self, _: &mut vt100::Screen, ty: &[u8], data: &[u8]) {
            self.recorded.push((ty.to_vec(), data.to_vec()));
        }
    }

    /// gutter does not parse OSC itself — it relies on vt100/vte's dispatch — so
    /// this test pins exactly what the pinned `vt100 0.16.2` / `vte 0.15.0`
    /// dispatches for each terminator and abort form. See ADR-004.
    ///
    /// Two of those forms diverge from the VT spec. These are vte gaps, not gutter
    /// bugs — gutter forwards verbatim whatever vt100 hands it:
    ///
    /// 1. **C1 ST `0x9C` is NOT an OSC terminator.** In `vte::advance_osc_string`
    ///    the raw `0x9C` byte falls into the catch-all `action_osc_put` arm — it
    ///    is appended to the OSC payload, not treated as ST. Only the two-byte
    ///    `ESC \` ST form terminates. An 8-bit-clean child emitting a bare `0x9C`
    ///    ST would have its OSC swallowed until the next real terminator.
    /// 2. **CAN `0x18` / SUB `0x1A` DISPATCH the OSC, they do not abort it.** In
    ///    `vte`, `0x18`/`0x1A` inside an OSC string call `osc_end` →
    ///    `osc_dispatch`, so a partial OSC-52 cut short by CAN is delivered with
    ///    the bytes seen so far rather than discarded.
    ///
    /// The test asserts the verified actual behaviour, so a future vte upgrade
    /// that fixes either divergence trips it and forces a conscious update. The
    /// forms gutter relies on daily — BEL, `ESC \`, and split-read reassembly
    /// across `process()` calls — all work as required.
    #[test]
    fn osc52_dispatch_confirmation_against_pinned_vte() {
        let mut parser =
            vt100::Parser::new_with_callbacks(24, 80, 0, RecordingCallbacks::new());

        // 1. BEL (0x07) terminator.
        parser.process(b"\x1b]52;c;QUJD\x07");
        // 2. ST (ESC \) terminator.
        parser.process(b"\x1b]52;c;REVG\x1b\\");
        // 3. Split form A: cut mid-payload across two process() calls — vte's
        //    osc_raw buffer reassembles across the boundary.
        parser.process(b"\x1b]52;c;S0xN");
        parser.process(b"Tk9Q\x07");
        // 4. Split form B: a different cut, between the `ty` and the payload.
        parser.process(b"\x1b]52;p;");
        parser.process(b"UlNU\x07");
        // 5. Non-52 OSC: a window-title set (OSC 0). Must NOT dispatch
        //    copy_to_clipboard — vt100 routes it to set_window_title.
        parser.process(b"\x1b]0;my title\x07");

        // The four clean OSC-52s dispatched once each, in order, verbatim; the
        // OSC-0 did not reach copy_to_clipboard.
        assert_eq!(
            parser.callbacks().recorded,
            vec![
                (b"c".to_vec(), b"QUJD".to_vec()),     // BEL
                (b"c".to_vec(), b"REVG".to_vec()),     // ST
                (b"c".to_vec(), b"S0xNTk9Q".to_vec()), // split A, reassembled
                (b"p".to_vec(), b"UlNU".to_vec()),     // split B, reassembled
            ],
            "BEL + ST + two split-reads dispatch once each with verbatim ty/data; \
             a non-52 OSC does not reach copy_to_clipboard"
        );

        // Finding 1: bare C1 ST 0x9C does not terminate the OSC in vte 0.15.0.
        // It is absorbed into the payload, so nothing dispatches until a real
        // terminator arrives.
        let mut c1 =
            vt100::Parser::new_with_callbacks(24, 80, 0, RecordingCallbacks::new());
        c1.process(b"\x1b]52;c;R0hJ\x9c");
        assert!(
            c1.callbacks().recorded.is_empty(),
            "FINDING: bare C1 ST 0x9C does not terminate the OSC in vte 0.15.0 — \
             nothing dispatched yet (spec gap to file upstream)"
        );

        // Finding 2: CAN 0x18 dispatches the partial OSC, it does not abort it.
        let mut can =
            vt100::Parser::new_with_callbacks(24, 80, 0, RecordingCallbacks::new());
        can.process(b"\x1b]52;c;aGV\x18");
        assert_eq!(
            can.callbacks().recorded,
            vec![(b"c".to_vec(), b"aGV".to_vec())],
            "FINDING: CAN 0x18 dispatches the partial OSC-52 in vte 0.15.0 rather \
             than aborting it (spec gap to file upstream)"
        );
    }

    /// The production [`GutterCallbacks`] writes the reconstructed OSC 52 to its
    /// injected sink (here a captured `Vec` standing in for `/dev/tty`). Proves the
    /// production `copy_to_clipboard` body forwards verbatim through the real
    /// callback path.
    #[test]
    fn production_callbacks_forward_osc52_to_injected_sink() {
        use std::io::Write;
        use std::sync::{Arc, Mutex};

        /// A shareable capture sink: the test reads back what the callback wrote
        /// while the parser owns the writer.
        #[derive(Clone)]
        struct Shared(Arc<Mutex<Vec<u8>>>);
        impl Write for Shared {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let captured = Arc::new(Mutex::new(Vec::new()));
        let sink = Shared(captured.clone());
        let mut parser = vt100::Parser::new_with_callbacks(
            24,
            80,
            0,
            GutterCallbacks::live(Box::new(sink)),
        );

        parser.process(b"\x1b]52;c;aGVsbG8=\x07");

        assert_eq!(
            *captured.lock().unwrap(),
            b"\x1b]52;c;aGVsbG8=\x07",
            "production copy_to_clipboard reconstructs ESC ] 52 ; c ; data BEL verbatim"
        );
    }
}
