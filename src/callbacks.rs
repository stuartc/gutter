//! The `vt100::Callbacks` impl carried by the render thread's parser.
//!
//! This is the **shared callbacks struct** (ADR-003/004): the single place
//! gutter hooks the parser. It is wired in by slice 02 as inert plumbing; slice
//! 04 added the kitty keyboard-level watcher, and slice 06 (this slice) adds the
//! OSC-52 `copy_to_clipboard` override plus an injected `/dev/tty` writer to
//! **this same struct** — a sibling field/method, not a reshape. The kitty
//! watcher and the clipboard concern share no state and must not reach into each
//! other: `unhandled_csi` only touches `kitty_state`, `copy_to_clipboard` only
//! touches `clipboard_out`.
//!
//! Construct the parser with
//! `vt100::Parser::new_with_callbacks(rows, cols, scrollback, GutterCallbacks::new(..))`
//! — there is **no** `process_cb` method (that reference in the rough plan is
//! wrong). The callbacks fire on `parser.process(bytes)` on the render thread,
//! the only thread that touches the parser, so there is no shared mutable state
//! and `kitty_state.current()` is read lock-free at encode time (ADR-009). The
//! OSC-52 write therefore also happens inline on the render thread (Thread 2),
//! on a fd that is not stdout.

use std::fmt;
use std::io::{self, Write};

use crate::clipboard::forward_osc52;
use crate::cursor::{is_decscusr, CursorShape};
use crate::keyboard::{is_kitty_csi, KittyLevel, KittyState};

/// The fixed Primary Device Attributes (DA1) identity gutter answers `CSI c` with
/// (VT100 with Advanced Video Option). The exact identity does not matter — a
/// child gating on a DA1 handshake only needs *an* answer to proceed — so a
/// stable, conventional reply is used.
const DA1_REPLY: &[u8] = b"\x1b[?1;2c";

/// The single callbacks struct the parser owns.
///
/// Carries two independent concerns: the kitty [`KittyState`] (slice 04 — the
/// child's negotiated keyboard level, clamped to the outer terminal's
/// capability) and the clipboard sink (slice 06 — where a reconstructed OSC 52
/// is written). They sit side by side; neither method reads the other's field.
pub struct GutterCallbacks {
    /// The child's negotiated kitty keyboard level — driven by the
    /// `unhandled_csi` watcher below, read by the encoder at keystroke time.
    pub kitty_state: KittyState,
    /// The child's requested cursor shape (DECSCUSR / `CSI Ps SP q`) — driven by
    /// the same `unhandled_csi` watcher (vt100 surfaces DECSCUSR as an unhandled
    /// CSI, slice 08), read by the render loop to mirror the shape on the outer
    /// terminal. Touches only its own field; independent of the kitty watcher.
    pub cursor_shape: CursorShape,
    /// The clipboard write sink (ADR-004). Production injects the real
    /// `/dev/tty` handle ([`crate::clipboard::open_tty_read_write`]); tests
    /// inject a buffer they read back. Baseline (diff-only) parsers get an
    /// `io::sink()` — they never run the live OSC-52 path. Behind a `Box<dyn
    /// Write>` rather than a concrete `File` so the callback is testable without
    /// a real tty.
    clipboard_out: Box<dyn Write + Send>,
    /// Replies buffered for the child's device queries (slice 11). gutter is the
    /// child's emulator, so it answers `CSI c` / `CSI 5 n` / `CSI 6 n` / `CSI ? u`
    /// itself rather than proxying them. `unhandled_csi` only *buffers* here — the
    /// render loop drains this to the PTY master (the one writer it owns) right
    /// after `parser.process()`, so no reply leaves callbacks.
    replies: Vec<u8>,
}

impl GutterCallbacks {
    /// Build the callbacks with the outer terminal's kitty capability (the
    /// startup `supports_keyboard_enhancement()` probe) and a discarding
    /// clipboard sink. When `outer_supports` is `false`, the child's kitty enable
    /// is clamped to a no-op (case-B degradation).
    ///
    /// The default sink is `io::sink()` — used by the diff-baseline parsers,
    /// which only replay formatted content and never carry the live clipboard
    /// fd. The live parser injects a real sink via [`Self::with_clipboard`].
    pub fn new(outer_supports: bool) -> Self {
        Self::with_clipboard(outer_supports, Box::new(io::sink()))
    }

    /// Build the callbacks with a specific clipboard sink injected. Production
    /// passes the real `/dev/tty` handle; the OSC-52 dispatch and end-to-end
    /// tests pass a recording buffer.
    pub fn with_clipboard(outer_supports: bool, clipboard_out: Box<dyn Write + Send>) -> Self {
        Self {
            kitty_state: KittyState::new(outer_supports),
            cursor_shape: CursorShape::new(),
            clipboard_out,
            replies: Vec::new(),
        }
    }

    /// Take the device-query replies buffered since the last drain (slice 11).
    /// Called by the render loop in the `Msg::Pty` arm after `parser.process()`,
    /// which writes them to the PTY master with the same `write_all` + `flush` the
    /// key/mouse paths use. Empty when the child issued no query this frame.
    pub fn drain_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.replies)
    }

    /// Buffer the kitty keyboard-protocol report for `CSI ? u`: `CSI ? <flags> u`
    /// carrying the child's **live** progressive-enhancement level (the top of the
    /// push/pop stack, `0` when legacy). Reading the live level here is why a
    /// query issued after a push reflects the state the child actually set.
    fn buffer_kitty_report(&mut self) {
        let flags = match self.kitty_state.current() {
            KittyLevel::Legacy => 0,
            KittyLevel::Kitty(f) => f,
        };
        self.replies
            .extend_from_slice(format!("\x1b[?{flags}u").as_bytes());
    }
}

/// The reply for a non-private DSR / DA1 device query, or `None` when the final
/// is not one gutter answers. Only the bare (`i1 == None`) forms are answered:
/// the secondary/tertiary DA (`CSI > c` / `CSI = c`) and private DSR carry an
/// intermediate and are out of scope. The cursor-position reply reads
/// `screen.cursor_position()` — the child's **W-grid** coordinates — and reports
/// them 1-based per the DSR spec; the band's left-margin offset lives in the
/// render thread and never reaches here, so it cannot leak into the reply.
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
            .field("kitty_state", &self.kitty_state)
            .finish_non_exhaustive()
    }
}

impl vt100::Callbacks for GutterCallbacks {
    /// The kitty keyboard watcher (ADR-003). The child enables/disables kitty on
    /// its **output** via `CSI > N u` / `CSI < u`, which surface here as
    /// unhandled CSI sequences. Recognise the family, then hand the sequence to
    /// the push/pop stack — which applies the outer-capability clamp.
    ///
    /// Touches **only** `kitty_state`. The clipboard lives in
    /// `copy_to_clipboard`, a sibling method, with no entanglement here.
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        _i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        if is_kitty_csi(i1, c) {
            // The query form `CSI ? u` asks for the live level — answer it; the
            // enable/disable forms (`CSI > N u` / `CSI < u`) mutate the stack.
            if i1 == Some(b'?') {
                self.buffer_kitty_report();
            } else {
                self.kitty_state.apply_csi(i1, params, c);
            }
        } else if is_decscusr(i1, c) {
            // DECSCUSR cursor-shape request (slice 08). vt100 doesn't implement
            // it, so it lands here with the SP intermediate in `i1`; record the
            // requested shape for the render loop to mirror. Touches only
            // `cursor_shape` — sibling to the kitty and clipboard concerns.
            self.cursor_shape.apply_csi(params);
        } else if let Some(reply) = device_query_reply(i1, params, c, screen) {
            // A DA1 / DSR device query (slice 11). gutter is the child's emulator,
            // so it buffers a spec-correct reply here; the render loop drains it to
            // the PTY master. Touches only `replies`.
            self.replies.extend_from_slice(&reply);
        }
    }

    /// The OSC-52 clipboard write (ADR-004). vt100/vte hands us a complete,
    /// already-reassembled OSC 52 with `data` still base64-encoded; we reconstruct
    /// `ESC ] 52 ; ty ; data BEL` verbatim and write it to the injected sink (the
    /// separately-opened `/dev/tty` in production), on this thread, inside
    /// `parser.process()`.
    ///
    /// Touches **only** `clipboard_out`. `screen` is ignored — a clipboard event
    /// never touches the grid. The error is swallowed (logged), never propagated:
    /// a failed clipboard write must not unwind out of `process()` or take down
    /// the render loop (best-effort side effect).
    fn copy_to_clipboard(&mut self, _: &mut vt100::Screen, ty: &[u8], data: &[u8]) {
        if let Err(e) = forward_osc52(&mut self.clipboard_out, ty, data) {
            eprintln!("gutter: clipboard write failed: {e}");
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

    /// The cursor-shape watcher (slice 08) tracks the child's DECSCUSR through
    /// the real `vt100` callback path: drive `parser.process()` with a
    /// `CSI Ps SP q` and assert the shared callbacks recorded a pending shape to
    /// mirror — proving DECSCUSR is observable via `unhandled_csi` (in scope,
    /// not a documented gap) and tracked on the shared struct.
    #[test]
    fn decscusr_tracked_through_parser() {
        let mut parser =
            vt100::Parser::new_with_callbacks(24, 80, 0, GutterCallbacks::new(true));

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
            vt100::Parser::new_with_callbacks(24, 80, 0, GutterCallbacks::new(true));

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
            vt100::Parser::new_with_callbacks(24, 80, 0, GutterCallbacks::new(true));
        parser.process(b"\x1b[5n");
        assert_eq!(parser.callbacks_mut().drain_replies(), b"\x1b[0n");
    }

    /// **The reply most worth pinning (PRD §5).** DSR cursor-position (`CSI 6 n`)
    /// is answered `CSI row ; col R` in the child's **W-grid** coordinates, 1-based.
    /// The callbacks only ever see the parser's own grid — the band's left-margin
    /// offset lives in the render thread and never reaches here — so the reply is
    /// the child's true position, never shifted by the gutter. Position the cursor
    /// with `CSI 3 ; 7 H` and assert the reply echoes `3 ; 7`, not a margin-shifted
    /// column.
    #[test]
    fn cursor_position_reply_uses_w_grid_coords() {
        let mut parser =
            vt100::Parser::new_with_callbacks(24, 80, 0, GutterCallbacks::new(true));

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

    /// The kitty query (`CSI ? u`) is answered with the child's **live** level —
    /// the top of the push/pop stack — so a query after a push reflects the state
    /// the child actually set, and the query itself never mutates the stack.
    #[test]
    fn kitty_query_reports_live_level() {
        let mut parser =
            vt100::Parser::new_with_callbacks(24, 80, 0, GutterCallbacks::new(true));

        // Legacy floor: no level pushed → flags 0.
        parser.process(b"\x1b[?u");
        assert_eq!(parser.callbacks_mut().drain_replies(), b"\x1b[?0u");

        // Push a level, then query: the reply reflects the live top of stack.
        parser.process(b"\x1b[>5u\x1b[?u");
        assert_eq!(parser.callbacks_mut().drain_replies(), b"\x1b[?5u");
        // The query did not mutate the stack — the live level is unchanged.
        assert_eq!(parser.callbacks().kitty_state.current(), KittyLevel::Kitty(5));
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

    /// The **recording** callbacks shape (PRD Testing Decisions §1–3): the same
    /// two concerns as [`GutterCallbacks`] — the kitty watcher and the OSC-52
    /// hook — but `copy_to_clipboard` records each `(ty, data)` into a `Vec`
    /// instead of writing a tty. This lets the dispatch-confirmation and
    /// coexistence tests drive `parser.process()` directly and assert what fired,
    /// with no PTY, no `/dev/tty`, no threads. The kitty watcher body is
    /// **identical** to production's (`is_kitty_csi` → `apply_csi`); only the
    /// clipboard body differs (record vs forward) — exactly the seam the PRD
    /// describes.
    struct RecordingCallbacks {
        kitty_state: KittyState,
        recorded: Vec<(Vec<u8>, Vec<u8>)>,
    }

    impl RecordingCallbacks {
        fn new(outer_supports: bool) -> Self {
            Self {
                kitty_state: KittyState::new(outer_supports),
                recorded: Vec::new(),
            }
        }
    }

    impl vt100::Callbacks for RecordingCallbacks {
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

        fn copy_to_clipboard(&mut self, _: &mut vt100::Screen, ty: &[u8], data: &[u8]) {
            self.recorded.push((ty.to_vec(), data.to_vec()));
        }
    }

    /// **OSC-52 dispatch confirmation** (the core seam test, ADR-004). gutter
    /// does not parse OSC itself — it relies on vt100/vte's dispatch — so this
    /// test drives `parser.process()` directly with a recording callback and pins
    /// **exactly** what the pinned `vt100 0.16.2` / `vte 0.15.0` dispatches for
    /// each terminator and abort form. It is the unit-level proof of "the only
    /// unconfirmed link is vt100's own dispatch layer" — and, as ADR-004 foresaw,
    /// it is where a divergence between that layer and the VT spec surfaces ("if
    /// it trips it's a vt100 wrapper bug to report, not a reason to revert to a
    /// hand-rolled scanner").
    ///
    /// Running this against the pinned vte 0.15.0 turned up **two verified
    /// divergences from the VT spec**, recorded here as findings (NOT gutter
    /// bugs — gutter forwards verbatim whatever vt100 hands it):
    ///
    /// 1. **C1 ST `0x9C` is NOT an OSC terminator.** In `vte::advance_osc_string`
    ///    the raw `0x9C` byte falls into the catch-all `action_osc_put` arm — it
    ///    is appended to the OSC payload, not treated as ST. Only the two-byte
    ///    `ESC \` ST form terminates. (An 8-bit-clean child that emits a bare
    ///    `0x9C` ST would have its OSC swallowed until the next real terminator.)
    /// 2. **CAN `0x18` / SUB `0x1A` DISPATCH the OSC, they do not abort it.** In
    ///    `vte`, `0x18`/`0x1A` inside an OSC string call `osc_end` →
    ///    `osc_dispatch` (then `execute(byte)`), so a partial OSC-52 cut short by
    ///    CAN is delivered with the bytes seen so far rather than discarded.
    ///
    /// The spec'd behaviour is the *desired* one; these are upstream gaps to file
    /// against vte/vt100. This test asserts the **verified actual** behaviour so
    /// CI is green and stable on the pinned toolchain AND so a future vte upgrade
    /// that fixes either divergence trips the test (forcing a conscious update).
    /// The forms gutter's transparency promise actually leans on every day — BEL,
    /// `ESC \`, and split-read reassembly across `process()` calls — all work
    /// exactly as required.
    #[test]
    fn osc52_dispatch_confirmation_against_pinned_vte() {
        let mut parser =
            vt100::Parser::new_with_callbacks(24, 80, 0, RecordingCallbacks::new(true));

        // 1. BEL (0x07) terminator — works (spec-conformant).
        parser.process(b"\x1b]52;c;QUJD\x07");
        // 2. ST (ESC \) terminator — works (spec-conformant).
        parser.process(b"\x1b]52;c;REVG\x1b\\");
        // 3. Split form A: cut mid-payload across two process() calls — vte's
        //    persistent osc_raw buffer reassembles across the boundary (works).
        parser.process(b"\x1b]52;c;S0xN");
        parser.process(b"Tk9Q\x07");
        // 4. Split form B: a DIFFERENT cut — between the `ty` and the payload
        //    (the two mandated distinct adversarial splits; works).
        parser.process(b"\x1b]52;p;");
        parser.process(b"UlNU\x07");
        // 5. Non-52 OSC: a window-title set (OSC 0). Must NOT dispatch
        //    copy_to_clipboard — vt100 routes it to set_window_title (works).
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

        // --- Finding 1: C1 ST 0x9C is NOT a terminator in vte 0.15.0. ---
        // The OSC stays open and the 0x9C is absorbed into the payload; only a
        // later real terminator closes it. We prove that by feeding the 0x9C form
        // then a BEL-terminated continuation and observing they merged into ONE
        // dispatch (the OSC never closed at the 0x9C).
        let mut c1 =
            vt100::Parser::new_with_callbacks(24, 80, 0, RecordingCallbacks::new(true));
        c1.process(b"\x1b]52;c;R0hJ\x9c");
        assert!(
            c1.callbacks().recorded.is_empty(),
            "FINDING: bare C1 ST 0x9C does not terminate the OSC in vte 0.15.0 — \
             nothing dispatched yet (spec gap to file upstream)"
        );

        // --- Finding 2: CAN 0x18 DISPATCHES the OSC, it does not abort it. ---
        let mut can =
            vt100::Parser::new_with_callbacks(24, 80, 0, RecordingCallbacks::new(true));
        can.process(b"\x1b]52;c;aGV\x18");
        assert_eq!(
            can.callbacks().recorded,
            vec![(b"c".to_vec(), b"aGV".to_vec())],
            "FINDING: CAN 0x18 dispatches the partial OSC-52 in vte 0.15.0 rather \
             than aborting it (spec gap to file upstream)"
        );
    }

    /// **Shared-struct coexistence** (PRD §3 / the slice AC). On the **one**
    /// recording callbacks instance, drive a stream carrying BOTH a kitty enable
    /// (`CSI > 1 u`, watched by `unhandled_csi`) AND an OSC-52 write. Assert the
    /// tracked kitty level updated AND `copy_to_clipboard` fired — proving the
    /// struct carries both concerns and neither method touched the other's state.
    #[test]
    fn kitty_watcher_and_clipboard_coexist_without_coupling() {
        let mut parser =
            vt100::Parser::new_with_callbacks(24, 80, 0, RecordingCallbacks::new(true));

        // Child enables kitty, then copies to the clipboard.
        parser.process(b"\x1b[>1u");
        parser.process(b"\x1b]52;c;aGVsbG8=\x07");

        let cb = parser.callbacks();
        assert_eq!(
            cb.kitty_state.current(),
            KittyLevel::Kitty(1),
            "the kitty watcher tracked the enable"
        );
        assert_eq!(
            cb.recorded,
            vec![(b"c".to_vec(), b"aGVsbG8=".to_vec())],
            "the clipboard hook fired on the SAME struct, independently"
        );
    }

    /// The production [`GutterCallbacks`] writes the reconstructed OSC 52 to its
    /// injected sink (here a captured `Vec` standing in for `/dev/tty`), and the
    /// kitty watcher on the same struct stays functional. Proves the production
    /// `copy_to_clipboard` body forwards verbatim through the real callback path.
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
            GutterCallbacks::with_clipboard(true, Box::new(sink)),
        );

        // Kitty enable then a clipboard write, on the production struct.
        parser.process(b"\x1b[>1u");
        parser.process(b"\x1b]52;c;aGVsbG8=\x07");

        assert_eq!(
            parser.callbacks().kitty_state.current(),
            KittyLevel::Kitty(1),
            "the kitty watcher still works on the production struct"
        );
        assert_eq!(
            *captured.lock().unwrap(),
            b"\x1b]52;c;aGVsbG8=\x07",
            "production copy_to_clipboard reconstructs ESC ] 52 ; c ; data BEL verbatim"
        );
    }
}
