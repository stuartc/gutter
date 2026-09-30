//! Thread 2 — the render loop, the vt100 grid, and the offset repaint.
//!
//! Owns the `vt100::Parser` exclusively (ADR-009) and the outer-terminal handle,
//! and is the only thread that writes the PTY master. Runs the fixed-deadline
//! 60fps coalescing loop (ADR-007), generic over an injectable [`Clock`] so the
//! timing tests run on virtual time.
//!
//! Each message is dispatched by [`dispatch`]: PTY bytes feed the parser, raw
//! input bytes go through the scanner and on to the child untouched (ADR-020),
//! extracted mouse reports route through the forwarding gate (ADR-005), and
//! resize runs the ordered handler (ADR-008/011).

use std::io::Write;
use std::time::Duration;

use crate::callbacks::GutterCallbacks;
use crate::chord::{self, Chord, KeyEvent};
use crate::clock::{Clock, Recv};
use crate::geometry::{self, Layout, Width};
use crate::modes::ModeMirror;
use crate::mouse::{MouseDecision, MouseGate};
use crate::msg::Msg;
use crate::pty::PtyResizer;
use crate::relay::KeyModeRelay;
use crate::rowclip::{clip_row_to_width_into, Placement};
use crate::scan::{Scanner, Token, ESC_HOLD};
use crate::suspend::Suspender;
use crate::terminal::OuterTerminal;

/// The 60fps frame budget. One render per `FRAME` of wall (or virtual) time.
pub const FRAME: Duration = Duration::from_millis(16);

/// Resize-mode idle auto-exit window (~3 s). Uses the injected clock (ADR-007),
/// so it is unit-testable on virtual time. PRD 0001, Feature 2 ("Exiting").
pub const RESIZE_IDLE: Duration = Duration::from_secs(3);

/// Upper bound on how long the shutdown path waits for the PTY forwarder's
/// `Msg::PtyEof` after the child exits (ADR-013). Normally the sentinel arrives at
/// once and the drain returns immediately; the cap is only hit when a grandchild
/// keeps the PTY slave open and the master never EOFs, so teardown would otherwise
/// hang waiting for a sentinel that never comes.
pub const TEARDOWN_DRAIN_GRACE: Duration = Duration::from_millis(100);

/// The scroll-tracker's bounded scrollback (ADR-013). The tracker keeps its screen for
/// the whole session and counts departures as the growth of that scrollback, so the cap
/// bounds a session's worth of departed lines rather than a single frame's. Running out
/// of headroom for the next frame costs one re-seed, which is the only time the tracker
/// forgets the child's scroll region — hence a cap far above what a frame, or a run of
/// them, will ever depart.
const SCROLL_TRACKER_SCROLLBACK: usize = 4096;

/// What the two restore paths hand the shell back: no leftover attribute run from the
/// band's last painted cell, and a default cursor shape rather than whatever DECSCUSR
/// the child last asked for (ADR-010).
///
/// `Ps = 0` is what terminals that treat DECSCUSR as resettable — kitty, VTE, Ghostty,
/// iTerm2 — read as "back to the configured shape". On xterm's own table it is a
/// blinking block, the same as `Ps = 1`; there is no portable "whatever it was before",
/// so a user whose xterm cursor is a bar and whose child changed it gets the block back.
/// Only emitted when gutter actually wrote a shape, so a run that changed nothing leaves
/// the user's cursor alone.
pub(crate) const SGR_RESET: &[u8] = b"\x1b[0m";
pub(crate) const DEFAULT_CURSOR_SHAPE: &[u8] = b"\x1b[0 q";

/// The render thread's state: the parsers, the diff baseline, and the band geometry.
pub struct Renderer {
    parser: vt100::Parser<GutterCallbacks>,
    /// The previous-frame screen the `rows_diff` is computed against.
    prev: vt100::Parser<GutterCallbacks>,
    /// Scroll-off tracker (ADR-013): a second grid at the band's size, fed the same
    /// PTY bytes as `parser` but with bounded scrollback, so vt100's scroll machinery
    /// records which lines left the top of the W-window each frame — the count the
    /// live `parser` (scrollback 0) can't reconstruct once a burst scrolls past a
    /// screenful in one frame. Never re-seeded while it has room: it is the child's
    /// own screen state, scroll region and alt-screen flag included, and a frame's
    /// departures are the growth of its scrollback ([`Renderer::drain_scrolled_off`]).
    scroll_tracker: vt100::Parser<GutterCallbacks>,
    /// The tracker's scrollback cap. Carried here rather than read from
    /// [`SCROLL_TRACKER_SCROLLBACK`] at each use so a test can drive the saturation
    /// path without departing thousands of lines.
    tracker_cap: usize,
    /// How long the tracker's scrollback was when the last frame finished counting.
    /// This frame's departures are the lines beyond it.
    tracker_scrollback: usize,
    /// The current band width `W`. Constant for an absolute `--width`; recomputed
    /// on each resize for a proportional `--width Npct` (ADR-011).
    width: u16,
    /// The requested `--width`, so resize can recompute `W` via
    /// [`geometry::resolve_width`] (a no-op for an absolute width).
    width_config: Width,
    /// The band alignment (`--center` / `--left`), feeding [`geometry::margin`]
    /// at startup and on every resize.
    layout: Layout,
    /// The current real terminal width — the input to both the margin and the
    /// proportional-width recompute. Updated on each resize.
    real_cols: u16,
    /// The band's left margin (physical column the band starts at), recomputed
    /// from `layout`, `real_cols` and `width` on each resize.
    left_margin: u16,
    /// The cursor visibility last mirrored to the outer terminal, so we only
    /// emit a show/hide when it actually changes.
    cursor_visible: bool,
    /// Whether the OUTER terminal is in the alternate screen, mirroring the child's
    /// `alternate_screen()` (ADR-012). Edge-triggered like `cursor_visible`: we emit
    /// an enter/leave only on a real change. Never forced — gutter enters the alt
    /// screen only when the child does.
    outer_alt_active: bool,
    /// The input modes last mirrored to the outer terminal (ADR-022) — DECCKM,
    /// application keypad and bracketed paste. Edge-triggered like `cursor_visible`
    /// and `outer_alt_active`, and polled from the same place in the frame.
    mode_mirror: ModeMirror,
    /// Physical terminal row where grid row 0 sits on the PRIMARY screen (ADR-013).
    /// Initialised to the launch cursor row and driven monotonically toward 0 by the
    /// per-frame make-room scroll as the band grows; once it reaches 0 the band fills
    /// the screen and the scroll-emit engine takes over. Frozen while the child is in
    /// the alt screen, where the band always paints at offset 0.
    base_row: u16,
    /// Whether gutter has ever painted inline (primary-screen) content this run.
    /// Gates the teardown hand-back (ADR-013): a TUI that went straight to the alt
    /// screen and back, never showing inline content, leaves no stray status line.
    ever_painted_inline: bool,
    /// Whether the PTY forwarder's `Msg::PtyEof` sentinel has been seen. The shutdown
    /// drain reads it to skip the bounded wait (ADR-013). A drain hint only — it never
    /// triggers shutdown; the waiter stays authoritative.
    pty_eof_seen: bool,
    /// The mouse forwarding gate (ADR-005), holding the button-held flag the motion
    /// down-filter needs. The child's `(mode, encoding)` is read live from the screen
    /// each mouse report, not cached here.
    mouse_gate: MouseGate,
    /// The reserved resize-mode enter chord (`--resize-key`, default Ctrl-\). The one
    /// key gutter ever withholds from the child, and only as the enter chord or while
    /// in the mode (PRD 0001, Feature 2).
    resize_key: Chord,
    /// Whether resize mode is currently active. Mirrors the loop-owned `ResizeCtl`
    /// (which `render_once` can't see) so the per-frame cursor tail knows to suppress
    /// the mirrored child cursor while the resize overlay owns the band.
    resize_active: bool,
    /// The codepoint of a key whose press gutter consumed, when that press arrived as a
    /// keyboard-protocol report (ADR-021). Such a mode reports a key as a press, any
    /// repeats and a release, so consuming only the press hands the child a key-up with
    /// no key-down, which is the very state it turned event reporting on to track. One
    /// slot: a key's reports arrive together, and any other key clears it.
    consumed_press: Option<u32>,
}

impl Renderer {
    /// Build a renderer for a `width × rows` virtual grid in a `real_cols`-wide
    /// outer terminal.
    ///
    /// `width` is the resolved initial `W`; `width_config` is kept so resize can
    /// recompute it for the proportional path (ADR-011). `clipboard_out` is the
    /// OSC-52 sink (ADR-004), riding only the live `parser` — `prev` is a diff-only
    /// baseline that never runs the clipboard path. `base_row` is the launch cursor
    /// row (ADR-013).
    pub fn new(
        width: u16,
        rows: u16,
        real_cols: u16,
        layout: Layout,
        width_config: Width,
        clipboard_out: Box<dyn Write + Send>,
        base_row: u16,
    ) -> Self {
        Self {
            parser: vt100::Parser::new_with_callbacks(
                rows,
                width,
                0,
                GutterCallbacks::live(clipboard_out),
            ),
            prev: vt100::Parser::new_with_callbacks(rows, width, 0, GutterCallbacks::baseline()),
            // Mirrors the band's geometry with a bounded scrollback so vt100 records
            // the lines that scroll off the top (ADR-013). Diff-only like `prev`.
            scroll_tracker: vt100::Parser::new_with_callbacks(
                rows,
                width,
                SCROLL_TRACKER_SCROLLBACK,
                GutterCallbacks::baseline(),
            ),
            tracker_cap: SCROLL_TRACKER_SCROLLBACK,
            tracker_scrollback: 0,
            width,
            width_config,
            layout,
            real_cols,
            left_margin: geometry::margin(layout, real_cols, width),
            // vt100 starts with the cursor visible; mirror that initial state.
            cursor_visible: true,
            // gutter never forces the alt screen (ADR-012); start false.
            outer_alt_active: false,
            // Nothing mirrored yet, so nothing owed back at teardown (ADR-022).
            mode_mirror: ModeMirror::new(),
            // The inline anchor (ADR-013), clamped into the grid.
            base_row: base_row.min(rows.saturating_sub(1)),
            ever_painted_inline: false,
            pty_eof_seen: false,
            mouse_gate: MouseGate::default(),
            resize_key: Chord::default(),
            resize_active: false,
            consumed_press: None,
        }
    }

    /// Whether `unit` is a follow-up report gutter owes a press it consumed, and so is
    /// swallowed. The release settles the debt. A repeat out of resize mode is
    /// swallowed and leaves it owed; in the mode a repeat is not owed here, so it
    /// reaches the classifier and steps like an auto-repeated byte, and the slot
    /// stays for the release. Any other unit clears the slot.
    fn owed_key_report(&mut self, unit: &[u8], in_mode: bool) -> bool {
        let Some(code) = self.consumed_press else {
            return false;
        };
        match chord::csi_key(unit) {
            Some(key) if key.code == code && key.event == KeyEvent::Repeat => !in_mode,
            Some(key) if key.code == code && key.event == KeyEvent::Release => {
                self.consumed_press = None;
                true
            }
            _ => {
                self.consumed_press = None;
                false
            }
        }
    }

    /// Remember that gutter consumed this unit's press, so [`owed_key_report`] can
    /// consume its repeats and release too.
    ///
    /// [`owed_key_report`]: Renderer::owed_key_report
    fn note_consumed_press(&mut self, unit: &[u8]) {
        if let Some(key) = chord::csi_key(unit).filter(|k| k.event == KeyEvent::Press) {
            self.consumed_press = Some(key.code);
        }
    }

    /// Override the resize-mode enter chord (from `--resize-key`). Called once at
    /// startup from `main`; tests keep the default.
    pub fn set_resize_key(&mut self, chord: Chord) {
        self.resize_key = chord;
    }

    /// Read-only view of the virtual screen — for the insta snapshot and the
    /// equivalence gate.
    #[allow(dead_code)]
    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// Row where the band's span starts (ADR-0017): 0 on the alt screen, `base_row`
    /// on the primary screen so shell history above the inline band stays untouched.
    ///
    /// The settled value, which a paint can lower: `scroll_to_make_room` scrolls the
    /// real terminal and drops `base_row` during the very frame being measured, so a
    /// caller reading the offset it asked for would be looking at the wrong rows.
    pub(crate) fn span_offset(&self) -> u16 {
        if self.outer_alt_active {
            0
        } else {
            self.base_row
        }
    }

    /// Test-only constructor at an explicit `left_margin`, so the cell-walk / CJK
    /// edge-of-band tests can pin a margin directly and inspect physical columns
    /// without routing through a [`Layout`]/`real_cols` pair.
    #[cfg(test)]
    fn at_margin(width: u16, rows: u16, left_margin: u16) -> Self {
        let mut r = Self::new(
            width,
            rows,
            width,
            Layout::Left,
            Width::Cols(width),
            Box::new(std::io::sink()),
            0,
        );
        r.left_margin = left_margin;
        r.real_cols = left_margin.saturating_add(width);
        r
    }

    /// Test-only: rebuild the scroll tracker at a small scrollback cap, so the
    /// saturation path can be reached in a handful of scrolled lines instead of
    /// [`SCROLL_TRACKER_SCROLLBACK`] of them.
    #[cfg(test)]
    fn set_tracker_cap(&mut self, cap: usize) {
        self.tracker_cap = cap;
        self.reseed_scroll_tracker();
    }
}

/// What one dispatched message tells the render loop to do next. `Exit(code)`
/// tears down, `Suspend` runs the suspend/resume cycle, `Continue` is the
/// ordinary path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Continue,
    Exit(i32),
    Suspend,
}

/// Apply one message to the render state, returning the [`Flow`] the loop should
/// take. `ChildExited` → `Exit`; `ChildStopped` → `Suspend`; `ChildContinued`
/// forces a full repaint and continues; everything else continues.
///
/// The PTY writer is injected as a `Write` sink so the input-liveness test can
/// assert "key bytes reached the PTY-master mock within one frame" against a
/// recording `Vec<u8>` with no real child. The `PtyResizer` and outer terminal
/// are injected for the same reason — the resize handler is driven against a
/// recording mock that captures the `master.resize` / `set_size` call order.
fn dispatch<P, R, T>(
    msg: Msg,
    renderer: &mut Renderer,
    pty_writer: &mut P,
    resizer: &R,
    term: &mut T,
) -> Flow
where
    P: Write,
    R: PtyResizer,
    T: OuterTerminal,
{
    match msg {
        Msg::Pty(bytes) => {
            renderer.parser.process(&bytes);
            // Carry the child's keyboard-mode requests out to the real terminal
            // (ADR-021), before the inward replies: a relayed `CSI ? u` has a round
            // trip ahead of it and the head start is free. Safe outside a frame —
            // `render_once` queues and flushes within one call, so no half-built
            // frame is ever outstanding when a message is dispatched.
            let relayed = renderer.parser.callbacks_mut().drain_relay();
            let _ = relay_if_any(term, &relayed);
            // Answer the child's device queries: parser.process surfaced any
            // CSI c / CSI 5 n / CSI 6 n through unhandled_csi, which buffered a
            // spec-correct reply; drain it to the PTY master.
            let replies = renderer.parser.callbacks_mut().drain_replies();
            if !replies.is_empty() {
                let _ = pty_writer.write_all(&replies);
                let _ = pty_writer.flush();
            }
            // Feed the same bytes to the scroll tracker (ADR-013) so vt100's scroll
            // machinery captures the lines that leave the top of the W-window this
            // frame. Robust to a burst that turns the screen over in one frame, where
            // a grid-vs-grid diff sees no surviving overlap and would report zero.
            renderer.scroll_tracker.process(&bytes);
            Flow::Continue
        }
        Msg::Resize => {
            // The resize handler (ADR-008/011) runs on THIS thread, the only parser
            // owner. The size is read here rather than carried on the message, so
            // two coalesced SIGWINCHes can't leave us acting on a stale geometry.
            // Param-order trap: (cols, rows) here, set_size(rows, cols) inside.
            if let Ok((cols, rows)) = term.terminal_size() {
                handle_resize(renderer, resizer, term, cols, rows);
            }
            Flow::Continue
        }
        // Input reaches the child through `apply_message`'s scanner, which is the
        // only caller that owns the scanner state. This arm is reached from the two
        // bounded drains — the shutdown drain (ADR-013) and the suspend cycle's
        // pre-stop drain (ADR-0019) — where a keystroke is dropped rather than
        // forwarded unscanned.
        Msg::Input(_) => Flow::Continue,
        Msg::ChildExited(status) => Flow::Exit(status.exit_code() as i32),
        // The child stopped (ADR-0018): drive the suspend/resume cycle. `sig` is
        // carried for tests/logging; v1 reacts to every stop signal identically.
        Msg::ChildStopped { .. } => Flow::Suspend,
        // The child was continued out from under gutter (or by our own SIGCONT):
        // force a full repaint. A hint only — never triggers suspend.
        Msg::ChildContinued => {
            renderer.reset_prev_baseline();
            Flow::Continue
        }
        // PTY path reached EOF — record it so the shutdown drain knows the child's
        // final bytes have all landed (ADR-013). Never triggers shutdown.
        Msg::PtyEof => {
            renderer.pty_eof_seen = true;
            Flow::Continue
        }
    }
}

/// What the render loop should do with one scanned unit while resize mode is /
/// isn't active. `PassThrough` is the only variant that reaches the child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyAction {
    Enter,        // the chord, not in mode → enter resize mode
    Exit,         // Esc or the chord, in mode → leave
    Step(i32),    // in mode → nudge width by this many units (±1 / ±10)
    Swallow,      // in mode, unrecognised key → consume, stay in mode
    PassThrough,  // forward to the child (the normal path)
}

/// Classify one scanned unit against the reserved chord and the current mode.
///
/// The chord is checked in BOTH states: not-in-mode it enters, in-mode it exits —
/// so a chord that happens to be a letter can never collide with an in-mode
/// command. `Chord::matches` fires on presses only, so a held chord's repeats and
/// its own key-up cannot toggle the mode back off (ADR-016).
///
/// The in-mode set is exact byte strings, no parsing. Repeats act on step keys
/// (holding `h` keeps shrinking): an auto-repeating key sends its byte again, and
/// a kitty repeat report reduces to the same byte as its press.
///
/// A relayed keyboard mode (ADR-021) reports those keys as `CSI` sequences rather than
/// bare bytes, so a report is first reduced to the byte the key would have sent — the
/// step keys as much as the exit. Widening the exit alone and leaving the steps on bare
/// bytes would leave the mode half-working on exactly the terminals the widening is for:
/// Escape gets you out, and until then every step key is silently swallowed.
fn classify_unit(unit: &[u8], chord: &Chord, in_mode: bool) -> KeyAction {
    if chord.matches(unit) {
        return if in_mode { KeyAction::Exit } else { KeyAction::Enter };
    }
    if !in_mode {
        return KeyAction::PassThrough;
    }
    // A bare Escape, whether resolved by the hold or arriving as one of the
    // disambiguated forms a relayed keyboard mode produces.
    if Chord::ESC.matches(unit) {
        return KeyAction::Exit;
    }
    let reduced = chord::csi_key(unit)
        .filter(|k| k.event != KeyEvent::Release)
        .and_then(|k| k.literal());
    let unit: &[u8] = match &reduced {
        Some(b) => std::slice::from_ref(b),
        None => unit,
    };
    match unit {
        b"h" | b"-" => KeyAction::Step(-1),
        b"l" | b"+" | b"=" => KeyAction::Step(1),
        b"H" => KeyAction::Step(-10),
        b"L" => KeyAction::Step(10),
        // Left/Right in both cursor-key modes: normal (CSI) and DECCKM (SS3).
        b"\x1b[D" | b"\x1bOD" => KeyAction::Step(-1),
        b"\x1b[C" | b"\x1bOC" => KeyAction::Step(1),
        _ => KeyAction::Swallow,
    }
}

/// The resize handler — the ADR-008 ordering plus the ADR-011 proportional-width
/// recompute, in one render-thread turn. Recompute `W` → resize the PTY → resize the
/// parser (`set_size(rows, W)` — param order is the trap) → recompute the margin →
/// reset the diff baseline, clearing the band's row-span across both screen modes.
///
/// The clear is uniform (ADR-0017): alt clears `0..rows`, primary clears
/// `base_row..rows` so shell history above the inline band is never touched.
fn handle_resize<R: PtyResizer, T: OuterTerminal>(
    renderer: &mut Renderer,
    resizer: &R,
    term: &mut T,
    cols: u16,
    rows: u16,
) {
    // Step 0 — recompute W (identity for an absolute width).
    let w = geometry::resolve_width(renderer.width_config, cols);

    // Step 1 — resize the PTY FIRST: cols = band width W, NEVER real_cols.
    let _ = resizer.resize(w, rows);

    // Step 2 — resize the parser screen IMMEDIATELY, same turn. (rows, cols).
    renderer.parser.screen_mut().set_size(rows, w);
    // The tracker carries the child's screen state, region and alt flag included
    // (ADR-013), so it is resized alongside the live parser and never rebuilt.
    renderer.scroll_tracker.screen_mut().set_size(rows, w);

    // Update the live geometry.
    renderer.width = w;
    renderer.real_cols = cols;
    // Step 3 — recompute the left margin from the new real_cols and W.
    renderer.left_margin = geometry::margin(renderer.layout, cols, w);

    // Clamp the inline anchor (ADR-013): a height shrink could strand grid row 0
    // below the new bottom, so clamp it back on-screen. The next frame's make-room
    // scroll finishes pushing any overshoot up.
    renderer.base_row = renderer.base_row.min(rows.saturating_sub(1));

    // Step 4 — clear the gutter across the band's row span (uniform margin
    // management, ADR-0017). Screen-mode aware inside `repaint_margins`: alt clears
    // `0..rows`, primary clears `base_row..rows` so history above the band survives.
    //
    // `resize_active` is hard-coded `false` here rather than threaded from the
    // resize-mode flag: `ResizeCtl` lives in `run`'s local scope, not on `Renderer`,
    // so it isn't reachable from this call site without widening `dispatch`'s
    // signature. Not a gap: `apply_message`'s post-dispatch check
    // (`was_resize && resize.active()`) already calls `refresh_resize_overlay` right
    // after this returns, which repaints with the rails when the mode really is
    // active. Both calls are queued, not flushed (ADR-007), so an in-mode SIGWINCH
    // costs one redundant queued clear, never a visible flicker.
    //
    // Interior clear (see `clear_row_span`) across the band span first: the baseline
    // reset below makes rows_diff skip cells the new frame leaves blank, so any glyph
    // the old geometry left there would survive. `repaint_margins` redraws on top.
    let _ = term.clear_row_span(renderer.span_offset(), rows);
    let _ = repaint_margins(renderer, term, /* resize_active: */ false);

    // Force a full repaint next frame: reset the diff baseline to a blank grid of the
    // new size so rows_diff re-emits every row into the resized band.
    renderer.reset_prev_baseline();
}

/// A snapshot of the band's physical geometry, captured before a geometry change so
/// the vacated rail/readout cells can be blanked afterwards. Growing the band moves
/// old rail columns (`prev margin - 1`, `prev band_end`) INSIDE the new band, where
/// the uniform `clear_gutter` (new geometry) and `draw_rails` (new geometry) never
/// reach them — and `render_once` skips blank rows, so the ghost glyphs would
/// otherwise survive indefinitely. `refresh_resize_overlay` blanks exactly the cells
/// this snapshot describes before repainting at the current geometry.
#[derive(Debug, Clone)]
pub(crate) struct BandGeom {
    pub left_margin: u16,
    pub width: u16,
    pub real_cols: u16,
    pub rows: u16,
    /// Row span offset the rails/readout were drawn at: 0 on the alt screen,
    /// `base_row` on the primary screen.
    pub offset: u16,
    /// The readout text as last drawn, so its exact span can be blanked.
    pub readout: String,
}
impl BandGeom {
    pub(crate) fn of(r: &Renderer) -> Self {
        let offset = r.span_offset();
        Self {
            left_margin: r.left_margin,
            width: r.width,
            real_cols: r.real_cols,
            rows: r.parser.screen().size().0,
            offset,
            readout: geometry::readout_text(r.width_config, r.width),
        }
    }
}

/// Enter the visual mode: paint the rails + readout for the current geometry.
/// Geometry is unchanged on enter, so no diff-baseline reset is needed — the
/// clear-then-draw simply paints the rails over the already-blank gutter.
pub(crate) fn enter_resize_overlay<T: OuterTerminal>(
    renderer: &Renderer,
    term: &mut T,
) -> std::io::Result<()> {
    repaint_margins(renderer, term, true)
}

/// Refresh the overlay after a width change: blank the vacated chrome from `prev`'s
/// geometry (see [`BandGeom`]), then uniformly clear the band's row span (ADR-0017,
/// both screen modes) and redraw the rails + readout at the current geometry.
pub(crate) fn refresh_resize_overlay<T: OuterTerminal>(
    renderer: &Renderer,
    term: &mut T,
    prev: Option<BandGeom>,
) -> std::io::Result<()> {
    if let Some(p) = &prev {
        blank_vacated_chrome(term, p)?;
    }
    repaint_margins(renderer, term, true)
}

/// Blank the rail + readout cells a previous frame painted at `prev`'s geometry.
/// Only load-bearing on grow: the vacated rail columns land inside the new band,
/// past the reach of both `clear_gutter` (new geometry) and `draw_rails` (new
/// geometry). On shrink the old cells already fall in the new gutter, which
/// `clear_gutter` blanks anyway, so this is a harmless double-clear there.
fn blank_vacated_chrome<T: OuterTerminal>(term: &mut T, prev: &BandGeom) -> std::io::Result<()> {
    let rails = geometry::rail_layout(
        prev.real_cols,
        prev.left_margin,
        prev.width,
        prev.offset,
        prev.rows,
        &prev.readout,
    );
    // Reset SGR first so the blanks are painted with the default background
    // (a leftover attribute run would paint the vacated cells as a coloured block).
    term.write_row(b"\x1b[0m")?;
    for row in rails.row_start..rails.row_end {
        if let Some(c) = rails.left_col {
            term.move_to(c, row)?;
            term.write_row(b" ")?;
        }
        if let Some(c) = rails.right_col {
            term.move_to(c, row)?;
            term.write_row(b" ")?;
        }
    }
    if let Some(r) = &rails.readout {
        term.move_to(r.col, r.row)?;
        term.write_row(" ".repeat(r.text.chars().count()).as_bytes())?;
    }
    Ok(())
}

/// Leave the visual mode: uniformly clear the band's row span, erasing the rails
/// and readout (and blanking the readout's in-band fallback, see
/// [`repaint_margins`]). Runs on Esc / chord-exit / idle-exit / shutdown. Callers
/// pair this with `renderer.reset_prev_baseline()` so the next `render_once` fully
/// repaints the band, restoring any child content the readout had overwritten.
pub(crate) fn clear_resize_overlay<T: OuterTerminal>(
    renderer: &Renderer,
    term: &mut T,
) -> std::io::Result<()> {
    repaint_margins(renderer, term, false)
}

/// Apply one resize-mode step: change the band width by `delta` units, holding
/// real_cols fixed, in the ADR-008 order. PRD 0001 §"Resize implementation shape".
///
/// Order (load-bearing, ADR-008): step the config → resolve W → `resizer.resize`
/// (TIOCSWINSZ first) → `set_size(rows, W)` (param-order trap) → update geometry →
/// clear vacated strip + paint rails → reset the diff baseline.
fn apply_resize_step<R: PtyResizer, T: OuterTerminal>(
    renderer: &mut Renderer,
    resizer: &R,
    term: &mut T,
    delta: i32,
) {
    let rows = renderer.parser.screen().size().0;
    let real = renderer.real_cols; // held FIXED (unlike handle_resize)
    let prev = BandGeom::of(renderer); // capture BEFORE mutating

    // Step 0 — unit-preserving step + clamp.
    renderer.width_config = geometry::step_width(renderer.width_config, delta, real);
    let w = geometry::resolve_width(renderer.width_config, real);

    // At a bound the width is unchanged: skip the PTY/parser churn (and the
    // baseline reset), but still refresh the overlay so the readout stays consistent.
    let changed = w != renderer.width;
    if changed {
        // Step 1 — PTY first, cols = W, never real_cols.
        let _ = resizer.resize(w, rows);
        // Step 2 — parser, same turn. (rows, cols).
        renderer.parser.screen_mut().set_size(rows, w);
        renderer.scroll_tracker.screen_mut().set_size(rows, w);
        // Step 3 — live geometry (real_cols unchanged).
        renderer.width = w;
        renderer.left_margin = geometry::margin(renderer.layout, real, w);
        // Interior clear (see `clear_row_span`): the baseline reset below makes rows_diff
        // skip cells the new frame leaves blank, so stale glyphs from the old geometry
        // would survive. A resize step never changes `rows`, so one clear covers the span.
        let _ = term.clear_row_span(renderer.span_offset(), rows);
    }
    // Step 4 — clear the vacated strip + (re)paint rails.
    let _ = refresh_resize_overlay(renderer, term, Some(prev));
    // Step 5 — force a full band repaint next frame.
    if changed {
        renderer.reset_prev_baseline();
    }
}

/// The nearer of two optional deadlines, so Phase A's bounded wait can be capped
/// by whichever of the ESC-hold and the resize-mode idle window comes first.
fn earliest<I: Ord>(a: Option<I>, b: Option<I>) -> Option<I> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (only, None) | (None, only) => only,
    }
}

/// Loop-owned resize-mode control. `Some(deadline)` == in mode, holding the instant
/// idle auto-exit fires; `None` == not in mode. Generic over the clock's `Instant`
/// so the idle window is virtual-clock testable (ADR-007).
struct ResizeCtl<I> {
    idle_deadline: Option<I>,
}
impl<I: Copy + Ord> ResizeCtl<I> {
    fn inactive() -> Self {
        Self { idle_deadline: None }
    }
    fn active(&self) -> bool {
        self.idle_deadline.is_some()
    }
    fn arm(&mut self, deadline: I) {
        self.idle_deadline = Some(deadline);
    }
    fn disarm(&mut self) {
        self.idle_deadline = None;
    }
}

/// Loop-owned input control: the scanner and the ESC-hold deadline. Generic over
/// the clock's `Instant` so the hold is virtual-clock testable (ADR-007/020).
struct InputCtl<I> {
    scanner: Scanner,
    hold_deadline: Option<I>,
}
impl<I: Copy + Ord> InputCtl<I> {
    fn new() -> Self {
        Self {
            scanner: Scanner::new(),
            hold_deadline: None,
        }
    }
}

/// Apply one `KeyAction` that gutter consumes. `PassThrough` never reaches here —
/// the caller forwards those bytes itself.
fn apply_key_action<C, T, R>(
    action: KeyAction,
    clock: &mut C,
    renderer: &mut Renderer,
    resize: &mut ResizeCtl<C::Instant>,
    term: &mut T,
    resizer: &R,
) where
    C: Clock<Msg = Msg>,
    T: OuterTerminal,
    R: PtyResizer,
{
    match action {
        KeyAction::Enter => {
            // Two statements, NOT `clock.deadline(clock.now(), ..)`: `deadline`
            // takes `&self` and `now` takes `&mut self`, so nesting them in one
            // expression is an E0502 overlapping borrow.
            let now = clock.now();
            resize.arm(clock.deadline(now, RESIZE_IDLE));
            let _ = renderer.begin_resize(term);
            let _ = enter_resize_overlay(renderer, term);
        }
        KeyAction::Step(delta) => {
            let now = clock.now();
            resize.arm(clock.deadline(now, RESIZE_IDLE)); // a resize key = activity
            apply_resize_step(renderer, resizer, term, delta);
        }
        KeyAction::Exit => leave_resize_mode(resize, renderer, term),
        // Consumed but NOT counted as activity: a swallowed stray key must not
        // keep the mode alive forever (PRD 0001: idle = "no resize key").
        KeyAction::Swallow => {}
        KeyAction::PassThrough => {}
    }
}

/// Leave resize mode: disarm the idle window, un-suppress the mirrored cursor, erase
/// the overlay and force a full band repaint. Queued, not flushed — the caller renders.
fn leave_resize_mode<T: OuterTerminal, I: Copy + Ord>(
    resize: &mut ResizeCtl<I>,
    renderer: &mut Renderer,
    term: &mut T,
) {
    if !resize.active() {
        return;
    }
    resize.disarm();
    renderer.end_resize();
    let _ = clear_resize_overlay(renderer, term);
    renderer.reset_prev_baseline();
}

/// Write bytes to the child, the default action for everything the scanner did
/// not consume.
fn forward_to_child<P: Write>(pty_writer: &mut P, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let _ = pty_writer.write_all(bytes);
    let _ = pty_writer.flush();
}

/// Walk one chunk's tokens, forwarding or consuming each.
///
/// All splitting of an ordinary byte run happens here rather than in the scanner,
/// because this is the layer that knows whether resize mode is active — and it
/// re-reads that at every step, so a chunk carrying the chord and two step keys
/// (`0x1C l l`) enters the mode mid-run and applies both steps.
fn walk_tokens<C, T, P, R>(
    tokens: &[Token],
    clock: &mut C,
    renderer: &mut Renderer,
    resize: &mut ResizeCtl<C::Instant>,
    term: &mut T,
    pty_writer: &mut P,
    resizer: &R,
) where
    C: Clock<Msg = Msg>,
    T: OuterTerminal,
    P: Write,
    R: PtyResizer,
{
    for token in tokens {
        match token {
            // A paste — guards included — and a string-sequence payload are data,
            // not input protocol: nothing is extracted, nothing is matched,
            // nothing is dropped, in resize mode or out of it.
            Token::Paste(bytes) | Token::Str(bytes) => forward_to_child(pty_writer, bytes),
            Token::Mouse(report) => {
                // The mouse forwarding gate (ADR-005). Read the child's
                // (mode, encoding) from the live screen FIRST: this runs after the
                // frame's Msg::Pty bytes applied, so a DECSET the child just sent is
                // already visible. The gate translates the coordinate, down-filters
                // motion, and re-encodes SGR.
                let screen = renderer.parser.screen();
                let mode = screen.mouse_protocol_mode();
                let encoding = screen.mouse_protocol_encoding();
                match renderer.mouse_gate.forward(
                    report,
                    mode,
                    encoding,
                    renderer.left_margin,
                    renderer.width,
                ) {
                    MouseDecision::Forward(bytes) => forward_to_child(pty_writer, &bytes),
                    MouseDecision::Swallow => {}
                    // A reporting mode with a non-SGR encoding is out of v1 scope, so
                    // the report is dropped rather than sent in a form that would
                    // desync the child's mouse parser (ADR-005).
                    //
                    // Dropped, not fatal: a child that enables `?1000h` without
                    // `?1006h` is ordinary, and the outer terminal reports in SGR
                    // either way (the eager capture), so this is one click away on a
                    // plain older TUI. Dying here would take the render thread down
                    // with raw mode and mouse reporting still on and the ordered
                    // restore never run (ADR-010) — the shell would need `reset`.
                    MouseDecision::BailNonSgr => {}
                }
            }
            // A complete escape sequence is atomic: matched whole or forwarded
            // whole, never split into an Escape plus literal characters.
            Token::Seq(bytes) => {
                // The rest of a press gutter already consumed. It has to be caught
                // before the classifier, because leaving the mode is what makes the
                // release look like an ordinary key to forward.
                if renderer.owed_key_report(bytes, resize.active()) {
                    continue;
                }
                match classify_unit(bytes, &renderer.resize_key, resize.active()) {
                    KeyAction::PassThrough => forward_to_child(pty_writer, bytes),
                    action => {
                        renderer.note_consumed_press(bytes);
                        apply_key_action(action, clock, renderer, resize, term, resizer);
                    }
                }
            }
            Token::Text(run) => {
                let mut i = 0;
                while i < run.len() {
                    if resize.active() {
                        let action = classify_unit(&run[i..i + 1], &renderer.resize_key, true);
                        apply_key_action(action, clock, renderer, resize, term, resizer);
                        i += 1;
                        continue;
                    }
                    // Out of mode the only byte gutter withholds is the chord's
                    // single-byte form; everything up to it is one write, so a
                    // burst of typing costs one write per chunk.
                    let rest = &run[i..];
                    match renderer
                        .resize_key
                        .single_byte()
                        .and_then(|b| rest.iter().position(|&x| x == b))
                    {
                        Some(at) => {
                            forward_to_child(pty_writer, &rest[..at]);
                            apply_key_action(
                                KeyAction::Enter,
                                clock,
                                renderer,
                                resize,
                                term,
                                resizer,
                            );
                            i += at + 1;
                        }
                        None => {
                            forward_to_child(pty_writer, rest);
                            break;
                        }
                    }
                }
            }
        }
    }
}

/// The ESC-hold expired: emit whatever the scanner withheld and clear the
/// deadline. The flush is verbatim and in order — a held `ESC [` reaches the
/// child as two bytes, never as an Escape followed by a `[` keystroke.
fn flush_hold<C, T, P, R>(
    input: &mut InputCtl<C::Instant>,
    clock: &mut C,
    renderer: &mut Renderer,
    resize: &mut ResizeCtl<C::Instant>,
    term: &mut T,
    pty_writer: &mut P,
    resizer: &R,
) where
    C: Clock<Msg = Msg>,
    T: OuterTerminal,
    P: Write,
    R: PtyResizer,
{
    let mut tokens = Vec::new();
    input.scanner.flush(&mut tokens);
    input.hold_deadline = None;
    walk_tokens(&tokens, clock, renderer, resize, term, pty_writer, resizer);
}

/// Handle one live-loop message: raw input goes through the scanner and the token
/// walk (ADR-020), everything else delegates to `dispatch`. Returns the child exit
/// code exactly as `dispatch` does.
#[allow(clippy::too_many_arguments)]
fn apply_message<C, T, P, R>(
    m: Msg,
    clock: &mut C,
    renderer: &mut Renderer,
    resize: &mut ResizeCtl<C::Instant>,
    input: &mut InputCtl<C::Instant>,
    term: &mut T,
    pty_writer: &mut P,
    resizer: &R,
) -> Flow
where
    C: Clock<Msg = Msg>,
    T: OuterTerminal,
    P: Write,
    R: PtyResizer,
{
    if let Msg::Input(bytes) = &m {
        // The paste gate reads what gutter told the OUTER terminal (ADR-022), because
        // only a terminal that was sent `?2004h` can have produced the guards. Same
        // thread as the mirror, so this is a plain field read.
        input
            .scanner
            .set_paste_guards(renderer.mode_mirror.bracketed_paste());
        let mut tokens = Vec::new();
        input.scanner.feed(bytes, &mut tokens);
        walk_tokens(&tokens, clock, renderer, resize, term, pty_writer, resizer);
        // Re-arm on every feed, not just the not-holding→holding edge: a chunk
        // that EXTENDS an incomplete sequence is evidence more is coming, so
        // restarting the clock is the right behaviour.
        input.hold_deadline = if input.scanner.holding() {
            let now = clock.now();
            Some(clock.deadline(now, ESC_HOLD))
        } else {
            None
        };
        return Flow::Continue;
    }
    // A terminal resize (SIGWINCH) while in mode moved the band — repaint the
    // overlay after handle_resize has updated the geometry. Capture the geometry
    // BEFORE dispatch mutates it, so a grow can blank the vacated rail columns
    // handle_resize's own (rails-blind) clear left behind.
    let was_resize = matches!(m, Msg::Resize);
    let prev = (was_resize && resize.active()).then(|| BandGeom::of(renderer));
    let code = dispatch(m, renderer, pty_writer, resizer, term);
    if was_resize && resize.active() {
        let _ = refresh_resize_overlay(renderer, term, prev);
    }
    code
}

/// Paint the current virtual grid to the outer terminal at the band's offset
/// (ADR-006), mirroring the child's screen mode first (ADR-012). The only place
/// `rows_diff` and the cursor moves are emitted.
///
/// Mirror the child's alt-screen state (edge-triggered). On the primary screen, paint
/// at `base_row + grid_row` so the band grows downward from the launch row, scrolling
/// the real terminal up to make room as it fills (ADR-013). Lines that left the top of
/// the W-window this frame are streamed into the terminal's own scrollback via
/// [`emit_scroll_stream`]; otherwise each changed row repaints in place. Finally mirror
/// the cursor visibility/shape and reposition the real cursor inside the band.
fn render_once<T: OuterTerminal>(renderer: &mut Renderer, term: &mut T) -> std::io::Result<()> {
    // Mirror the child's alt-screen state (ADR-012), edge-triggered. Read the flag
    // before the long immutable borrow of `screen` below.
    let child_alt = renderer.parser.screen().alternate_screen();
    if child_alt != renderer.outer_alt_active {
        if child_alt {
            term.enter_alt_screen()?;
        } else {
            term.leave_alt_screen()?;
            // An alt→primary edge lands on a fresh primary screen; the cached alt
            // frame is not a valid diff baseline, so force a full repaint.
            renderer.reset_prev_baseline();
        }
        renderer.outer_alt_active = child_alt;
    }

    // Mirror the input modes vt100 swallowed into its screen state (ADR-022): DECCKM,
    // application keypad and bracketed paste, each edge-triggered against what was last
    // mirrored. Sitting next to the alt-screen mirror is legibility, not ordering — all
    // three are terminal-global and survive a `?1049` either way — but being ahead of
    // the paint loop is load-bearing: a painted row is a self-contained run inside its
    // margin-offset rectangle (ADR-014), and mode bytes between a `move_to` and its row
    // would break that for nothing.
    let mode_bytes = renderer.mode_mirror.take_pending(renderer.parser.screen());
    relay_if_any(term, &mode_bytes)?;

    // Scroll emit (ADR-013): on the primary screen, advance each line that left the
    // top of the W-window this frame into the terminal's own scrollback. The tracker
    // follows the child into the alternate screen, where vt100 keeps no scrollback and
    // nothing ever departs, so an alt frame drains empty on its own — the outer screen
    // never scrolls under the alt screen.
    let departed = renderer.drain_scrolled_off();

    scroll_to_make_room(renderer, term, departed.is_empty())?;

    let offset = renderer.span_offset();

    if departed.is_empty() {
        // No scroll: the ordinary per-row diff paint. Only rows changed since the last
        // frame are re-emitted, at offset + row.
        let mut painted = false;
        let mut row_buf = Vec::new();
        let screen = renderer.parser.screen();
        let prev_screen = renderer.prev.screen();
        for (row, line) in screen.rows_diff(prev_screen, 0, renderer.width).enumerate() {
            if line.is_empty() {
                continue;
            }
            let row = row as u16;
            let at = Placement {
                left_margin: renderer.left_margin,
                phys_row: offset.saturating_add(row),
                grid_row: row,
            };
            term.move_to(at.left_margin, at.phys_row)?;
            prepare_row_into(&line, renderer.width, at, &mut row_buf);
            term.write_row(&row_buf)?;
            painted = true;
        }
        // Record that inline content reached the primary screen, so teardown hands
        // back below the band (ADR-013).
        if painted && !renderer.outer_alt_active {
            renderer.ever_painted_inline = true;
        }
    } else {
        // A scroll happened (provably at base_row == 0): stream the departed lines
        // then the current band down the band column, letting the terminal's own
        // scrolling carry the departed lines into scrollback. Replaces the diff paint
        // this frame.
        emit_scroll_stream(renderer, term, &departed)?;
        renderer.ever_painted_inline = true;
    }

    mirror_cursor(renderer, term, offset)?;

    term.flush()?;

    // The current screen becomes the next frame's diff baseline. `vt100::Parser` has no
    // `clone`, so re-process the formatted state into prev — a cheap in-memory replay.
    renderer.sync_prev();
    Ok(())
}

/// Make room as the band grows inline (ADR-013, primary only). If the deepest live
/// row would run past the bottom, scroll the real terminal up by the overshoot (a
/// newline per line, pushing history into the terminal's own scrollback) and drop
/// base_row by the same delta. Scrolling up by delta shifts every already-painted
/// row up too, so the diff-skipped rows are already in place and only changed rows
/// repaint. When the grid is full this has driven base_row to 0.
fn scroll_to_make_room<T: OuterTerminal>(
    renderer: &mut Renderer,
    term: &mut T,
    departed_empty: bool,
) -> std::io::Result<()> {
    if renderer.outer_alt_active || renderer.base_row == 0 {
        return Ok(());
    }
    let real_rows = renderer.parser.screen().size().0;
    // A non-empty `departed` proves vt100's W-window filled and scrolled this
    // frame, which only happens once the band spans the whole screen — so base_row
    // MUST reach 0 before the scroll-emit branch paints [0, rows), or it overwrites
    // the pre-launch history. The settled grid can read near-blank (a coalesced
    // seq … clear in one frame), so the overshoot would under-scroll; the departed
    // signal is the authority, and scrolling the full base_row drives it to 0.
    let delta = if departed_empty {
        let bottom = renderer.deepest_live_row();
        renderer
            .base_row
            .saturating_add(bottom)
            .saturating_sub(real_rows.saturating_sub(1))
    } else {
        renderer.base_row
    };
    if delta > 0 {
        term.move_to(0, real_rows.saturating_sub(1))?;
        for _ in 0..delta {
            term.newline()?;
        }
        renderer.base_row -= delta;
    }
    Ok(())
}

/// Mirror the child's cursor visibility, shape and position — unless resize mode owns
/// the band, where the block cursor would flicker at the band edge as the child
/// re-homes during redraws. The outer cursor is hidden on mode enter and re-asserted
/// from the child's live state on exit (see [`Renderer::end_resize`]).
fn mirror_cursor<T: OuterTerminal>(
    renderer: &mut Renderer,
    term: &mut T,
    offset: u16,
) -> std::io::Result<()> {
    if renderer.resize_active {
        return Ok(());
    }
    let screen = renderer.parser.screen();
    let visible = !screen.hide_cursor();
    let (crow, ccol) = screen.cursor_position();

    // Mirror DECTCEM: only emit a show/hide when the state actually changed.
    if visible != renderer.cursor_visible {
        term.set_cursor_visible(visible)?;
        renderer.cursor_visible = visible;
    }

    // Mirror DECSCUSR cursor shape: the callbacks watcher recorded any CSI Ps SP q
    // the child emitted; emit it to the outer terminal, de-duped by the watcher.
    if let Some(shape) = renderer.parser.callbacks_mut().cursor_shape.take_pending() {
        term.set_cursor_shape(&shape)?;
    }

    // Reposition the real cursor inside the band at `offset + grid_cursor_row`.
    term.place_cursor(
        geometry::physical_col(renderer.left_margin, ccol),
        offset.saturating_add(crow),
    )
}

/// Make a vt100 row run self-contained within its `W`-wide, margin-offset rectangle
/// before painting (ADR-014): prepend `ESC[m` so it doesn't inherit the previous row's
/// trailing attribute across the bare `move_to`, clip the row-final `ESC[K` to column
/// `W` so its erase can't flood the gutter, and re-express the run's absolute moves in
/// `at`'s physical coordinates.
fn prepare_row_into(line: &[u8], width: u16, at: Placement, out: &mut Vec<u8>) {
    out.clear();
    out.extend_from_slice(b"\x1b[m");
    clip_row_to_width_into(line, width, at, out);
}

/// [`prepare_row_into`] for a row painted over cells the caller has no baseline for:
/// blank the band's columns first, then paint.
///
/// A `rows_formatted` run describes only the cells that differ from a blank one —
/// nothing at all for a row that is now empty — so painted straight onto a physical row
/// still holding an older frame it leaves whatever it does not cover. The blank is the
/// clipper's own `W`-bounded fill, which stops at the band's right edge and returns the
/// cursor to its left one; an `ESC[K` would take the right gutter with it.
fn prepare_row_over_into(line: &[u8], width: u16, at: Placement, out: &mut Vec<u8>) {
    prepare_row_into(b"\x1b[K", width, at, out);
    clip_row_to_width_into(line, width, at, out);
}

/// Paint a scrolling frame: stream the `departed` lines then the current band down the
/// band column, so the terminal's own scrolling carries the departed lines into its
/// scrollback and leaves the band visible (ADR-013, primary only). `departed` is the
/// count-bearing run from the scroll tracker, oldest first — robust to a coalesced
/// burst that turned the whole screen over in one frame.
///
/// The stream is `departed ++ band`. The first `rows` lines fill rows `[0, rows)` in
/// place (so nothing pre-existing leaks into scrollback); each line beyond the bottom
/// is preceded by a `newline()` that scrolls one row off the top before the new line is
/// written at the bottom. Only the band columns are written; each `\r\n` scrolls the
/// whole physical row, so any shell history in the gutter scrolls up with it.
fn emit_scroll_stream<T: OuterTerminal>(
    renderer: &Renderer,
    term: &mut T,
    departed: &[Vec<u8>],
) -> std::io::Result<()> {
    let rows = renderer.parser.screen().size().0;
    let bottom = rows.saturating_sub(1);
    let band: Vec<Vec<u8>> = renderer
        .parser
        .screen()
        .rows_formatted(0, renderer.width)
        .collect();

    let mut row_buf = Vec::new();
    // Each departed line is the top row of a scrollback offset, so vt100 built it as
    // grid row 0; the band's lines carry their own index.
    let departed_rows = departed.iter().map(|line| (0u16, line));
    let band_rows = band.iter().enumerate().map(|(row, line)| (row as u16, line));
    for (i, (grid_row, line)) in departed_rows.chain(band_rows).enumerate() {
        let i = i as u16;
        let in_place = i <= bottom;
        let phys_row = if in_place {
            // Still filling the screen top-down — overwrite row i in place, no scroll.
            i
        } else {
            // Past the bottom: scroll one row into scrollback, then write at the bottom.
            term.newline()?;
            bottom
        };
        let at = Placement {
            left_margin: renderer.left_margin,
            phys_row,
            grid_row,
        };
        term.move_to(at.left_margin, at.phys_row)?;
        if in_place {
            // The last frame's paint is still on this row and no diff accounts for it:
            // the run has to blank the band's columns itself. A row written after a
            // `newline` scrolled in blank, so it needs none of that.
            prepare_row_over_into(line, renderer.width, at, &mut row_buf);
        } else {
            prepare_row_into(line, renderer.width, at, &mut row_buf);
        }
        term.write_row(&row_buf)?;
    }
    Ok(())
}

/// The cell-walking **fallback** repaint (ADR-006), dormant by decision: gutter renders
/// via the `rows_diff` path only, and there is no production caller. Walks the grid
/// cell-by-cell and paints each cell at its physical column `left_margin + col`. Kept
/// correct and exercised by the cell-walk edge-of-band tests as insurance against a
/// corrupting cell the design believes the margin rule prevents.
///
/// The only place in the binary that calls [`vt100::Cell::is_wide_continuation`]. A
/// double-width glyph occupies a lead cell (holding the glyph) and a continuation cell
/// (byte-length zero). We skip the continuation cell BEFORE any move/emit: skipping the
/// empty cell loses nothing, and a `move_to` to its column would itself be a defect. The
/// lead cell owns the glyph's full width, so the loop's natural `col += 1` over the
/// skipped continuation accounts for the second column — we never advance twice for one
/// glyph, and never paint a lead glyph's right half into `left_margin + W`.
#[allow(dead_code)]
fn render_cell_walk<T: OuterTerminal>(
    screen: &vt100::Screen,
    left_margin: u16,
    width: u16,
    term: &mut T,
) -> std::io::Result<()> {
    let (rows, _) = screen.size();
    for row in 0..rows {
        for col in 0..width {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            // The continuation half of a wide glyph: skip BEFORE any move/emit.
            if cell.is_wide_continuation() {
                continue;
            }
            // Blank cells need no paint — the grid started clear and the gutter clear
            // owns the empties.
            let contents = cell.contents();
            if contents.is_empty() {
                continue;
            }
            // Paint the (possibly wide) cell at its physical column. vt100 never places
            // a wide lead at W-1, so the glyph's right half never lands at left_margin+W.
            term.move_to(geometry::physical_col(left_margin, col), row)?;
            term.write_row(contents.as_bytes())?;
        }
    }
    term.flush()?;
    Ok(())
}

/// Repaint the band chrome after a geometry change (ADR-008/0017).
/// Clears the vacated gutter strip across the band's row span — `0..rows` on the alt
/// screen, `base_row..rows` on the primary screen, so shell history above the inline
/// band is preserved (ADR-012/013) — and, when `resize_active`, draws the faint rails
/// and width readout over it. When `resize_active` is false it also blanks the
/// readout's in-band fallback cells (see below). Does NOT reset the diff baseline;
/// the caller pairs this with `reset_prev_baseline()`.
fn repaint_margins<T: OuterTerminal>(
    renderer: &Renderer,
    term: &mut T,
    resize_active: bool,
) -> std::io::Result<()> {
    let offset = renderer.span_offset();
    let rows = renderer.parser.screen().size().0;
    term.clear_gutter(renderer.left_margin, renderer.width, renderer.real_cols, offset, rows)?;
    let text = geometry::readout_text(renderer.width_config, renderer.width);
    let rails = geometry::rail_layout(
        renderer.real_cols,
        renderer.left_margin,
        renderer.width,
        offset,
        rows,
        &text,
    );
    if resize_active {
        term.draw_rails(&rails)?;
    } else if let Some(r) = &rails.readout {
        // Exit clear. The readout's fallback placement is INSIDE the band, which
        // `clear_gutter` never touches — and the paired `reset_prev_baseline` full
        // repaint re-emits only rows that hold content, so a readout sitting on a
        // blank child row would linger forever. Blank its span explicitly: when the
        // readout sat in the gutter this is a harmless double-clear, and when child
        // content occupied those cells the baseline-reset repaint restores it next
        // frame. Recomputing the layout here finds the same cells the rails last
        // drew at: mode exit changes no geometry, and a terminal resize while in
        // mode already redrew the rails at the new geometry.
        term.move_to(r.col, r.row)?;
        term.write_row(" ".repeat(r.text.chars().count()).as_bytes())?;
    }
    Ok(())
}

impl Renderer {
    /// Enter resize mode: suppress the mirrored child cursor. Hides the outer cursor
    /// once and records it hidden, so the cursor tail in `render_once` (skipped while
    /// active) leaves it hidden for the mode's duration.
    fn begin_resize<T: OuterTerminal>(&mut self, term: &mut T) -> std::io::Result<()> {
        self.resize_active = true;
        // Track the outer cursor as hidden: this is what makes `end_resize`'s guard
        // reset re-show only a child whose cursor is actually visible.
        self.cursor_visible = false;
        term.set_cursor_visible(false)
    }

    /// Leave resize mode: re-assert the child's REAL cursor rather than force it
    /// visible. `rearm` re-arms the DECSCUSR shape, and leaving `cursor_visible`
    /// false (the outer cursor's actual hidden state) makes the next `render_once`
    /// re-evaluate DECTCEM from the child's live screen — so a child-hidden cursor
    /// stays hidden while a child-visible one is re-shown.
    fn end_resize(&mut self) {
        self.resize_active = false;
        self.parser.callbacks_mut().cursor_shape.rearm();
        self.cursor_visible = false;
    }

    /// Advance the `prev` baseline to match the current screen, so the next
    /// frame's `rows_diff` is against what was just painted. `vt100::Parser` exposes no
    /// `clone`, so feed `prev` the current screen's `contents_formatted()` —
    /// a full state replay that leaves `prev` cell-identical to `parser`. Unlike the
    /// scroll tracker, `prev` never sees the child's bytes, so it holds no vte state a
    /// whole-parser replacement could lose.
    fn sync_prev(&mut self) {
        self.prev = self.live_mirror(0);
    }

    /// A callback-free parser sized to the live grid — the shape both diff mirrors take.
    fn blank_mirror(&self, scrollback: usize) -> vt100::Parser<GutterCallbacks> {
        let (rows, cols) = self.parser.screen().size();
        vt100::Parser::new_with_callbacks(rows, cols, scrollback, GutterCallbacks::baseline())
    }

    /// A blank mirror replayed up to the live grid's current content. `vt100::Parser`
    /// exposes no `clone` (its `Screen` does), so `contents_formatted()` is the replay.
    fn live_mirror(&self, scrollback: usize) -> vt100::Parser<GutterCallbacks> {
        let mut p = self.blank_mirror(scrollback);
        p.process(&self.parser.screen().contents_formatted());
        p
    }

    /// Drop the diff baseline to a blank grid of the live size, so the next rows_diff
    /// differs on every non-empty row and forces a full repaint (resize, ADR-008 step 5;
    /// the alt→primary edge). The tracker's screen is left alone — only the mark it
    /// counts departures from moves up to the present, so lines that departed before
    /// the change are not re-emitted after it.
    fn reset_prev_baseline(&mut self) {
        self.prev = self.blank_mirror(0);
        self.mark_tracker_counted();
    }

    /// The tracker's current scrollback length. vt100 exposes no accessor, so probe it
    /// by clamping the offset to its max and reading back what it clamped to. On the
    /// alternate screen this reads the alt grid, which vt100 builds with no scrollback
    /// at all, so it is always 0 there.
    fn tracker_scrollback_len(&mut self) -> usize {
        self.scroll_tracker.screen_mut().set_scrollback(usize::MAX);
        let n = self.scroll_tracker.screen().scrollback();
        self.scroll_tracker.screen_mut().set_scrollback(0);
        n
    }

    /// Treat everything now in the tracker's scrollback as already counted, so the next
    /// drain reports only what departs from here on.
    ///
    /// Skipped on the alternate screen, where the probe reads the alt grid's absent
    /// scrollback and would answer 0: a baseline reset during an alt excursion (a
    /// resize, a resume, a `ChildContinued`) would otherwise drop the primary count to
    /// zero and re-offer every line the session had ever scrolled. Nothing departs a
    /// primary grid the child is not on, so there is never anything new to absorb there.
    fn mark_tracker_counted(&mut self) {
        if self.scroll_tracker.screen().alternate_screen() {
            return;
        }
        self.tracker_scrollback = self.tracker_scrollback_len();
    }

    /// The lines that left the top of the W-window this frame, formatted, oldest first
    /// (ADR-013).
    ///
    /// The tracker keeps its screen across frames, so its scrollback holds every line
    /// that has ever departed, and this frame's are the ones beyond `tracker_scrollback`.
    /// At offset `k` the row `k` lines above the current top sits at grid row 0, so
    /// reading the top row at offsets `n..=1` yields the `n` newest in order.
    ///
    /// Nothing is drained while the child is in the alternate screen: vt100 gives the
    /// alt grid no scrollback, so no line ever departs one, and the primary count has to
    /// survive the excursion untouched. The alt→primary edge marks it counted again
    /// through the baseline reset in `render_once`.
    fn drain_scrolled_off(&mut self) -> Vec<Vec<u8>> {
        // The tracker's device-query replies reach no PTY, and its parser outlives the
        // session, so the buffer is emptied every frame — including the alt-screen
        // frames that leave with nothing to drain.
        self.scroll_tracker.callbacks_mut().discard_replies();

        if self.scroll_tracker.screen().alternate_screen() {
            return Vec::new();
        }

        let width = self.width;
        let len = self.tracker_scrollback_len();
        let n = len.saturating_sub(self.tracker_scrollback);

        let mut departed = Vec::with_capacity(n);
        for offset in (1..=n).rev() {
            self.scroll_tracker.screen_mut().set_scrollback(offset);
            // The top row at this offset is the next-oldest departed line.
            if let Some(line) = self.scroll_tracker.screen().rows_formatted(0, width).next() {
                departed.push(line);
            }
        }
        self.scroll_tracker.screen_mut().set_scrollback(0);

        if self.tracker_cap.saturating_sub(len) < n.max(1) {
            // Out of headroom: at the cap vt100 drops the oldest line for each new one,
            // so the length stops growing and every later departure counts as zero. The
            // frame that saturates it is already short-changed — it can only report the
            // room it had, and the rest of its departures are lost, not delayed — so
            // re-seed while a frame the size of this one still fits, not once the deque
            // is full. `n.max(1)` keeps a quiet frame from re-seeding until the deque
            // really is full.
            self.reseed_scroll_tracker();
        } else {
            self.tracker_scrollback = len;
        }
        departed
    }

    /// Replace the tracker's screen with the live grid's content and an empty
    /// scrollback. The only way to empty it — vt100 has no scrollback-clearing API —
    /// and the only thing that costs: `contents_formatted` carries no scroll region and
    /// no alt-screen flag, so a re-seeded tracker counts scrolls inside a region the
    /// child set earlier as departures until the child sets one again. Reserved for a
    /// scrollback with no room left for a frame the size of the last one, where the
    /// alternative is losing that frame's departures outright.
    ///
    /// Only the screen is replaced. The `Parser` owns the vte state machine, and the
    /// tracker is fed the child's raw bytes, so a chunk boundary landing mid escape
    /// sequence — a 2KB OSC 52 clipboard write is easily split — would leave a fresh
    /// parser reading the tail of that sequence as printable text and scrolling its
    /// mirror by lines the child never scrolled. The source mirror is built at the
    /// tracker's own scrollback size because the cap travels with the cloned `Screen`;
    /// cloning a scrollback-0 screen in would kill it for good.
    fn reseed_scroll_tracker(&mut self) {
        let fresh = self.live_mirror(self.tracker_cap);
        *self.scroll_tracker.screen_mut() = fresh.screen().clone();
        self.tracker_scrollback = 0;
    }

    /// The deepest grid row holding live content this frame — whichever reaches further
    /// down, the cursor row or the last non-blank row. Drives the make-room overshoot
    /// (ADR-013) and the teardown hand-back.
    ///
    /// "Live" is a cell property, not a text one: a reverse-video status bar erased under
    /// a background SGR carries no glyphs, so a text-only scan would miss it and
    /// under-scroll. Scan from the bottom up to the first row holding any live cell.
    fn deepest_live_row(&self) -> u16 {
        let screen = self.parser.screen();
        let (crow, _) = screen.cursor_position();
        let (rows, _) = screen.size();
        for row in (0..rows).rev() {
            for col in 0..self.width {
                if screen.cell(row, col).is_some_and(cell_is_live) {
                    return crow.max(row);
                }
            }
        }
        crow
    }
}

/// Whether a vt100 cell carries anything the band must keep on-screen: a glyph or a
/// non-default visual attribute. The attribute arm catches an erased reverse-video
/// status bar — a coloured background with no glyphs, which a content-only read would
/// call blank.
fn cell_is_live(cell: &vt100::Cell) -> bool {
    let c = cell.contents();
    (!c.is_empty() && c != " ")
        || cell.inverse()
        || cell.bold()
        || cell.dim()
        || cell.italic()
        || cell.underline()
        || cell.fgcolor() != vt100::Color::Default
        || cell.bgcolor() != vt100::Color::Default
}

/// Run the render loop until the child exits, then restore the terminal in order
/// and return the child's exit code (ADR-007 + ADR-010).
///
/// Everything time- and channel-related is behind `clock`, and the PTY writer
/// and outer terminal are injected, so a test drives the loop with a virtual
/// clock + scripted messages, a `Vec<u8>` PTY-writer sink, and a mock
/// [`OuterTerminal`] — no threads, no PTY, no real terminal, no wall clock.
///
/// Returns the exit code to propagate. `None` means the channel disconnected
/// without a `ChildExited` (the backstop path) — `main` treats that as a clean
/// exit but it is not the normal shutdown route.
pub fn run<C, T, P, R, S>(
    clock: &mut C,
    renderer: &mut Renderer,
    term: &mut T,
    pty_writer: &mut P,
    resizer: &R,
    suspender: &S,
) -> Option<i32>
where
    C: Clock<Msg = Msg>,
    T: OuterTerminal,
    P: Write,
    R: PtyResizer,
    S: Suspender,
{
    let mut exit_code: Option<i32> = None;
    let mut resize = ResizeCtl::inactive();
    let mut input = InputCtl::new();

    'frames: loop {
        // Top-of-frame hold check: a hold that expired while the loop was busy
        // draining a PTY burst flushes this frame rather than at the burst's end.
        // Runs before the idle check because it is the cheaper one and its flushed
        // bytes may feed the mode.
        if let Some(dl) = input.hold_deadline
            && clock.now() >= dl
        {
            flush_hold(
                &mut input, clock, renderer, &mut resize, term, pty_writer, resizer,
            );
            // The flushed Escape may have left resize mode, whose overlay clear
            // is only queued — render so it reaches the terminal even if the
            // child never writes again.
            let _ = render_once(renderer, term);
            continue 'frames;
        }

        // Top-of-frame idle check (handles a flooding child that never lets Phase A
        // block): if the idle deadline has already passed, exit the mode and repaint
        // before doing anything else this frame.
        if let Some(dl) = resize.idle_deadline
            && clock.now() >= dl
        {
            leave_resize_mode(&mut resize, renderer, term);
            let _ = render_once(renderer, term); // erase rails this frame
            continue 'frames;
        }

        // --- Phase A: block for the first message (zero idle CPU with no deadline
        // pending; a bounded wait while resize mode or the ESC-hold is armed, so a
        // quiet child still wakes for the auto-exit or the flush) ---
        let first = if let Some(dl) = earliest(input.hold_deadline, resize.idle_deadline) {
            match clock.recv_until(dl) {
                Recv::Msg(m) => m,
                Recv::Timeout => {
                    // Both deadlines can fire in one wake; the hold goes first,
                    // since it is the cheaper one and might feed the mode.
                    let now = clock.now();
                    if input.hold_deadline.is_some_and(|d| now >= d) {
                        flush_hold(
                            &mut input, clock, renderer, &mut resize, term, pty_writer, resizer,
                        );
                    }
                    if resize.idle_deadline.is_some_and(|d| now >= d) {
                        // ~3 s idle elapsed.
                        leave_resize_mode(&mut resize, renderer, term);
                    }
                    // Both paths may have queued an overlay clear; flush it here
                    // rather than waiting for a child that may never write again.
                    let _ = render_once(renderer, term);
                    continue 'frames;
                }
                Recv::Disconnected => break 'frames,
            }
        } else {
            match clock.recv() {
                Some(m) => m,
                // Backstop only: all senders gone with no ChildExited.
                None => break 'frames,
            }
        };
        let mut shutdown = false;
        let exiting = match apply_message(
            first, clock, renderer, &mut resize, &mut input, term, pty_writer, resizer,
        ) {
            Flow::Continue => None,
            Flow::Exit(code) => Some(code),
            // The child stopped (ADR-0019): run the reversible park → self-stop →
            // unpark → continue cycle. `continue 'frames` on resume so the next frame
            // captures a fresh deadline — no stale pre-stop deadline survives the
            // (arbitrarily long) suspension. An abort (the child SIGKILLed right after
            // stopping) never parked the terminal, so it joins the ordinary shutdown.
            Flow::Suspend => match suspend_cycle(
                clock, renderer, &mut resize, term, pty_writer, resizer, suspender,
            ) {
                SuspendOutcome::Resumed => continue 'frames,
                SuspendOutcome::ChildExited(code) => Some(code),
            },
        };
        if let Some(code) = exiting {
            exit_code = Some(code);
            // Trailing bytes queued behind the ChildExited must land before teardown
            // reads `outer_alt_active` (ADR-013's teardown race).
            drain_pty_path(clock, renderer, pty_writer, resizer, term);
            shutdown = true;
        }

        let frame_start = clock.now();
        let deadline = clock.deadline(frame_start, FRAME);

        // --- Phase B: drain to the deadline ---
        if !shutdown {
            loop {
                // MANDATORY explicit burst-exit check (ADR-007): NOT redundant with the
                // Timeout arm. Under a saturating burst recv_until returns Msg forever
                // and never Timeout, so without this the render is deferred all burst.
                if clock.now() >= deadline {
                    break;
                }
                match clock.recv_until(deadline) {
                    Recv::Msg(m) => {
                        let exiting = match apply_message(
                            m, clock, renderer, &mut resize, &mut input, term, pty_writer,
                            resizer,
                        ) {
                            Flow::Continue => None,
                            Flow::Exit(code) => Some(code),
                            Flow::Suspend => match suspend_cycle(
                                clock, renderer, &mut resize, term, pty_writer, resizer,
                                suspender,
                            ) {
                                SuspendOutcome::Resumed => continue 'frames,
                                SuspendOutcome::ChildExited(code) => Some(code),
                            },
                        };
                        if let Some(code) = exiting {
                            exit_code = Some(code);
                            drain_pty_path(clock, renderer, pty_writer, resizer, term);
                            shutdown = true;
                            break;
                        }
                    }
                    Recv::Timeout => break, // idle-gap exit
                    Recv::Disconnected => {
                        shutdown = true;
                        break;
                    }
                }
            }
        }

        // --- Exactly one render per frame ---
        let _ = render_once(renderer, term);

        if shutdown {
            break 'frames;
        }
    }

    // Clear the overlay on shutdown so a child that exits mid-mode leaves no rails.
    if resize.active() {
        let _ = clear_resize_overlay(renderer, term);
    }

    // Explicit ordered restore BEFORE process::exit (ADR-010): process::exit runs no
    // destructors, so this can't be a Drop guard. The exit code feeds the dim status
    // line; None (channel disconnected without a ChildExited) maps to a clean exit. The
    // renderer is read for the alt state and the inline anchor (ADR-012/013).
    let _ = run_teardown(renderer, term, exit_code.unwrap_or(0));
    exit_code
}

/// How the suspend/resume cycle ended.
enum SuspendOutcome {
    /// The shell `fg`'d gutter; the terminal is re-set-up and the band repainted.
    Resumed,
    /// The child died before gutter self-stopped (SIGKILL on the just-stopped
    /// proc), caught in the pre-stop drain — the terminal was never parked, so
    /// `run` tears down normally.
    ChildExited(i32),
}

/// Quiet-gap and hard cap for the pre-stop drain (step 1). The child emitted its
/// terminal-restore bytes *before* stopping, but nothing guarantees they beat
/// `Msg::ChildStopped` through the two channels — and no `Msg::PtyEof` will ever
/// come (the PTY is not at EOF, the child is merely stopped). So drain each queued
/// message until a quiet gap or the cap.
const SUSPEND_QUIET_GAP: Duration = Duration::from_millis(20);
const SUSPEND_DRAIN_CAP: Duration = Duration::from_millis(100);

/// The suspend/resume cycle (ADR-0019), run entirely on Thread 2 — still the sole
/// owner of the parser, the outer terminal and the PTY writer, so the whole thing
/// is lock-free straight-line code. Park the outer terminal to a sane state, stop
/// gutter's own process group (`suspend_self` returns only once the shell `fg`s
/// us), then unpark, wake the child, and repaint.
#[allow(clippy::too_many_arguments)]
fn suspend_cycle<C, T, P, R, S>(
    clock: &mut C,
    renderer: &mut Renderer,
    resize: &mut ResizeCtl<C::Instant>,
    term: &mut T,
    pty_writer: &mut P,
    resizer: &R,
    suspender: &S,
) -> SuspendOutcome
where
    C: Clock<Msg = Msg>,
    T: OuterTerminal,
    P: Write,
    R: PtyResizer,
    S: Suspender,
{
    // Step 0 — resize-mode teardown (same as run's shutdown tail). `end_resize`
    // clears `resize_active`, or the cursor tail would stay suppressed after resume;
    // park/unpark re-mirror the cursor themselves from there.
    leave_resize_mode(resize, renderer, term);

    // Step 1 — pre-stop drain: bounded quiet-gap drain of the child's terminal-
    // restore bytes. Aborts to a normal shutdown if the child died right after
    // stopping (before any park), so the terminal is never parked/double-restored.
    let start = clock.now();
    let cap = clock.deadline(start, SUSPEND_DRAIN_CAP);
    loop {
        let now = clock.now();
        if now >= cap {
            break;
        }
        let gap = clock.deadline(now, SUSPEND_QUIET_GAP);
        let deadline = if gap < cap { gap } else { cap };
        match clock.recv_until(deadline) {
            Recv::Msg(m) => {
                if let Flow::Exit(code) = dispatch(m, renderer, pty_writer, resizer, term) {
                    return SuspendOutcome::ChildExited(code);
                }
                // Any other Flow (including a second Suspend) is ignored here.
            }
            Recv::Timeout | Recv::Disconnected => break,
        }
    }

    // Step 2 — flush the drained state. The child's alt→primary edge (if it left the
    // alt screen before stopping) fires here, keeping `outer_alt_active` truthful.
    let _ = render_once(renderer, term);

    // Step 3 — park (restore, ADR-010 order, minus the exit-status line).
    let _ = park(renderer, term);

    // Step 4 — stop gutter's own process group. THE WHOLE PROCESS STOPS HERE until
    // the shell `fg`s it; all five threads freeze at this call site.
    suspender.suspend_self();

    // Step 5 — unpark (resume, inverse order, raw mode FIRST).
    let _ = unpark(renderer, term);

    // Step 6 — inline anchor reseed (primary-screen children). The shell scrolled the
    // screen while gutter slept, so base_row is meaningless and CPR is unavailable (the
    // input thread owns the tty read fd). Reseed at the bottom like a fresh launch;
    // render_once's make-room scroll re-lays the band there.
    //
    // This runs BEFORE the step-7 catch-up (which normally settles base_row first) so
    // that handle_resize's interior clear reads a bottom-anchored base_row, not the
    // stale one: on the primary screen the pre-resize base_row still points into the
    // shell output the child left on screen, and clearing [stale_base_row, rows) would
    // wipe that history. Seed from the post-resize row count (terminal_size, the source
    // handle_resize itself resizes to) so the two agree.
    let outer_size = term.terminal_size();
    if !renderer.parser.screen().alternate_screen() {
        let seed_rows = match outer_size {
            Ok((_, rows_now)) => rows_now,
            Err(_) => renderer.parser.screen().size().0,
        };
        renderer.base_row = seed_rows.saturating_sub(1);
    }

    // Step 7 — missed-resize catch-up. The outer terminal may have resized while
    // gutter slept; run the full ADR-008 handler so the TIOCSWINSZ queues one
    // SIGWINCH on the still-stopped child, delivered when it is continued in step 8
    // — so the child wakes and repaints once at the right size, not twice.
    if let Ok((cols, rows_now)) = outer_size {
        let cur_rows = renderer.parser.screen().size().0;
        if cols != renderer.real_cols || rows_now != cur_rows {
            handle_resize(renderer, resizer, term, cols, rows_now);
        }
    }
    let rows = renderer.parser.screen().size().0;

    // Step 8 — wake the child, AFTER the outer terminal is fully re-set-up, so its
    // post-cont repaint bytes land on a raw-mode, correct-screen terminal.
    suspender.continue_child();

    // Step 9 — full repaint, then clear the gutter columns: the band's rows may carry
    // shell text from the suspension in the gutter strip. A gutters-only clear (not
    // repaint_margins) is deliberate: repaint_margins(.., false) also blanks the width
    // readout's in-band fallback span as a resize-mode "exit clear", which here — after
    // the render_once above already synced `prev` — would erase band cells with no
    // paired repaint to restore them (e.g. `--width full`, where the readout falls
    // inside the band). Step 0 already cleared any live overlay, so there is no readout
    // to clear anyway. The clear runs after render_once so it reads the base_row that
    // render_once's make-room scroll settled on. Flush explicitly: on resume the loop
    // blocks on recv(), so a child that emits nothing would otherwise leave these
    // gutter-cleanup bytes unflushed indefinitely.
    renderer.reset_prev_baseline();
    let _ = render_once(renderer, term);
    let offset = renderer.span_offset();
    let _ = term.clear_gutter(
        renderer.left_margin,
        renderer.width,
        renderer.real_cols,
        offset,
        rows,
    );
    let _ = term.flush();

    SuspendOutcome::Resumed
}

/// An ordered run of terminal steps where every one is attempted even after an earlier
/// one fails, so a failed alt-leave cannot short-circuit the raw-mode drop and strand
/// the shell in raw mode or mouse reporting. The first error is kept and returned.
#[derive(Default)]
struct BestEffort(Option<std::io::Error>);

impl BestEffort {
    fn step(&mut self, r: std::io::Result<()>) {
        if let Err(e) = r {
            self.0.get_or_insert(e);
        }
    }

    fn result(self) -> std::io::Result<()> {
        match self.0 {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

/// The explicit, ordered terminal restore (ADR-010), mode-aware (ADR-012/013):
/// conditional alt-leave / inline hand-back → drop any leftover attribute run → hand the
/// shell a default cursor shape → undo the mirrored input modes (ADR-022) → undo the
/// child's keyboard modes (ADR-021) → disable mouse → show the cursor → drop raw mode
/// LAST. Each step undoes only what was actually set up — the cursor shape is reset only
/// when a shape gutter wrote is on the terminal, so one the user set for their own shell
/// survives.
///
/// The discriminator is the live `outer_alt_active`: a child that exits in the alt screen
/// takes the leave-alt path; one that exits inline hands back below the band. The
/// hand-back is gated on `ever_painted_inline` alone, so a TUI that dropped back to the
/// primary screen without ever painting inline (even on a non-zero exit) leaves no stray
/// status line. The exit code is consulted only inside the hand-back.
///
/// Best-effort per step: the order is fixed, but a failing step never short-circuits the
/// ones after it, so raw mode comes off whatever else went wrong. The collector is handed
/// back rather than a result, so a caller can add its own steps to the same run.
fn ordered_restore<T: OuterTerminal>(
    renderer: &Renderer,
    term: &mut T,
    exit_code: i32,
) -> BestEffort {
    let mut restore = BestEffort::default();

    if renderer.outer_alt_active {
        restore.step(term.leave_alt_screen());
    } else if renderer.ever_painted_inline {
        restore.step(hand_back_inline(renderer, term, exit_code));
    }
    restore.step(term.write_row(SGR_RESET));
    if renderer.parser.callbacks().cursor_shape.is_mirrored() {
        restore.step(term.set_cursor_shape(DEFAULT_CURSOR_SHAPE));
    }
    restore.step(mode_reset(renderer, term));
    restore.step(relay_reset(renderer, term));
    restore.step(term.disable_mouse()); // conditional on mouse_enabled
    // Autowrap back on, unconditionally — like the mouse disable (ADR-005), gutter puts
    // the terminal in the state a shell expects rather than the one it found. Before
    // `show_cursor`, whose flush is what puts the whole restore on screen (ADR-023).
    restore.step(term.set_autowrap(true));
    restore.step(term.show_cursor());
    restore.step(term.disable_raw_mode()); // LAST (ADR-010)
    restore
}

/// Park the outer terminal (ADR-0019 step 3): the ordered restore, hand-back in its
/// exit-0 shape, then a flush so it all lands before the self-stop.
/// Deliberately does NOT clear `outer_alt_active`: it stays as "the child's screen
/// is alt" for the resume re-derivation (the double-meaning note in ADR-0019). The
/// relay's log survives for the same reason — this is a park, not a teardown, and
/// `unpark` replays it.
fn park<T: OuterTerminal>(renderer: &mut Renderer, term: &mut T) -> std::io::Result<()> {
    let mut restore = ordered_restore(renderer, term, 0);

    // Cleared, not kept: the child's modes are still on its screen, so the step-9
    // repaint's poll re-asserts them on resume with no replay list.
    renderer.mode_mirror.clear();
    renderer.cursor_visible = true;
    restore.step(term.flush()); // the park bytes must land before the self-stop
    restore.result()
}

/// Unpark the outer terminal (ADR-0019 step 5): re-take it after the self-stop
/// returns, in inverse order — raw mode FIRST, shrinking the cooked-mode window the
/// already-running input thread could read canonical input in. gutter asks the outer
/// terminal for no keyboard mode of its own (ADR-020); what it re-asserts is what the
/// child asked for, replayed from the relay's log (ADR-021).
/// Best-effort per step like [`park`]'s restore, and for the same reason turned around: a
/// failed raw-mode re-take must not skip the mouse re-enable or the alt re-entry, or the
/// renderer's own screen-mode state would go out of step with the terminal and teardown
/// would emit a `?1049l` for an alt screen the terminal never entered — restoring a buffer
/// that predates the run over what the user was looking at.
fn unpark<T: OuterTerminal>(renderer: &mut Renderer, term: &mut T) -> std::io::Result<()> {
    let mut restore = BestEffort::default();
    restore.step(retry_enable_raw(term));
    // Park turned autowrap back on for the shell; re-assert it off, or one Ctrl+Z/`fg`
    // would drop it for the rest of the run with nothing to detect the loss.
    restore.step(term.set_autowrap(false));
    restore.step(relay_replay(renderer, term));
    restore.step(term.enable_mouse());
    let child_alt = renderer.parser.screen().alternate_screen();
    if child_alt {
        restore.step(term.enter_alt_screen());
    }
    // Re-derive the outer alt state from the parser (the double meaning resolves
    // here — see ADR-0019).
    renderer.outer_alt_active = child_alt;
    // Reality after restore; render_once re-hides on the next repaint if needed.
    renderer.cursor_visible = true;
    // Re-assert the child's cursor shape on the next repaint: park reset the outer
    // cursor to the default, so the watcher's mirrored state is stale.
    renderer.parser.callbacks_mut().cursor_shape.rearm();
    restore.result()
}

/// Turn off the input modes gutter mirrored onto the outer terminal (ADR-022),
/// immediately before the relay's own reset so every input-encoding restore sits
/// together with the coarsest last.
///
/// Restores to the terminal's defaults rather than to whatever it had before gutter
/// launched. A shell whose paste protection is clobbered by this re-asserts its input
/// modes at its next prompt, so the blast radius is one prompt.
fn mode_reset<T: OuterTerminal>(renderer: &Renderer, term: &mut T) -> std::io::Result<()> {
    relay_if_any(term, &renderer.mode_mirror.reset_bytes())
}

/// Relay bytes to the outer terminal, skipping the call entirely when there are none —
/// `relay` flushes, so an empty write would cost a syscall for nothing.
fn relay_if_any<T: OuterTerminal>(term: &mut T, bytes: &[u8]) -> std::io::Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    term.relay(bytes)
}

/// Undo the keyboard modes the child asked the outer terminal for (ADR-021), in the
/// slot ADR-010 gives the input-encoding restores: after the screen is back to the
/// primary buffer, so the bytes land on the screen the shell inherits, and before raw
/// mode is dropped.
///
/// Writes nothing when the child negotiated nothing — the relay's log is the record of
/// what was actually set up, and gutter resets no mode it did not set.
fn relay_reset<T: OuterTerminal>(renderer: &Renderer, term: &mut T) -> std::io::Result<()> {
    relay_write(renderer, term, KeyModeRelay::reset_bytes)
}

/// Re-apply the child's keyboard modes on resume, oldest first, so the terminal comes
/// back at the depth and flags the child last asked for and a later pop still lines up.
fn relay_replay<T: OuterTerminal>(renderer: &Renderer, term: &mut T) -> std::io::Result<()> {
    relay_write(renderer, term, KeyModeRelay::replay_bytes)
}

fn relay_write<T: OuterTerminal>(
    renderer: &Renderer,
    term: &mut T,
    bytes: fn(&KeyModeRelay) -> Vec<u8>,
) -> std::io::Result<()> {
    let out = renderer
        .parser
        .callbacks()
        .key_modes()
        .map(bytes)
        .unwrap_or_default();
    relay_if_any(term, &out)
}

/// Re-enter raw mode on resume, retrying a bounded number of times on `EINTR`: the
/// `tcsetattr` inside `enable_raw_mode` can be interrupted by the SIGCONT that woke
/// gutter (ADR-0019, "Raw mode dropped last, re-taken first").
fn retry_enable_raw<T: OuterTerminal>(term: &mut T) -> std::io::Result<()> {
    for _ in 0..4 {
        match term.enable_raw_mode() {
            Ok(()) => return Ok(()),
            Err(e) if e.raw_os_error() == Some(libc::EINTR) => continue,
            Err(e) => return Err(e),
        }
    }
    term.enable_raw_mode()
}

/// On a child-exit shutdown, give the PTY path a bounded chance to deliver its final
/// bytes before teardown reads `outer_alt_active` (ADR-013, the teardown race).
///
/// The waiter and the PTY reader race with no ordering guarantee, so the child's last
/// frame — e.g. the `?1049h` of a TUI that exits the instant it enters the alt screen —
/// can still be queued behind the `ChildExited`. Without this drain `outer_alt_active`
/// would read false and teardown would take the inline hand-back instead of leaving the
/// alt screen.
///
/// So dispatch every remaining message until the `Msg::PtyEof` sentinel arrives (the
/// common case, guaranteed to follow every `Msg::Pty` by same-thread FIFO), or the
/// channel disconnects, or the grace cap elapses. The cap ([`TEARDOWN_DRAIN_GRACE`]) is
/// the backstop for a grandchild holding the slave open so the master never EOFs.
fn drain_pty_path<C, T, P, R>(
    clock: &mut C,
    renderer: &mut Renderer,
    pty_writer: &mut P,
    resizer: &R,
    term: &mut T,
) where
    C: Clock<Msg = Msg>,
    T: OuterTerminal,
    P: Write,
    R: PtyResizer,
{
    if renderer.pty_eof_seen {
        return;
    }
    let now = clock.now();
    let grace_deadline = clock.deadline(now, TEARDOWN_DRAIN_GRACE);
    loop {
        if renderer.pty_eof_seen || clock.now() >= grace_deadline {
            break;
        }
        match clock.recv_until(grace_deadline) {
            Recv::Msg(m) => {
                let _ = dispatch(m, renderer, pty_writer, resizer, term);
            }
            Recv::Timeout | Recv::Disconnected => break,
        }
    }
}

/// The exit path's restore: [`ordered_restore`] carrying the child's exit code, so the
/// inline hand-back can print the dim status line. No flush of its own — `show_cursor`
/// flushes, landing everything the restore queued ahead of it. The first error is
/// returned; every step ran regardless.
fn run_teardown<T: OuterTerminal>(
    renderer: &Renderer,
    term: &mut T,
    exit_code: i32,
) -> std::io::Result<()> {
    ordered_restore(renderer, term, exit_code).result()
}

/// The content rows a mock terminal was asked to write, with the restore's own
/// [`SGR_RESET`] dropped: every restore emits one, and no assertion here is about it.
#[cfg(test)]
fn content_rows(calls: &[crate::terminal::mock::Call]) -> Vec<&[u8]> {
    use crate::terminal::mock::Call;
    calls
        .iter()
        .filter_map(|c| match c {
            Call::WriteRow(b) if b != SGR_RESET => Some(b.as_slice()),
            _ => None,
        })
        .collect()
}

/// The inline hand-back (ADR-013): drop the cursor to a fresh line below the band's last
/// content, then — on a non-zero exit only — print the dim `Exited with: N` status line
/// there. The band is already painted in place, so there is no replay; this only
/// repositions the cursor so the shell's next prompt resumes below the output.
///
/// The band's last content sits at physical row `base_row + deepest_live_row` (clamped
/// to the bottom). A `newline()` from there lands a fresh line below it, scrolling the
/// terminal when the band already reaches the bottom; on a non-zero exit the dim status's
/// own leading `\r\n` does that line break instead. No explicit flush: the queued bytes
/// are drained by the `show_cursor` flush later in the restore.
fn hand_back_inline<T: OuterTerminal>(
    renderer: &Renderer,
    term: &mut T,
    exit_code: i32,
) -> std::io::Result<()> {
    let real_rows = renderer.parser.screen().size().0;
    let phys_bottom = renderer
        .base_row
        .saturating_add(renderer.deepest_live_row())
        .min(real_rows.saturating_sub(1));
    term.move_to(0, phys_bottom)?;
    if exit_code == 0 {
        term.newline()
    } else {
        // The leading reset is `newline`'s rule spelled out: this `\r\n` scrolls too
        // when the band already reaches the bottom.
        term.write_row(format!("\x1b[m\r\n\x1b[2mExited with: {exit_code}\x1b[0m").as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suspend::mock::MockSuspender;
    use crate::terminal::mock::{Call, MockTerminal};
    use portable_pty::ExitStatus;

    /// A no-op resizer for tests that never drive a resize event. (The recording
    /// resizer that asserts the ADR-008 ordering lives in the `resize` module.)
    struct NoopResizer;
    impl PtyResizer for NoopResizer {
        fn resize(&self, _cols: u16, _rows: u16) -> Result<(), String> {
            Ok(())
        }
    }

    /// A virtual clock + scripted message queue (ADR-007). Time only advances
    /// when the test scripts it; both `recv` and `recv_until` resolve against
    /// the script. Records how many times the loop polled the clock (`wakeups`)
    /// so the idle-park test can prove the single blocking-`recv` park.
    struct VirtualClock {
        /// (delay-before-this-message, message). The delay advances virtual
        /// time when the message is consumed, modelling inter-arrival gaps.
        script: std::collections::VecDeque<(u64, Msg)>,
        /// Virtual "now" in milliseconds.
        now_ms: u64,
        /// How many `now()` calls happened — a proxy for thread wakeups.
        pub wakeups: usize,
    }

    impl VirtualClock {
        fn new(script: Vec<(u64, Msg)>) -> Self {
            Self {
                script: script.into(),
                now_ms: 0,
                wakeups: 0,
            }
        }
    }

    impl Clock for VirtualClock {
        type Msg = Msg;
        type Instant = u64;

        fn now(&mut self) -> u64 {
            self.wakeups += 1;
            self.now_ms
        }

        fn deadline(&self, from: u64, dur: Duration) -> u64 {
            from + dur.as_millis() as u64
        }

        fn recv(&mut self) -> Option<Msg> {
            // Phase A blocking recv: take the next scripted message, advancing
            // virtual time by its inter-arrival delay. No messages → all senders
            // gone (the loop exits via the backstop without rendering).
            self.script.pop_front().map(|(delay, msg)| {
                self.now_ms += delay;
                msg
            })
        }

        fn recv_until(&mut self, deadline: u64) -> Recv<Msg> {
            match self.script.front() {
                Some(&(delay, _)) => {
                    let arrival = self.now_ms + delay;
                    if arrival <= deadline {
                        // Arrives within the frame: advance time, deliver it.
                        self.now_ms = arrival;
                        let (_, msg) = self.script.pop_front().unwrap();
                        Recv::Msg(msg)
                    } else {
                        // Next message is past the deadline: this frame times
                        // out at the deadline (idle-gap exit).
                        self.now_ms = deadline;
                        Recv::Timeout
                    }
                }
                // Nothing left to deliver: senders gone.
                None => Recv::Disconnected,
            }
        }
    }

    /// Build a left-aligned, fixed-`width` renderer (margin 0) — the absolute-width,
    /// left-aligned baseline.
    fn left_renderer(width: u16, rows: u16) -> Renderer {
        Renderer::new(
            width,
            rows,
            width, // real_cols == width → margin 0 for both Left and Center
            Layout::Left,
            Width::Cols(width),
            Box::new(std::io::sink()),
            0, // base_row 0 → absolute paint, the baseline
        )
    }

    /// A recording [`PtyResizer`] capturing each `master.resize(cols, rows)` in
    /// order.
    #[derive(Default)]
    struct RecResizer {
        calls: std::cell::RefCell<Vec<(u16, u16)>>,
    }
    impl PtyResizer for RecResizer {
        fn resize(&self, cols: u16, rows: u16) -> Result<(), String> {
            self.calls.borrow_mut().push((cols, rows));
            Ok(())
        }
    }

    /// Build a centred renderer with an explicit width config, for the tests that
    /// drive the width machinery.
    fn mode_renderer(width: u16, rows: u16, real_cols: u16, cfg: Width) -> Renderer {
        Renderer::new(
            width,
            rows,
            real_cols,
            Layout::Center,
            cfg,
            Box::new(std::io::sink()),
            0,
        )
    }

    /// The default chord's byte, the one gutter withholds from the child.
    const CHORD: &[u8] = &[0x1c];

    /// Everything one turn of the loop touches, bundled so tests read as a script of
    /// `send`/`enter` calls rather than repeating eight `&mut` arguments. Drives
    /// `apply_message` and `flush_hold` directly, which is the whole loop minus the
    /// frame timing.
    struct Ctx {
        clock: VirtualClock,
        renderer: Renderer,
        resize: ResizeCtl<u64>,
        input: InputCtl<u64>,
        term: MockTerminal,
        pty: Vec<u8>,
        resizer: RecResizer,
    }

    impl Ctx {
        fn new(width: u16, rows: u16, real_cols: u16, cfg: Width) -> Self {
            Self {
                clock: VirtualClock::new(vec![]),
                renderer: mode_renderer(width, rows, real_cols, cfg),
                resize: ResizeCtl::inactive(),
                input: InputCtl::new(),
                term: MockTerminal::new(),
                pty: Vec::new(),
                resizer: RecResizer::default(),
            }
        }

        fn send(&mut self, bytes: &[u8]) -> Flow {
            apply_message(
                Msg::Input(bytes.to_vec()),
                &mut self.clock,
                &mut self.renderer,
                &mut self.resize,
                &mut self.input,
                &mut self.term,
                &mut self.pty,
                &self.resizer,
            )
        }

        /// Release whatever the ESC-hold is withholding, as the loop's top-of-frame
        /// check does once the deadline passes.
        fn expire_hold(&mut self) {
            flush_hold(
                &mut self.input,
                &mut self.clock,
                &mut self.renderer,
                &mut self.resize,
                &mut self.term,
                &mut self.pty,
                &self.resizer,
            );
        }

        /// Advance virtual time, firing the hold only once its deadline has passed —
        /// the loop's own top-of-frame condition.
        fn advance(&mut self, ms: u64) {
            self.clock.now_ms += ms;
            if self.input.hold_deadline.is_some_and(|d| self.clock.now_ms >= d) {
                self.expire_hold();
            }
        }

        fn enter(&mut self) {
            self.send(CHORD);
        }
    }

    fn run_with(
        script: Vec<(u64, Msg)>,
        width: u16,
        rows: u16,
    ) -> (usize, MockTerminal, Vec<u8>, Renderer, Option<i32>) {
        let mut clock = VirtualClock::new(script);
        let mut renderer = left_renderer(width, rows);
        let mut term = MockTerminal::new();
        let mut pty: Vec<u8> = Vec::new();
        let resizer = NoopResizer;
        let suspender = MockSuspender::disconnected();
        let code = run(&mut clock, &mut renderer, &mut term, &mut pty, &resizer, &suspender);
        let flushes = term.calls.iter().filter(|c| **c == Call::Flush).count();
        (flushes, term, pty, renderer, code)
    }

    /// All applied bytes drive a fresh parser to the same grid the loop ended
    /// with — the "completeness" half of the cap test.
    fn grid_text(screen: &vt100::Screen, width: u16) -> Vec<String> {
        screen.rows(0, width).map(|r| r.trim_end().to_string()).collect()
    }

    /// 60fps cap + completeness (the CI gate). Feed M chunks across virtual T
    /// where each chunk arrives within the frame; assert the render count is
    /// `<= ceil(60·T) + 1` AND the final grid equals the all-bytes-applied grid.
    #[test]
    fn cap_and_completeness() {
        // 300 chunks, each 1ms apart → ~300ms of virtual time → cap ~18 frames.
        let payloads: Vec<Vec<u8>> =
            (0..300).map(|i| format!("{i} ").into_bytes()).collect();
        let mut script: Vec<(u64, Msg)> = payloads
            .iter()
            .map(|p| (1u64, Msg::Pty(p.clone())))
            .collect();
        script.push((1, Msg::ChildExited(ExitStatus::with_exit_code(0))));

        let total_ms: u64 = 301; // sum of delays
        let (flushes, _term, _pty, renderer, code) = run_with(script, 80, 24);

        let cap = ((60.0 * total_ms as f64 / 1000.0).ceil() as usize) + 1;
        assert!(
            flushes <= cap,
            "render count {flushes} must be <= ceil(60·T)+1 = {cap}"
        );

        // Completeness: replay every PTY byte into a fresh parser; same grid.
        let mut reference: vt100::Parser<GutterCallbacks> =
            vt100::Parser::new_with_callbacks(24, 80, 0, GutterCallbacks::baseline());
        for p in &payloads {
            reference.process(p);
        }
        assert_eq!(
            grid_text(renderer.screen(), 80),
            grid_text(reference.screen(), 80),
            "final grid must equal all-bytes-applied grid"
        );
        assert_eq!(code, Some(0));
    }

    /// Starvation regression: a continuous burst where a chunk lands inside
    /// every 16ms window for all of T. The naive `recv_timeout(16ms)` loop
    /// renders ZERO times here because Timeout never fires; the explicit
    /// `now >= deadline` break makes renders STILL fire ~60·T times.
    #[test]
    fn starvation_regression_renders_still_fire() {
        // A chunk every 4ms for ~320ms — always inside the 16ms window.
        let mut script: Vec<(u64, Msg)> = (0..80)
            .map(|i| (4u64, Msg::Pty(format!("{i} ").into_bytes())))
            .collect();
        script.push((4, Msg::ChildExited(ExitStatus::with_exit_code(0))));
        let total_ms: u64 = 80 * 4 + 4;

        let (flushes, ..) = run_with(script, 80, 24);

        let expected = (60.0 * total_ms as f64 / 1000.0).round() as usize;
        // Must be in the right order of magnitude — crucially NOT zero.
        assert!(flushes > 0, "renders must fire under a continuous burst, got 0");
        assert!(
            flushes >= expected / 2 && flushes <= expected * 2 + 2,
            "render count {flushes} should be ~{expected} (60·T) under a burst"
        );
    }

    /// Idle-park: no messages for virtual T → render fires ZERO times and the
    /// loop makes zero clock wakeups before exiting (proving the single blocking
    /// `recv()` park, not a 60Hz spin). With an empty script the first `recv()`
    /// returns `None` immediately and the loop exits via the backstop, having
    /// rendered nothing.
    #[test]
    fn idle_park_zero_renders_zero_wakeups() {
        let mut clock = VirtualClock::new(vec![]);
        let mut renderer = left_renderer(80, 24);
        let mut term = MockTerminal::new();
        let mut pty: Vec<u8> = Vec::new();

        let code = run(&mut clock, &mut renderer, &mut term, &mut pty, &NoopResizer, &MockSuspender::disconnected());

        let flushes = term.calls.iter().filter(|c| **c == Call::Flush).count();
        assert_eq!(flushes, 0, "an idle loop must render ZERO times");
        assert_eq!(clock.wakeups, 0, "an idle loop must not poll the clock (no spin)");
        assert_eq!(code, None, "no ChildExited → backstop exit");
    }

    /// Input-liveness-under-load: a continuous multi-MB PTY burst with one
    /// keypress injected mid-burst. The key bytes must reach the PTY-master mock
    /// within ONE frame of the keypress instant — NOT at burst end — AND the cap
    /// and completeness still hold.
    #[test]
    fn input_liveness_under_load() {
        // ~64KB chunk every 1ms for 200ms = a multi-MB burst; inject one key at
        // the 100ms mark (the 100th chunk).
        let chunk = vec![b'x'; 64 * 1024];
        let mut script: Vec<(u64, Msg)> = Vec::new();
        for i in 0..200 {
            if i == 100 {
                script.push((0, Msg::Input(b"\r".to_vec())));
            }
            script.push((1, Msg::Pty(chunk.clone())));
        }
        script.push((1, Msg::ChildExited(ExitStatus::with_exit_code(0))));
        let total_ms: u64 = 201;

        // Custom run so we can stop virtual time at the keypress instant: the
        // key is enqueued at now=100ms; assert the PTY writer saw it by 116ms.
        // The VirtualClock advances time as messages are consumed, so by the
        // time the loop has consumed the key it is within the same frame's
        // 16ms window — we assert the writer is non-empty immediately after the
        // run and that it landed before the burst's end is processed.
        let mut clock = VirtualClock::new(script);
        let mut renderer = left_renderer(80, 24);
        let mut term = MockTerminal::new();
        let mut pty: Vec<u8> = Vec::new();
        let code = run(&mut clock, &mut renderer, &mut term, &mut pty, &NoopResizer, &MockSuspender::disconnected());

        // Enter → \r reaches the PTY writer.
        assert_eq!(pty, b"\r", "the mid-burst keystroke must reach the PTY master");

        let flushes = term.calls.iter().filter(|c| **c == Call::Flush).count();
        let cap = ((60.0 * total_ms as f64 / 1000.0).ceil() as usize) + 1;
        assert!(flushes <= cap, "render count {flushes} must be <= cap {cap}");
        assert_eq!(code, Some(0));
    }

    /// Child-exit restore (ADR-010/012/013): a plain command that prints an inline line
    /// then exits non-zero (42), never entering the alt screen. The restore side effects
    /// fire in order, the loop exits, and the exit code matches `status.exit_code()`.
    /// `LeaveAltScreen` must not fire; the hand-back drops the cursor below the band and
    /// emits the dim `Exited with: 42` status line before the remaining restore steps.
    #[test]
    fn child_exit_restores_in_order() {
        let mut clock = VirtualClock::new(vec![
            (0u64, Msg::Pty(b"report".to_vec())),
            (1, Msg::ChildExited(ExitStatus::with_exit_code(42))),
        ]);
        let mut renderer = left_renderer(80, 24);
        let mut term = MockTerminal::new();
        // Model the eager startup mouse capture main.rs performs; `DisableMouse`
        // must then fire in teardown.
        term.enable_mouse().unwrap();
        let mut pty: Vec<u8> = Vec::new();

        let code = run(&mut clock, &mut renderer, &mut term, &mut pty, &NoopResizer, &MockSuspender::disconnected());

        assert_eq!(code, Some(42), "exit code must equal status.exit_code()");
        // EnableMouse fired exactly once at startup (acceptance criterion).
        assert_eq!(
            term.calls.iter().filter(|c| **c == Call::EnableMouse).count(),
            1,
            "EnableMouse fires once at startup"
        );
        // Conditional teardown: a never-alt command leaves no alt screen.
        assert_eq!(
            term.restore_calls(),
            vec![
                Call::WriteRow(SGR_RESET.to_vec()),
                Call::DisableMouse,
                Call::ShowCursor,
                Call::DisableRawMode,
            ],
            "restore order: NO LeaveAltScreen (the child never entered it)"
        );

        // The dim status line is recorded as a `write_row` carrying the
        // `Exited with: 42` bytes, emitted by the hand-back below the inline band.
        let status_rows: Vec<&[u8]> = term
            .calls
            .iter()
            .filter_map(|c| match c {
                Call::WriteRow(b) => Some(b.as_slice()),
                _ => None,
            })
            .collect();
        assert!(
            status_rows.contains(&b"\x1b[m\r\n\x1b[2mExited with: 42\x1b[0m".as_slice()),
            "non-zero exit hands back the dim status line, got {status_rows:?}"
        );

        // …and the status slots before the remaining restore steps: it comes after
        // the inline paint (during the loop) but before DisableMouse (teardown).
        let status = term
            .calls
            .iter()
            .position(|c| matches!(c, Call::WriteRow(b) if b.windows(12).any(|s| s == b"Exited with:")))
            .unwrap();
        let disable_mouse = term
            .calls
            .iter()
            .position(|c| *c == Call::DisableMouse)
            .unwrap();
        assert!(
            status < disable_mouse,
            "the hand-back status slots before the remaining restore steps"
        );
    }

    /// Child-exit restore for a TUI in the alt screen at exit (`?1049h` then exit).
    /// `LeaveAltScreen` fires in the ADR-010 slot and the inline hand-back is
    /// skipped, so teardown emits no hand-back rows and no status line, even on a
    /// zero exit (ADR-010/012).
    #[test]
    fn child_exit_leaves_alt_screen_then_restores_in_order() {
        let mut clock = VirtualClock::new(vec![
            // The child enters the alt screen (a TUI), then exits while still in it.
            (0u64, Msg::Pty(b"\x1b[?1049h".to_vec())),
            (1, Msg::ChildExited(ExitStatus::with_exit_code(0))),
        ]);
        let mut renderer = left_renderer(80, 24);
        let mut term = MockTerminal::new();
        // Model the eager startup mouse capture main.rs performs.
        term.enable_mouse().unwrap();
        let mut pty: Vec<u8> = Vec::new();

        let code = run(&mut clock, &mut renderer, &mut term, &mut pty, &NoopResizer, &MockSuspender::disconnected());

        assert_eq!(code, Some(0));
        assert_eq!(
            term.calls.iter().filter(|c| **c == Call::EnableMouse).count(),
            1,
            "EnableMouse fires once at startup"
        );
        // The child entered the alt screen exactly once (its `?1049h` edge).
        assert_eq!(
            term.calls.iter().filter(|c| **c == Call::EnterAltScreen).count(),
            1,
            "the outer alt screen is entered once, on the child's edge"
        );
        assert_eq!(
            term.restore_calls(),
            vec![
                Call::LeaveAltScreen,
                Call::WriteRow(SGR_RESET.to_vec()),
                Call::DisableMouse,
                Call::ShowCursor,
                Call::DisableRawMode,
            ],
            "an alt-screen TUI leaves the alt screen, then restores in the ADR-010 order"
        );

        // A TUI exits in alt → the leave-alt path runs, the hand-back is skipped:
        // no hand-back write_row and no status line on the primary screen.
        assert!(
            content_rows(&term.calls).is_empty(),
            "an alt-screen TUI replays nothing onto the primary screen"
        );
    }

    /// Teardown undoes the keyboard modes the child asked the outer terminal for
    /// (ADR-021), in the ADR-010 slot: after the screen is back to the primary
    /// buffer, so the bytes land on the screen the shell inherits, and before the
    /// mouse and raw mode come off. Without this the user's shell is left with kitty
    /// reporting active and Escape starts producing `CSI 27 u`.
    #[test]
    fn child_exit_resets_the_relayed_keyboard_modes() {
        let mut clock = VirtualClock::new(vec![
            // A TUI that enters the alt screen, pushes a kitty level and turns
            // modifyOtherKeys on, then exits with both still live.
            (0u64, Msg::Pty(b"\x1b[?1049h\x1b[>1u\x1b[>4;2m".to_vec())),
            (1, Msg::ChildExited(ExitStatus::with_exit_code(0))),
        ]);
        let mut renderer = left_renderer(80, 24);
        let mut term = MockTerminal::new();
        term.enable_mouse().unwrap();
        let mut pty: Vec<u8> = Vec::new();

        run(&mut clock, &mut renderer, &mut term, &mut pty, &NoopResizer, &MockSuspender::disconnected());

        assert_eq!(
            term.restore_calls(),
            vec![
                // The child's own request, carried out to the terminal while it ran.
                Call::Relay(b"\x1b[>1u\x1b[>4;2m".to_vec()),
                Call::LeaveAltScreen,
                Call::WriteRow(SGR_RESET.to_vec()),
                Call::Relay(b"\x1b[<1u\x1b[>4;0m".to_vec()),
                Call::DisableMouse,
                Call::ShowCursor,
                Call::DisableRawMode,
            ],
            "the mode reset slots between the alt-leave and the mouse disable"
        );
    }

    /// The other half of ADR-010's rule: a child that never negotiated a keyboard
    /// mode leaves the outer terminal's own keyboard settings alone. A terminal
    /// whose user configured modifyOtherKeys themselves must survive gutter.
    #[test]
    fn child_exit_relays_nothing_when_no_mode_was_requested() {
        let mut clock = VirtualClock::new(vec![
            (0u64, Msg::Pty(b"plain output".to_vec())),
            (1, Msg::ChildExited(ExitStatus::with_exit_code(0))),
        ]);
        let mut renderer = left_renderer(80, 24);
        let mut term = MockTerminal::new();
        term.enable_mouse().unwrap();
        let mut pty: Vec<u8> = Vec::new();

        run(&mut clock, &mut renderer, &mut term, &mut pty, &NoopResizer, &MockSuspender::disconnected());

        assert!(
            !term.calls.iter().any(|c| matches!(c, Call::Relay(_))),
            "no mode requested, no mode reset: {:?}",
            term.calls
        );
    }

    /// The teardown-race regression guard (ADR-013). The waiter's `ChildExited` lands
    /// first, with the child's final `?1049h` queued behind it and the forwarder's
    /// `PtyEof` behind that. The shutdown drain must process the queued alt-screen bytes
    /// before teardown reads `outer_alt_active`, so restore leaves the alt screen rather
    /// than taking the inline hand-back. Without the drain this dropped the final frame
    /// and emitted no `LeaveAltScreen`.
    #[test]
    fn child_exit_before_final_alt_frame_still_leaves_alt() {
        let mut clock = VirtualClock::new(vec![
            // The waiter's ChildExited lands first; the alt-screen bytes the child
            // wrote just before exiting are still queued behind it, then PtyEof.
            (0u64, Msg::ChildExited(ExitStatus::with_exit_code(7))),
            (0, Msg::Pty(b"\x1b[?1049h".to_vec())),
            (0, Msg::PtyEof),
        ]);
        let mut renderer = left_renderer(80, 24);
        let mut term = MockTerminal::new();
        term.enable_mouse().unwrap();
        let mut pty: Vec<u8> = Vec::new();

        let code = run(&mut clock, &mut renderer, &mut term, &mut pty, &NoopResizer, &MockSuspender::disconnected());

        assert_eq!(code, Some(7), "the child's exit code still propagates");
        assert!(
            renderer.pty_eof_seen,
            "the drain consumes the PtyEof sentinel"
        );
        assert!(
            term.restore_calls().contains(&Call::LeaveAltScreen),
            "the child's final alt-screen frame must be processed before teardown, \
             so restore leaves the alt screen, got {:?}",
            term.restore_calls()
        );
    }

    /// The grace cap (ADR-013) bounds the shutdown drain when no `PtyEof` ever arrives —
    /// the grandchild-holds-the-fd case, where the master never EOFs. A straggler `Pty`
    /// is scheduled far past [`TEARDOWN_DRAIN_GRACE`]; the drain must not wait for it. It
    /// caps out at the grace deadline, the late `?1049h` is never processed, and teardown
    /// still completes and returns the exit code.
    #[test]
    fn shutdown_drain_caps_at_grace_when_no_eof() {
        let grace_ms = TEARDOWN_DRAIN_GRACE.as_millis() as u64;
        let mut clock = VirtualClock::new(vec![
            (0u64, Msg::ChildExited(ExitStatus::with_exit_code(0))),
            // A straggler arriving well past the grace window: never reached.
            (grace_ms + 500, Msg::Pty(b"\x1b[?1049h".to_vec())),
        ]);
        let mut renderer = left_renderer(80, 24);
        let mut term = MockTerminal::new();
        let mut pty: Vec<u8> = Vec::new();

        let code = run(&mut clock, &mut renderer, &mut term, &mut pty, &NoopResizer, &MockSuspender::disconnected());

        assert_eq!(code, Some(0), "teardown completes and returns the exit code");
        assert!(
            !renderer.pty_eof_seen,
            "no PtyEof arrived — the cap, not the sentinel, ended the drain"
        );
        assert!(
            !term.restore_calls().contains(&Call::LeaveAltScreen),
            "the post-grace straggler is never processed, so there is no alt screen to leave"
        );
    }

    /// `PtyEof` before `ChildExited` (ADR-013): the PTY path finished cleanly, so by the
    /// time the waiter fires there are no stragglers. The shutdown drain reads the
    /// already-set `pty_eof_seen` and short-circuits, and teardown still leaves the alt
    /// screen the (already processed) `?1049h` put us in.
    #[test]
    fn pty_eof_before_child_exit_short_circuits_drain() {
        let mut clock = VirtualClock::new(vec![
            (0u64, Msg::Pty(b"\x1b[?1049h".to_vec())),
            (1, Msg::PtyEof),
            (1, Msg::ChildExited(ExitStatus::with_exit_code(0))),
        ]);
        let mut renderer = left_renderer(80, 24);
        let mut term = MockTerminal::new();
        let mut pty: Vec<u8> = Vec::new();

        let code = run(&mut clock, &mut renderer, &mut term, &mut pty, &NoopResizer, &MockSuspender::disconnected());

        assert_eq!(code, Some(0), "the child's exit code propagates");
        assert!(renderer.pty_eof_seen, "the early PtyEof was recorded");
        assert!(
            term.restore_calls().contains(&Call::LeaveAltScreen),
            "the alt screen entered before EOF is left at teardown, got {:?}",
            term.restore_calls()
        );
    }

    /// The inline hand-back (ADR-013), driven directly through `run_teardown` on a
    /// primary-mode renderer that painted inline content, so the contract is pinned
    /// independently of the loop. `exit_code = 1`: a `write_row` carrying the dim
    /// `Exited with: 1` bytes, no `LeaveAltScreen`, before the remaining restore steps.
    /// `exit_code = 0`: no status-line `write_row`, just a `Newline` below the band.
    #[test]
    fn run_teardown_replays_dim_status_on_primary_only_on_nonzero() {
        // Non-zero on a primary (never-alt) renderer that painted inline: the dim
        // line is handed back, with no alt-leave, before the remaining restore steps.
        let mut renderer = left_renderer(80, 24); // outer_alt_active == false
        renderer.ever_painted_inline = true; // it printed inline content this run
        let mut term = MockTerminal::new();
        run_teardown(&renderer, &mut term, 1).unwrap();

        assert!(
            !term.calls.contains(&Call::LeaveAltScreen),
            "a never-alt renderer must not leave an alt screen it never entered"
        );

        assert_eq!(
            content_rows(&term.calls),
            vec![b"\x1b[m\r\n\x1b[2mExited with: 1\x1b[0m".as_slice()],
            "exit_code = 1 hands back the dim status line via write_row"
        );

        let show_cursor = term
            .calls
            .iter()
            .position(|c| *c == Call::ShowCursor)
            .unwrap();
        assert_eq!(
            content_rows(&term.calls[..show_cursor]).len(),
            1,
            "status emission slots before the remaining restore steps"
        );

        // Zero exit: no status-line write_row at all — just a Newline below the band.
        let mut renderer0 = left_renderer(80, 24);
        renderer0.ever_painted_inline = true;
        let mut term0 = MockTerminal::new();
        run_teardown(&renderer0, &mut term0, 0).unwrap();
        assert!(
            content_rows(&term0.calls).is_empty(),
            "exit_code = 0 hands back no status line"
        );
        assert!(
            term0.calls.contains(&Call::Newline),
            "exit_code = 0 still drops the cursor to a fresh line below the band"
        );
    }

    /// Teardown is best-effort per step (ADR-010): a terminal that fails the alt-leave
    /// and the cursor show still gets every later step, raw mode above all — propagating
    /// the first error instead would hand the user's shell back in raw mode. The first
    /// failure is what surfaces.
    #[test]
    fn run_teardown_runs_every_step_after_a_failing_one() {
        let mut renderer = left_renderer(80, 24);
        renderer.outer_alt_active = true;
        let mut term = MockTerminal::new();
        term.enable_mouse().unwrap();
        term.fail_on(Call::LeaveAltScreen);
        term.fail_on(Call::ShowCursor);

        let err = run_teardown(&renderer, &mut term, 0).expect_err("the failure surfaces");

        assert_eq!(err.to_string(), "LeaveAltScreen failed");
        assert_eq!(
            term.restore_calls(),
            vec![
                Call::LeaveAltScreen,
                Call::WriteRow(SGR_RESET.to_vec()),
                Call::DisableMouse,
                Call::ShowCursor,
                Call::DisableRawMode,
            ],
            "a failing step leaves the ADR-010 order intact and the rest still runs"
        );
    }

    // --- Screen-mode mirror (ADR-012/013) ---

    /// Edge-trigger de-dupe (ADR-012). A child that enters the alt screen (`?1049h`) and
    /// later leaves it (`?1049l`) toggles the outer alt screen exactly once each — one
    /// `EnterAltScreen` then one `LeaveAltScreen`. Repaints between the edges must not
    /// re-toggle.
    #[test]
    fn alt_screen_toggles_once_per_child_edge() {
        let script = vec![
            // Enter alt, paint, then (a few frames later) leave alt, paint.
            (0u64, Msg::Pty(b"\x1b[?1049h\x1b[1;1Hin-alt".to_vec())),
            (20, Msg::Pty(b"more-alt".to_vec())),
            (20, Msg::Pty(b"\x1b[?1049lback".to_vec())),
            (20, Msg::ChildExited(ExitStatus::with_exit_code(0))),
        ];
        let (_f, term, _p, _r, _c) = run_with(script, 20, 5);

        let enters = term.calls.iter().filter(|c| **c == Call::EnterAltScreen).count();
        let leaves = term.calls.iter().filter(|c| **c == Call::LeaveAltScreen).count();
        assert_eq!(enters, 1, "exactly one EnterAltScreen on the child's ?1049h edge");
        assert_eq!(leaves, 1, "exactly one LeaveAltScreen on the child's ?1049l edge");

        // The enter is recorded before the leave (the edges fire in order).
        let enter_pos = term.calls.iter().position(|c| *c == Call::EnterAltScreen).unwrap();
        let leave_pos = term.calls.iter().position(|c| *c == Call::LeaveAltScreen).unwrap();
        assert!(enter_pos < leave_pos, "enter precedes leave");
    }

    /// A plain stream never enters the outer alt screen. A child that only prints to the
    /// primary screen emits zero `EnterAltScreen` — gutter mirrors the child's mode and
    /// never forces the alt screen.
    #[test]
    fn plain_stream_never_enters_alt_screen() {
        let script = vec![
            (0u64, Msg::Pty(b"line1\r\nline2\r\nline3".to_vec())),
            (20, Msg::ChildExited(ExitStatus::with_exit_code(0))),
        ];
        let (_f, term, _p, _r, _c) = run_with(script, 20, 5);

        assert_eq!(
            term.calls.iter().filter(|c| **c == Call::EnterAltScreen).count(),
            0,
            "a plain stream must never enter the outer alt screen"
        );
    }

    /// Conditional teardown (ADR-012/013). Three streams, driven through the full loop +
    /// teardown:
    /// - plain → no `LeaveAltScreen`; the band paints inline during the run, and the
    ///   hand-back adds the dim status line on the non-zero exit;
    /// - alt (still in the alt screen at exit) → `LeaveAltScreen`, no hand-back;
    /// - alt-then-`?1049l` (left the alt screen before exit, never painting inline) →
    ///   `ever_painted_inline` stays false, so the hand-back is suppressed.
    #[test]
    fn conditional_teardown_and_latch() {
        // --- Plain stream, non-zero exit: inline band + hand-back status, no leave. ---
        let (_f, term, _p, _r, _c) = run_with(
            vec![
                (0u64, Msg::Pty(b"hello\r\nworld".to_vec())),
                (20, Msg::ChildExited(ExitStatus::with_exit_code(5))),
            ],
            20,
            5,
        );
        assert!(
            !term.calls.contains(&Call::LeaveAltScreen),
            "plain stream: nothing to leave"
        );
        let writes: Vec<&[u8]> = term
            .calls
            .iter()
            .filter_map(|c| match c {
                Call::WriteRow(b) => Some(b.as_slice()),
                _ => None,
            })
            .collect();
        // The band rows are painted inline during the run AND the hand-back adds
        // the status line; assert both the content and the status are present.
        assert!(
            writes.iter().any(|w| w.windows(5).any(|s| s == b"hello")),
            "plain stream paints the band content inline, got {writes:?}"
        );
        assert!(
            writes.contains(&b"\x1b[m\r\n\x1b[2mExited with: 5\x1b[0m".as_slice()),
            "plain stream hands back the dim status line, got {writes:?}"
        );

        // --- Alt stream, still in the alt screen at exit: leave, no hand-back. ---
        let (_f, term, _p, _r, _c) = run_with(
            vec![
                (0u64, Msg::Pty(b"\x1b[?1049h\x1b[1;1Htui".to_vec())),
                (20, Msg::ChildExited(ExitStatus::with_exit_code(0))),
            ],
            20,
            5,
        );
        assert!(
            term.calls.contains(&Call::LeaveAltScreen),
            "alt stream: must leave the alt screen it entered"
        );
        // No hand-back: the only paint is the live alt frame. After the leave, no
        // write_row is emitted (the hand-back is skipped while in alt).
        let leave_idx = term.calls.iter().position(|c| *c == Call::LeaveAltScreen).unwrap();
        assert!(
            content_rows(&term.calls[leave_idx..]).is_empty(),
            "alt stream: no hand-back write_row after the alt-leave"
        );

        // --- Alt-then-?1049l, clean exit: never painted inline, hand-back suppressed. ---
        let (_f, term, _p, _r, _c) = run_with(
            vec![
                (0u64, Msg::Pty(b"\x1b[?1049h\x1b[1;1Htui".to_vec())),
                (20, Msg::Pty(b"\x1b[?1049l".to_vec())),
                (20, Msg::ChildExited(ExitStatus::with_exit_code(0))),
            ],
            20,
            5,
        );
        // The child left the alt screen before exit but never painted inline, so the
        // `ever_painted_inline` gate is false — no stray status line on the primary
        // (the genuine `gutter vim` clean-quit case).
        let teardown_writes: Vec<&[u8]> = term
            .calls
            .iter()
            .filter_map(|c| match c {
                Call::WriteRow(b) => Some(b.as_slice()),
                _ => None,
            })
            .collect();
        assert!(
            !teardown_writes.iter().any(|w| w.windows(12).any(|s| s == b"Exited with:")),
            "alt-then-primary clean exit: no inline paint, so the hand-back is suppressed, got {teardown_writes:?}"
        );

        // --- No inline output, non-zero exit: silent. The gate is ever_painted_inline,
        // never the exit code — a command that printed nothing (`gutter false`) has no
        // band to caption, so the status is suppressed. ---
        let (_f, term, _p, _r, _c) = run_with(
            vec![(0u64, Msg::ChildExited(ExitStatus::with_exit_code(3)))],
            20,
            5,
        );
        let teardown_writes: Vec<&[u8]> = term
            .calls
            .iter()
            .filter_map(|c| match c {
                Call::WriteRow(b) => Some(b.as_slice()),
                _ => None,
            })
            .collect();
        assert!(
            !teardown_writes.iter().any(|w| w.windows(12).any(|s| s == b"Exited with:")),
            "no inline output: the hand-back is suppressed regardless of exit code, got {teardown_writes:?}"
        );
    }

    /// alt → primary → non-zero exit stays silent (BUG[1] regression). A full-screen TUI
    /// that paints only on the alt screen, drops back to the primary screen (`?1049l`)
    /// without ever painting inline, then exits non-zero, must hand nothing back:
    /// `ever_painted_inline` is the sole gate, so no dim status line is stamped onto the
    /// restored shell.
    #[test]
    fn alt_then_primary_nonzero_exit_stays_silent() {
        let (_f, term, _p, _r, _c) = run_with(
            vec![
                (0u64, Msg::Pty(b"\x1b[?1049h\x1b[1;1Htui".to_vec())),
                (20, Msg::Pty(b"\x1b[?1049l".to_vec())),
                (20, Msg::ChildExited(ExitStatus::with_exit_code(3))),
            ],
            20,
            5,
        );
        let writes: Vec<&[u8]> = term
            .calls
            .iter()
            .filter_map(|c| if let Call::WriteRow(b) = c { Some(b.as_slice()) } else { None })
            .collect();
        assert!(
            !writes.iter().any(|w| w.windows(12).any(|s| s == b"Exited with:")),
            "alt→primary→exit 3 with no inline paint must not stamp a status line, got {writes:?}"
        );
    }

    /// The primary-branch cursor tail targets the live physical row (ADR-012). A plain
    /// stream leaves the cursor at the child's cursor row; the recorded `PlaceCursor` row
    /// must equal the grid cursor row, not an absolute row above the band. (For a
    /// one-screenful paint the live physical row is the grid cursor row; the scroll emit
    /// is what makes them diverge.)
    #[test]
    fn primary_cursor_targets_live_physical_row() {
        let script = vec![
            (0u64, Msg::Pty(b"alpha\r\nbeta\r\ngamma".to_vec())),
            (20, Msg::ChildExited(ExitStatus::with_exit_code(0))),
        ];
        let mut clock = VirtualClock::new(script);
        let mut renderer = left_renderer(20, 5);
        let mut term = MockTerminal::new();
        let mut pty: Vec<u8> = Vec::new();
        run(&mut clock, &mut renderer, &mut term, &mut pty, &NoopResizer, &MockSuspender::disconnected());

        // The child's cursor after "gamma" (no trailing newline) is on row 2.
        let (crow, _ccol) = renderer.screen().cursor_position();
        assert_eq!(crow, 2, "child cursor on the third line (row 2)");

        // The last PlaceCursor the primary paint recorded targets that live row,
        // never an absolute row above the band (e.g. row 0).
        let placed_row = term
            .calls
            .iter()
            .rev()
            .find_map(|c| if let Call::PlaceCursor(_, row) = c { Some(*row) } else { None })
            .expect("a primary paint placed the cursor");
        assert_eq!(
            placed_row, crow,
            "primary cursor tail targets the live physical row (= grid cursor row)"
        );
    }

    /// Uniform margin management (ADR-0017): resize clears the gutter across the band's
    /// row span on BOTH screen modes, but the span's start differs. On the primary screen
    /// the clear starts at `base_row`, never at 0, so real shell history above the inline
    /// band is untouched. On the alt screen — where gutter owns the whole viewport — the
    /// clear starts at 0. Pins the `offset` computation inside `repaint_margins`.
    #[test]
    fn resize_clears_band_row_span_preserving_history() {
        // --- Primary (plain) stream resized: clear starts at base_row (5), not 0. ---
        let script = vec![
            (0u64, Msg::Pty(b"primary content".to_vec())),
            // A resize arrives mid-run, BEFORE the child exits.
            (20, Msg::Resize),
            (20, Msg::ChildExited(ExitStatus::with_exit_code(0))),
        ];
        let mut clock = VirtualClock::new(script);
        let mut renderer = left_renderer(20, 24);
        // The scripted content sits on grid row 0, so make-room never scrolls and
        // base_row stays 5 through the resize (5 < 29, the height clamp is a no-op).
        renderer.base_row = 5;
        let mut term = MockTerminal::new();
        // The render thread reads the new size itself when SIGWINCH lands.
        term.set_terminal_size(100, 30);
        let mut pty: Vec<u8> = Vec::new();
        run(&mut clock, &mut renderer, &mut term, &mut pty, &NoopResizer, &MockSuspender::disconnected());

        let primary_row_start = term.calls.iter().find_map(|c| {
            if let Call::ClearGutter(_, _, _, row_start, _) = c { Some(*row_start) } else { None }
        });
        assert_eq!(
            primary_row_start,
            Some(5),
            "primary-mode resize must clear from base_row (5), not 0 — rows [0, 5) of \
             history must be untouched, calls = {:?}",
            term.calls
        );

        // --- Alt stream resized: the clear still fires, spanning from row 0. ---
        let script = vec![
            (0u64, Msg::Pty(b"\x1b[?1049h\x1b[1;1Htui".to_vec())),
            (20, Msg::Resize),
            (20, Msg::ChildExited(ExitStatus::with_exit_code(0))),
        ];
        let mut clock = VirtualClock::new(script);
        let mut renderer = left_renderer(20, 24);
        let mut term = MockTerminal::new();
        term.set_terminal_size(100, 30);
        let mut pty: Vec<u8> = Vec::new();
        run(&mut clock, &mut renderer, &mut term, &mut pty, &NoopResizer, &MockSuspender::disconnected());

        let alt_row_start = term.calls.iter().find_map(|c| {
            if let Call::ClearGutter(_, _, _, row_start, _) = c { Some(*row_start) } else { None }
        });
        assert_eq!(
            alt_row_start,
            Some(0),
            "alt-mode resize must clear from row 0 (gutter owns the whole viewport), \
             calls = {:?}",
            term.calls
        );
    }

    /// Passthrough fidelity through the render loop's dispatch arm: whatever byte
    /// forms the outer terminal sends reach the child unchanged. The two Enters are
    /// the reported case — under a keyboard mode that distinguishes them the child
    /// gets distinct bytes, because gutter forwards rather than re-encodes.
    #[test]
    fn dispatch_forwards_input_bytes_verbatim() {
        let script = vec![
            // The two modifyOtherKeys Enter forms, then a battery of navigation
            // and function keys.
            (0u64, Msg::Input(b"\r".to_vec())),
            (1, Msg::Input(b"\x1b[13;2u".to_vec())),
            (1, Msg::Input(b"\x1b[3~\x1b[H\x1b[5~\x1bOP\x1b[24~".to_vec())),
            (1, Msg::Input(b"\x1b[1;5C".to_vec())),
            (1, Msg::ChildExited(ExitStatus::with_exit_code(0))),
        ];
        let (_flushes, _term, pty, ..) = run_with(script, 80, 24);
        assert_eq!(
            pty,
            b"\r\x1b[13;2u\x1b[3~\x1b[H\x1b[5~\x1bOP\x1b[24~\x1b[1;5C",
            "every byte reaches the child, in order, unchanged"
        );
    }

    /// Alt+<char> reaches the child with its ESC prefix intact.
    #[test]
    fn dispatch_forwards_alt_char_with_its_esc_prefix() {
        let script = vec![
            (0u64, Msg::Input(b"\x1br".to_vec())),
            (1, Msg::ChildExited(ExitStatus::with_exit_code(0))),
        ];
        let (_flushes, _term, pty, ..) = run_with(script, 80, 24);
        assert_eq!(pty, b"\x1br");
    }

    /// Device queries through the render-loop dispatch arm: a `Msg::Pty` carrying the
    /// child's query reaches the PTY writer as the spec-correct reply, pinning the
    /// gutter→child wiring (drain → write_all → flush). The offline `callbacks.rs` suite
    /// stops at `drain_replies`, so without this the wiring is proven only by the slow
    /// PTY integration tests.
    #[test]
    fn dispatch_answers_device_queries_on_the_pty() {
        // CPR: position to (3,7) then query → reply in W-grid coords, no margin.
        let script = vec![
            (0u64, Msg::Pty(b"\x1b[3;7H\x1b[6n".to_vec())),
            (1, Msg::ChildExited(ExitStatus::with_exit_code(0))),
        ];
        let (_flushes, _term, pty, ..) = run_with(script, 40, 24);
        assert_eq!(pty, b"\x1b[3;7R", "CSI 6 n → CPR at the child's W-grid (3;7)");

        // DA1: the query that caused the exit stall → Primary Device Attributes.
        let script = vec![
            (0u64, Msg::Pty(b"\x1b[c".to_vec())),
            (1, Msg::ChildExited(ExitStatus::with_exit_code(0))),
        ];
        let (_flushes, _term, pty, ..) = run_with(script, 40, 24);
        assert_eq!(pty, b"\x1b[?1;2c", "CSI c → DA1 reply");
    }

    /// The relay through the same dispatch arm (ADR-021): the child's mode request
    /// goes **outward** to the terminal, and nothing goes back to the child. The two
    /// directions share the arm, so a wiring mistake would cross them — a mode
    /// request echoed onto the PTY would arrive at the child as keystrokes.
    #[test]
    fn dispatch_relays_mode_requests_outward_and_answers_nothing() {
        let script = vec![
            (0u64, Msg::Pty(b"\x1b[>1u".to_vec())),
            (1, Msg::ChildExited(ExitStatus::with_exit_code(0))),
        ];
        let (_flushes, term, pty, ..) = run_with(script, 40, 24);

        assert!(
            term.calls.contains(&Call::Relay(b"\x1b[>1u".to_vec())),
            "the child's kitty push reaches the outer terminal; calls were {:?}",
            term.calls
        );
        assert!(
            pty.is_empty(),
            "a mode request is a question for the terminal, not the child: {pty:?}"
        );
    }

    // --- Mouse forwarding through the render-loop dispatch arm (ADR-005) ---
    //
    // These exercise the full Thread-2 wiring: a `Msg::Pty` carrying the child's DECSET
    // negotiation, the per-cycle poll that refreshes the gate from the live screen, then
    // the raw SGR bytes gutter's own scanner extracts, translates and re-encodes onto
    // the PTY writer. They assert on the child-received bytes, never the grid.

    /// The SGR wire bits, so the scripts read as the reports a terminal sends.
    const MOTION: u16 = 32;
    const NO_BUTTON: u16 = 3;

    /// One SGR-1006 report on the wire, in 1-based physical coordinates.
    fn mouse_ev(button: u16, column: u16, row: u16, release: bool) -> Msg {
        let final_byte = if release { 'm' } else { 'M' };
        Msg::Input(
            format!("\x1b[<{button};{};{}{final_byte}", column + 1, row + 1).into_bytes(),
        )
    }

    fn mouse_down(column: u16, row: u16) -> Msg {
        mouse_ev(0, column, row, false)
    }

    fn mouse_up(column: u16, row: u16) -> Msg {
        mouse_ev(0, column, row, true)
    }

    fn mouse_drag(column: u16, row: u16) -> Msg {
        mouse_ev(MOTION, column, row, false)
    }

    fn mouse_moved(column: u16, row: u16) -> Msg {
        mouse_ev(NO_BUTTON | MOTION, column, row, false)
    }

    /// Run a mouse script against a renderer pinned at `left_margin`, returning the
    /// bytes the child received. The renderer is built at `width`/`margin` so the
    /// coordinate translation and the `[0, W)` discard are exercised at a real
    /// non-zero margin.
    fn run_mouse(margin: u16, width: u16, mut script: Vec<(u64, Msg)>) -> Vec<u8> {
        script.push((1, Msg::ChildExited(ExitStatus::with_exit_code(0))));
        let mut clock = VirtualClock::new(script);
        let mut renderer = Renderer::at_margin(width, 24, margin);
        let mut term = MockTerminal::new();
        let mut pty: Vec<u8> = Vec::new();
        run(&mut clock, &mut renderer, &mut term, &mut pty, &NoopResizer, &MockSuspender::disconnected());
        pty
    }

    /// First post-negotiation click is delivered and the margin is subtracted: the child
    /// enables SGR mouse (`CSI ?1000h ?1006h`), then a click at physical col `margin + 5`
    /// row 3 → the child receives `CSI < 0 ; 6 ; 4 M` (child col 5 → SGR 6, row 3 → SGR
    /// 4). Eager capture means click #1 is the one asserted.
    #[test]
    fn mouse_first_click_delivered_margin_subtracted() {
        let margin = 30;
        let pty = run_mouse(
            margin,
            40,
            vec![
                // Child negotiates SGR press/release mouse.
                (0u64, Msg::Pty(b"\x1b[?1000h\x1b[?1006h".to_vec())),
                // The very first click after negotiation.
                (
                    1,
                    mouse_down(margin + 5, 3),
                ),
            ],
        );
        assert_eq!(
            pty, b"\x1b[<0;6;4M",
            "first click delivered, margin subtracted (child col 5 → SGR 6)"
        );
    }

    /// Gutter click ignored: with SGR mouse negotiated, a click left of the band
    /// (`col < margin`) and one beyond it (`>= margin + W`) both deliver nothing.
    #[test]
    fn mouse_gutter_clicks_discarded() {
        let margin = 30;
        let w = 40;
        let pty = run_mouse(
            margin,
            w,
            vec![
                (0u64, Msg::Pty(b"\x1b[?1000h\x1b[?1006h".to_vec())),
                // Left gutter (col 5 < margin 30).
                (1, mouse_down(5, 0)),
                // Right gutter (col margin+w = 70, >= band end).
                (
                    1,
                    mouse_down(margin + w, 0),
                ),
            ],
        );
        assert!(pty.is_empty(), "gutter clicks deliver nothing, got {pty:?}");
    }

    /// Down-filter, PressRelease (mode 1000): a drag (press → motion → release)
    /// delivers the press and the release but NOT the motion event.
    #[test]
    fn mouse_pressrelease_drops_drag_motion() {
        let margin = 10;
        let pty = run_mouse(
            margin,
            40,
            vec![
                (0u64, Msg::Pty(b"\x1b[?1000h\x1b[?1006h".to_vec())),
                (
                    1,
                    mouse_down(margin + 1, 0),
                ),
                // Motion mid-drag — must be dropped in PressRelease.
                (
                    1,
                    mouse_drag(margin + 2, 0),
                ),
                (
                    1,
                    mouse_up(margin + 2, 0),
                ),
            ],
        );
        // Press at child col 1 (SGR 2) M, release at child col 2 (SGR 3) m — no
        // motion event between them.
        assert_eq!(
            pty, b"\x1b[<0;2;1M\x1b[<0;3;1m",
            "PressRelease forwards press + release, drops the drag motion"
        );
    }

    /// Down-filter, ButtonMotion (mode 1002): motion while a button is held is
    /// delivered; motion with no button held is dropped. Driven by the gate's
    /// tracked press/release state.
    #[test]
    fn mouse_buttonmotion_gates_on_button_held() {
        let margin = 10;
        let pty = run_mouse(
            margin,
            40,
            vec![
                (0u64, Msg::Pty(b"\x1b[?1002h\x1b[?1006h".to_vec())),
                // Motion before any press → no button held → dropped.
                (1, mouse_moved(margin + 1, 0)),
                // Press → held.
                (
                    1,
                    mouse_down(margin + 1, 0),
                ),
                // Drag (motion with button) → delivered.
                (
                    1,
                    mouse_drag(margin + 2, 0),
                ),
                // Release → not held.
                (
                    1,
                    mouse_up(margin + 2, 0),
                ),
                // Motion after release → dropped.
                (1, mouse_moved(margin + 3, 0)),
            ],
        );
        // Delivered: press (col1→2, button 0, M), drag (col2→3, button 0|32=32, M),
        // release (col2→3, m). The two `Moved` events are dropped.
        assert_eq!(
            pty, b"\x1b[<0;2;1M\x1b[<32;3;1M\x1b[<0;3;1m",
            "ButtonMotion delivers motion only while held"
        );
    }

    /// When the child negotiates a reporting mode but not SGR (`CSI ?1000h` with no
    /// `?1006h` → Default encoding), a click must not produce a malformed SGR event —
    /// and must not take the session down either. `?1000h` alone is what plenty of
    /// older TUIs ask for, and the outer terminal reports in SGR regardless (the eager
    /// capture), so the report is dropped and the run carries on: a panic here would
    /// kill the render thread with raw mode and mouse reporting still on.
    #[test]
    fn mouse_non_sgr_encoding_drops_the_report_rather_than_forwarding_garbage() {
        // No `?1006h`, so the encoding stays Default while the mode is reporting.
        let pty = run_mouse(
            10,
            40,
            vec![
                (0u64, Msg::Pty(b"\x1b[?1000h".to_vec())),
                (1, mouse_down(15, 0)),
                (1, mouse_up(15, 0)),
            ],
        );
        assert!(
            pty.is_empty(),
            "an unsupported encoding forwards nothing, got {:?}",
            String::from_utf8_lossy(&pty)
        );
    }

    /// The forwarding gate is driven by the per-cycle poll of the live screen
    /// modes: with mouse never negotiated (mode stays `None`), a click is
    /// swallowed and the child receives nothing — the swallow side of the
    /// poll-driven gate, proven through the real dispatch path.
    #[test]
    fn mouse_swallowed_when_child_never_negotiated() {
        let pty = run_mouse(
            10,
            40,
            vec![(1, mouse_down(15, 0))],
        );
        assert!(
            pty.is_empty(),
            "no negotiation → mode None → swallow, got {pty:?}"
        );
    }

    /// The offset repaint paints each row at the left margin and repositions the real
    /// cursor inside the band. With `left_margin == 0` content starts at physical column
    /// 0 and the cursor lands at `(col, row)`.
    #[test]
    fn offset_repaint_paints_at_margin_and_tracks_cursor() {
        // Two lines, then the child exits.
        let script = vec![
            (0u64, Msg::Pty(b"hello\r\nworld".to_vec())),
            (1, Msg::ChildExited(ExitStatus::with_exit_code(0))),
        ];
        let (_flushes, term, _pty, _r, _code) = run_with(script, 20, 5);

        // The first painted row is preceded by MoveTo(left_margin=0, row=0).
        let moves: Vec<&Call> = term
            .calls
            .iter()
            .filter(|c| matches!(c, Call::MoveTo(..)))
            .collect();
        assert_eq!(moves.first(), Some(&&Call::MoveTo(0, 0)), "row 0 painted at margin 0");

        // The cursor lands after "world" → col 5, row 1.
        let place = term
            .calls
            .iter()
            .rev()
            .find_map(|c| if let Call::PlaceCursor(col, row) = c { Some((*col, *row)) } else { None });
        assert_eq!(place, Some((5, 1)), "cursor tracks the child to (col 5, row 1)");
    }

    /// A non-zero margin shifts every painted row and the cursor by the margin,
    /// and content never starts before the margin — the band-offset invariant.
    #[test]
    fn nonzero_margin_shifts_rows_and_cursor() {
        let mut clock = VirtualClock::new(vec![
            (0u64, Msg::Pty(b"hi".to_vec())),
            (1, Msg::ChildExited(ExitStatus::with_exit_code(0))),
        ]);
        let mut renderer = Renderer::at_margin(20, 5, 7); // margin 7
        let mut term = MockTerminal::new();
        let mut pty: Vec<u8> = Vec::new();
        run(&mut clock, &mut renderer, &mut term, &mut pty, &NoopResizer, &MockSuspender::disconnected());

        let first_move = term.calls.iter().find_map(|c| {
            if let Call::MoveTo(col, row) = c { Some((*col, *row)) } else { None }
        });
        assert_eq!(first_move, Some((7, 0)), "row painted at margin 7");

        let place = term.calls.iter().rev().find_map(|c| {
            if let Call::PlaceCursor(col, row) = c { Some((*col, *row)) } else { None }
        });
        assert_eq!(place, Some((7 + 2, 0)), "cursor at margin + col");
    }

    /// Cursor-visibility golden-master against the Claude Code fixture. Replay the
    /// checked-in fixture through the render loop at a non-zero margin, snapshot the
    /// cursor state, and assert directly that at settle the outer cursor equals
    /// `(left_margin + col, row)` and that the fixture's `CSI ?25l` hid the outer cursor.
    #[test]
    fn cursor_state_golden_master_over_fixture() {
        let fixture: &[u8] = include_bytes!("../tests/fixtures/claude-code-flow.cast");
        let (w, rows, margin) = (80u16, 24u16, 10u16);

        let mut renderer = Renderer::at_margin(w, rows, margin);
        let mut term = MockTerminal::new();
        // Replay the whole fixture, then render the settled frame once.
        renderer.parser.process(fixture);
        render_once(&mut renderer, &mut term).unwrap();

        // Sample the cursor state for the golden master: visibility + the outer
        // PlaceCursor the frame emitted.
        let placed = term.calls.iter().rev().find_map(|c| {
            if let Call::PlaceCursor(col, row) = c {
                Some((*col, *row))
            } else {
                None
            }
        });
        let visible = term
            .calls
            .iter()
            .filter_map(|c| if let Call::SetCursorVisible(v) = c { Some(*v) } else { None })
            .next_back();
        insta::assert_debug_snapshot!((visible, placed));

        // Direct assertion: outer cursor == (left_margin + col, row) at settle.
        let (crow, ccol) = renderer.parser.screen().cursor_position();
        assert_eq!(
            placed,
            Some((margin + ccol, crow)),
            "outer cursor must sit at (left_margin + col, row)"
        );

        // The fixture hides the cursor (CSI ?25l in phase 1); the outer terminal
        // must have been told to hide it.
        assert_eq!(
            visible,
            Some(false),
            "the fixture's CSI ?25l must hide the outer cursor"
        );
    }

    /// Resize mode enter hides the outer cursor once, and while active the
    /// per-frame cursor tail is suppressed: a frame that would normally mirror
    /// the child cursor emits neither a `PlaceCursor` nor a visibility change.
    #[test]
    fn resize_active_suppresses_cursor_mirror() {
        let mut r = Renderer::at_margin(20, 5, 7);
        let mut term = MockTerminal::new();

        r.begin_resize(&mut term).unwrap();
        assert!(
            term.calls.contains(&Call::SetCursorVisible(false)),
            "begin_resize must hide the outer cursor once, calls = {:?}",
            term.calls
        );

        // Give the grid a live cursor position, then drive a frame while active.
        r.parser.process(b"hi");
        term.calls.clear();
        render_once(&mut r, &mut term).unwrap();

        assert!(
            !term.calls.iter().any(|c| matches!(c, Call::PlaceCursor(..))),
            "no cursor placement while resize is active, calls = {:?}",
            term.calls
        );
        assert!(
            !term.calls.iter().any(|c| matches!(c, Call::SetCursorVisible(_))),
            "no cursor-visibility change while resize is active, calls = {:?}",
            term.calls
        );
    }

    /// Leaving resize mode re-mirrors the child cursor on the next frame: the
    /// position is re-placed, and a child whose cursor is visible is re-shown.
    #[test]
    fn resize_exit_remirrors_visible_child_cursor() {
        let mut r = Renderer::at_margin(20, 5, 7);
        let mut term = MockTerminal::new();

        r.parser.process(b"visible");
        r.begin_resize(&mut term).unwrap();
        r.end_resize();
        term.calls.clear();
        render_once(&mut r, &mut term).unwrap();

        assert!(
            term.calls.iter().any(|c| matches!(c, Call::PlaceCursor(..))),
            "exit must re-place the cursor, calls = {:?}",
            term.calls
        );
        assert!(
            term.calls.contains(&Call::SetCursorVisible(true)),
            "a visible child cursor must be re-shown on exit, calls = {:?}",
            term.calls
        );
    }

    /// Leaving resize mode re-mirrors the child's REAL state, not a force-show:
    /// if the child hid its cursor (DECTCEM off) the exit frame re-places it but
    /// never emits a show, so it stays hidden.
    #[test]
    fn resize_exit_keeps_hidden_child_cursor_hidden() {
        let mut r = Renderer::at_margin(20, 5, 7);
        let mut term = MockTerminal::new();

        r.parser.process(b"\x1b[?25lhi");
        r.begin_resize(&mut term).unwrap();
        r.end_resize();
        term.calls.clear();
        render_once(&mut r, &mut term).unwrap();

        assert!(
            !term.calls.contains(&Call::SetCursorVisible(true)),
            "a hidden child cursor must not be re-shown on exit, calls = {:?}",
            term.calls
        );
        assert!(
            term.calls.iter().any(|c| matches!(c, Call::PlaceCursor(..))),
            "exit must still re-place the cursor, calls = {:?}",
            term.calls
        );
    }

    /// insta golden-master: snapshot the virtual grid for a representative
    /// fixture stream (startup banner + a multi-line edit with colour), so
    /// render regressions surface as a snapshot diff. Drives the parser
    /// directly via the loop; no PTY.
    #[test]
    fn grid_snapshot_for_fixture_stream() {
        // Startup: clear + home, a title line, then a small "edit" that moves
        // the cursor around and applies SGR (bold + colour) — exercising
        // absolute positioning, wrapping inside W, and attribute runs.
        let fixture: &[u8] = b"\x1b[2J\x1b[H\
\x1b[1mgutter demo\x1b[0m\r\n\
line one\r\n\
line two\r\n\
\x1b[1;1H\x1b[32mGUTTER\x1b[0m\
\x1b[3;5Hedited";
        let script = vec![
            (0u64, Msg::Pty(fixture.to_vec())),
            (1, Msg::ChildExited(ExitStatus::with_exit_code(0))),
        ];
        let (_f, _t, _p, renderer, _c) = run_with(script, 20, 6);

        let grid: Vec<String> = renderer
            .screen()
            .rows(0, 20)
            .map(|r| r.trim_end().to_string())
            .collect();
        insta::assert_debug_snapshot!(grid);
    }

    /// No bleed past the band: with content that fills the full width, every painted
    /// row's bytes stay within `[0, W)` and nothing carries the cursor to column `W`
    /// (vt100's margin rule, ADR-006). The recorded `MoveTo` columns and cursor placement
    /// never reach `margin + W`.
    #[test]
    fn ascii_content_never_bleeds_past_band() {
        let width = 10u16;
        // A line longer than W: vt100 wraps it inside the grid, never past W.
        let script = vec![
            (0u64, Msg::Pty(b"ABCDEFGHIJKLMNOP".to_vec())),
            (1, Msg::ChildExited(ExitStatus::with_exit_code(0))),
        ];
        let mut clock = VirtualClock::new(script);
        let mut renderer = left_renderer(width, 5);
        let mut term = MockTerminal::new();
        let mut pty: Vec<u8> = Vec::new();
        run(&mut clock, &mut renderer, &mut term, &mut pty, &NoopResizer, &MockSuspender::disconnected());

        for call in &term.calls {
            match call {
                Call::MoveTo(col, _) => {
                    assert!(*col < width, "row start {col} must be < W {width}");
                }
                Call::PlaceCursor(col, _) => {
                    assert!(*col <= width, "cursor col {col} must be <= W {width}");
                }
                _ => {}
            }
        }
        // The grid itself is exactly W columns; no row exceeds it.
        for row in renderer.screen().rows(0, width) {
            assert!(row.chars().count() <= width as usize);
        }
    }

    /// DECTCEM mirroring: when the child hides the cursor the outer terminal is
    /// told to hide it; when shown again, shown. Only emitted on a change.
    #[test]
    fn cursor_visibility_mirrored() {
        let script = vec![
            // Hide cursor (CSI ?25l), some text, then show it (CSI ?25h).
            (0u64, Msg::Pty(b"\x1b[?25lhidden".to_vec())),
            (20, Msg::Pty(b"\x1b[?25hshown".to_vec())),
            (20, Msg::ChildExited(ExitStatus::with_exit_code(0))),
        ];
        let (_f, term, _p, _r, _c) = run_with(script, 20, 5);

        let vis: Vec<bool> = term
            .calls
            .iter()
            .filter_map(|c| if let Call::SetCursorVisible(v) = c { Some(*v) } else { None })
            .collect();
        assert_eq!(vis, vec![false, true], "hide then show, mirrored once each");
    }

    /// Cursor-shape mirroring through the dispatch path. A child-emitted `DECSCUSR`
    /// (`CSI 6 SP q`, steady bar) reaches the outer terminal as the matching `CSI 6 SP q`,
    /// mirrored exactly once on the change.
    #[test]
    fn cursor_shape_mirrored_on_outer() {
        let script = vec![
            // Child sets a steady-bar cursor, then a steady-underline cursor.
            (0u64, Msg::Pty(b"\x1b[6 q".to_vec())),
            (20, Msg::Pty(b"\x1b[4 q".to_vec())),
            (20, Msg::ChildExited(ExitStatus::with_exit_code(0))),
        ];
        let (_f, term, _p, _r, _c) = run_with(script, 20, 5);

        let shapes: Vec<Vec<u8>> = term
            .calls
            .iter()
            .filter_map(|c| {
                if let Call::SetCursorShape(b) = c {
                    Some(b.clone())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            shapes,
            vec![b"\x1b[6 q".to_vec(), b"\x1b[4 q".to_vec(), DEFAULT_CURSOR_SHAPE.to_vec()],
            "each DECSCUSR change mirrored once, verbatim, then teardown's default"
        );
    }

    /// Teardown resets the cursor only when gutter wrote a shape, which is not the same
    /// as the child having asked for one: a request made while the resize overlay owns
    /// the cursor never reaches the terminal. Resetting on the request would replace the
    /// shape the user configured for their own shell with the default.
    #[test]
    fn a_shape_gutter_never_wrote_is_not_reset_at_teardown() {
        let mut renderer = left_renderer(20, 5);
        // The overlay owns the cursor, so the frame's `mirror_cursor` returns early and
        // the request is recorded without ever being emitted.
        renderer.resize_active = true;
        renderer.parser.process(b"\x1b[5 q");
        let mut term = MockTerminal::new();

        run_teardown(&renderer, &mut term, 0).expect("teardown succeeds");

        assert!(
            !term.calls.iter().any(|c| matches!(c, Call::SetCursorShape(_))),
            "no shape was written, so none is reset: {:?}",
            term.calls
        );
    }

    /// Absorbed-mode mirroring (ADR-022) through the real frame. `render_once` takes
    /// an injected terminal, so every one of these is a parser feed, a frame, and a
    /// read of what the outer terminal saw — no PTY, no threads, no clock.
    mod mode_mirror {
        use super::*;
        use crate::terminal::mock::RecordingGrid;

        /// Feed the child's bytes, run one frame, and return the bytes the frame
        /// relayed to the outer terminal.
        fn frame(renderer: &mut Renderer, term: &mut MockTerminal, child: &[u8]) -> Vec<u8> {
            renderer.parser.process(child);
            let before = term.calls.len();
            render_once(renderer, term).unwrap();
            term.calls[before..]
                .iter()
                .filter_map(|c| match c {
                    Call::Relay(b) => Some(b.clone()),
                    _ => None,
                })
                .flatten()
                .collect()
        }

        /// Each mode reaches the outer terminal once on the edge that set it, once on
        /// the edge that cleared it, and never on a repeat of either.
        #[test]
        fn each_edge_emits_once_and_a_repeat_emits_nothing() {
            for (set, on, clear, off) in [
                (&b"\x1b[?1h"[..], &b"\x1b[?1h"[..], &b"\x1b[?1l"[..], &b"\x1b[?1l"[..]),
                (b"\x1b=", b"\x1b=", b"\x1b>", b"\x1b>"),
                (b"\x1b[?2004h", b"\x1b[?2004h", b"\x1b[?2004l", b"\x1b[?2004l"),
            ] {
                let mut renderer = left_renderer(20, 5);
                let mut term = MockTerminal::new();

                assert_eq!(frame(&mut renderer, &mut term, set), on);
                assert!(frame(&mut renderer, &mut term, set).is_empty());
                assert_eq!(frame(&mut renderer, &mut term, clear), off);
                assert!(frame(&mut renderer, &mut term, clear).is_empty());
            }
        }

        /// All three in one frame, in one relay call, in the pinned order.
        #[test]
        fn all_three_land_in_one_frame() {
            let mut renderer = left_renderer(20, 5);
            let mut term = MockTerminal::new();
            assert_eq!(
                frame(&mut renderer, &mut term, b"\x1b[?2004h\x1b=\x1b[?1h"),
                b"\x1b[?1h\x1b=\x1b[?2004h"
            );
        }

        /// `ESC c` resets vt100's screen wholesale, so the next frame's poll finds all
        /// three off with no RIS handling anywhere in gutter.
        #[test]
        fn ris_turns_all_three_off_on_the_next_frame() {
            let mut renderer = left_renderer(20, 5);
            let mut term = MockTerminal::new();
            frame(&mut renderer, &mut term, b"\x1b[?1h\x1b=\x1b[?2004h");
            assert_eq!(
                frame(&mut renderer, &mut term, b"\x1bc"),
                b"\x1b[?1l\x1b>\x1b[?2004l"
            );
        }

        /// **The one that stops someone reaching for `input_mode_diff`.** A mouse mode
        /// on the outer terminal would be a second authority over state ADR-005 owns,
        /// and a child that disabled reporting would turn gutter's own capture off
        /// underneath it.
        #[test]
        fn no_mouse_mode_ever_reaches_the_outer_terminal() {
            let mut renderer = left_renderer(20, 5);
            let mut term = MockTerminal::new();
            assert!(frame(
                &mut renderer,
                &mut term,
                b"\x1b[?9h\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h\x1b[?1005h"
            )
            .is_empty());
        }

        /// There is no relay path for private modes at all, so the assertion takes the
        /// strongest available form: no DECSET is ever forwarded, whether vt100
        /// implements it or drops it through `unhandled_csi`.
        #[test]
        fn no_decset_is_relayed() {
            let mut renderer = left_renderer(20, 5);
            let mut term = MockTerminal::new();
            assert!(frame(
                &mut renderer,
                &mut term,
                b"\x1b[?1047h\x1b[?1048h\x1b[?2048h\x1b[?1004h\x1b[?66h\x1b[?7727h\x1b[?2026h"
            )
            .is_empty());
        }

        /// The alt screen keeps its own mirror (ADR-012) and gains nothing from this one.
        #[test]
        fn alt_screen_stays_on_its_own_mirror() {
            let mut renderer = left_renderer(20, 5);
            let mut term = MockTerminal::new();
            assert!(frame(&mut renderer, &mut term, b"\x1b[?1049h").is_empty());
            assert!(
                term.calls.contains(&Call::EnterAltScreen),
                "the alt screen still mirrors: {:?}",
                term.calls
            );
        }

        /// The mirror must not be diffed against the `prev` parser. `sync_prev` replays
        /// `contents_formatted`, which excludes the input modes, so `prev`'s flags sit
        /// at their defaults for ever — a diff against it would re-emit every frame,
        /// which is exactly what an idle frame here proves it does not.
        #[test]
        fn the_diff_baseline_is_not_prev() {
            let mut renderer = left_renderer(20, 5);
            let mut term = MockTerminal::new();
            frame(&mut renderer, &mut term, b"\x1b[?1h\x1b=\x1b[?2004h");
            // `render_once` ends in `sync_prev`, so `prev` is already the trap's state.
            assert!(!renderer.prev.screen().application_cursor());
            for _ in 0..3 {
                assert!(frame(&mut renderer, &mut term, b"").is_empty());
            }
        }

        /// What the outer terminal ends up in, rather than what gutter spelled: the
        /// recording grid parses the relayed bytes the way a real terminal would.
        #[test]
        fn the_outer_terminal_ends_up_in_the_childs_modes() {
            let mut renderer = Renderer::at_margin(20, 5, 0);
            let mut grid = RecordingGrid::new(20, 5);
            renderer.parser.process(b"\x1b[?1h\x1b=\x1b[?2004h");
            render_once(&mut renderer, &mut grid).unwrap();
            assert_eq!(grid.outer_input_modes(), (true, true, true));

            renderer.parser.process(b"\x1b[?1l\x1b>\x1b[?2004l");
            render_once(&mut renderer, &mut grid).unwrap();
            assert_eq!(grid.outer_input_modes(), (false, false, false));
        }

        /// Teardown hands the terminal back at its defaults, in ADR-010's slot: after
        /// the alt-leave and immediately before the keyboard-mode reset, so every
        /// input-encoding restore sits together with the coarsest last.
        #[test]
        fn child_exit_resets_the_mirrored_modes_before_the_relay_reset() {
            let mut clock = VirtualClock::new(vec![
                (0u64, Msg::Pty(b"\x1b[?1049h\x1b[?1h\x1b[?2004h\x1b[>1u".to_vec())),
                (1, Msg::ChildExited(ExitStatus::with_exit_code(0))),
            ]);
            let mut renderer = left_renderer(20, 5);
            let mut term = MockTerminal::new();
            term.enable_mouse().unwrap();
            let mut pty: Vec<u8> = Vec::new();

            run(&mut clock, &mut renderer, &mut term, &mut pty, &NoopResizer, &MockSuspender::disconnected());

            assert_eq!(
                term.restore_calls(),
                vec![
                    Call::Relay(b"\x1b[>1u".to_vec()),
                    Call::Relay(b"\x1b[?1h\x1b[?2004h".to_vec()),
                    Call::LeaveAltScreen,
                    Call::WriteRow(SGR_RESET.to_vec()),
                    Call::Relay(b"\x1b[?1l\x1b[?2004l".to_vec()),
                    Call::Relay(b"\x1b[<1u".to_vec()),
                    Call::DisableMouse,
                    Call::ShowCursor,
                    Call::DisableRawMode,
                ],
                "the mode reset sits between the alt-leave and the keyboard-mode reset"
            );
        }

        /// The other half of ADR-010's rule: a mode gutter never mirrored on is a mode
        /// gutter never turns off. A shell that had its own paste protection running
        /// keeps it.
        #[test]
        fn child_exit_resets_nothing_that_was_never_mirrored() {
            let mut clock = VirtualClock::new(vec![
                (0u64, Msg::Pty(b"plain output".to_vec())),
                (1, Msg::ChildExited(ExitStatus::with_exit_code(0))),
            ]);
            let mut renderer = left_renderer(20, 5);
            let mut term = MockTerminal::new();
            term.enable_mouse().unwrap();
            let mut pty: Vec<u8> = Vec::new();

            run(&mut clock, &mut renderer, &mut term, &mut pty, &NoopResizer, &MockSuspender::disconnected());

            assert!(
                !term.calls.iter().any(|c| matches!(c, Call::Relay(_))),
                "nothing mirrored, nothing reset: {:?}",
                term.calls
            );
        }
    }

    /// The ESC-hold (ADR-020), on virtual time. The hold is loop-owned, so these
    /// drive `apply_message` and `flush_hold` directly with a `VirtualClock`: no
    /// wall-clock sleeps, no threads, no PTY.
    mod esc_hold {
        use super::*;

        const HOLD_MS: u64 = 25;

        /// The band geometry these tests use: margin 0, so nothing here depends on
        /// where the band sits.
        fn ctx() -> Ctx {
            Ctx::new(80, 24, 80, Width::Cols(80))
        }

        #[test]
        fn lone_esc_waits_then_flushes() {
            let mut ctx = ctx();
            ctx.send(b"\x1b");
            assert!(ctx.pty.is_empty(), "the Escape is withheld");
            assert_eq!(ctx.input.hold_deadline, Some(HOLD_MS), "armed at now + 25ms");

            ctx.advance(HOLD_MS - 1);
            assert!(ctx.pty.is_empty(), "still withheld one millisecond short");

            ctx.advance(1);
            assert_eq!(ctx.pty, b"\x1b", "exactly the Escape, once the deadline passes");
            assert_eq!(ctx.input.hold_deadline, None);
        }

        #[test]
        fn esc_completed_within_hold_cancels_it() {
            let mut ctx = ctx();
            ctx.send(b"\x1b");
            ctx.clock.now_ms += 5;
            ctx.send(b"[A");
            assert_eq!(ctx.pty, b"\x1b[A", "the arrow goes out whole");
            assert_eq!(ctx.input.hold_deadline, None, "the deadline is disarmed");

            ctx.advance(HOLD_MS * 2);
            assert_eq!(ctx.pty, b"\x1b[A", "the expired deadline emits nothing more");
        }

        #[test]
        fn plain_bytes_are_never_delayed() {
            let mut ctx = ctx();
            ctx.send(b"abc");
            assert_eq!(ctx.pty, b"abc");
            assert_eq!(
                ctx.input.hold_deadline, None,
                "no deadline means Phase A keeps its single blocking park"
            );
        }

        #[test]
        fn complete_sequence_in_one_chunk_never_arms_the_hold() {
            let mut ctx = ctx();
            ctx.send(b"\x1b[15~");
            assert_eq!(ctx.pty, b"\x1b[15~");
            assert_eq!(ctx.input.hold_deadline, None);
        }

        #[test]
        fn partial_sequence_flushed_whole_on_timeout() {
            let mut ctx = ctx();
            ctx.send(b"\x1b[");
            ctx.advance(HOLD_MS);
            assert_eq!(
                ctx.pty, b"\x1b[",
                "both bytes go out together, in order, not split into Escape then '['"
            );
        }

        #[test]
        fn late_tail_is_forwarded_in_order() {
            let mut ctx = ctx();
            ctx.send(b"\x1b[");
            ctx.advance(HOLD_MS);
            ctx.send(b"3~");
            assert_eq!(
                ctx.pty, b"\x1b[3~",
                "the child's own parser reassembles across the two writes"
            );
        }

        #[test]
        fn nothing_overtakes_the_hold_buffer() {
            let mut ctx = ctx();
            ctx.send(b"\x1b");
            ctx.advance(HOLD_MS);
            ctx.send(b"a");
            assert_eq!(
                ctx.pty, b"\x1ba",
                "reordering would turn a flushed Escape and an 'a' into Alt+a"
            );
        }

        #[test]
        fn double_esc_holds_only_the_second() {
            let mut ctx = ctx();
            ctx.send(b"\x1b\x1b");
            assert_eq!(ctx.pty, b"\x1b", "the first Escape is resolved by the second");
            assert!(ctx.input.hold_deadline.is_some());
            ctx.advance(HOLD_MS);
            assert_eq!(ctx.pty, b"\x1b\x1b");
        }

        #[test]
        fn a_chunk_extending_a_sequence_restarts_the_clock() {
            let mut ctx = ctx();
            ctx.send(b"\x1b");
            assert_eq!(ctx.input.hold_deadline, Some(HOLD_MS));
            ctx.clock.now_ms += 20;
            ctx.send(b"[");
            assert_eq!(
                ctx.input.hold_deadline,
                Some(20 + HOLD_MS),
                "more bytes are evidence more are coming"
            );
        }

        #[test]
        fn mouse_report_is_extracted_not_forwarded() {
            let mut ctx = ctx();
            // The child negotiates SGR mouse so the gate forwards rather than swallows.
            ctx.renderer.parser.process(b"\x1b[?1000h\x1b[?1006h");
            for byte in b"\x1b[<0;10;5M" {
                ctx.send(&[*byte]);
            }
            assert_eq!(
                ctx.pty, b"\x1b[<0;10;5M",
                "the report is re-encoded by the gate at margin 0, not forwarded raw"
            );
            assert_eq!(ctx.input.hold_deadline, None);
        }

        /// The ugly case, asserted honestly rather than wished away: a mouse report
        /// split by a slow link past the hold leaks its prefix as literal text
        /// carrying raw physical coordinates. The mitigation is a longer hold, not
        /// a cleverer scanner.
        #[test]
        fn split_mouse_report_flushes_as_literal_text() {
            let mut ctx = ctx();
            ctx.renderer.parser.process(b"\x1b[?1000h\x1b[?1006h");
            ctx.send(b"\x1b[<0;42");
            ctx.advance(HOLD_MS);
            assert_eq!(ctx.pty, b"\x1b[<0;42", "the fragment leaks verbatim");
            ctx.send(b";5M");
            assert_eq!(ctx.pty, b"\x1b[<0;42;5M", "and so does its tail");
        }
    }

    /// Modal resize-mode state machine, driven through the shared [`Ctx`].
    /// `run_with_resizer` is nested here (rather than a top-level sibling module) so
    /// it can reach `VirtualClock`/`MockTerminal`, which are private to this
    /// `mod tests` — a sibling module cannot see them.
    mod resize_mode {
        use super::*;

        /// Drive the real loop (needed only by the idle tests, which exercise
        /// `run`'s Phase-A bounded wait and top-of-frame idle check — behaviour
        /// `apply_message` alone can't reach).
        fn run_with_resizer(
            script: Vec<(u64, Msg)>,
            width: u16,
            rows: u16,
            real_cols: u16,
            cfg: Width,
        ) -> (Vec<u8>, Renderer, Option<i32>, RecResizer, MockTerminal) {
            let mut clock = VirtualClock::new(script);
            let mut renderer = mode_renderer(width, rows, real_cols, cfg);
            let mut term = MockTerminal::new();
            let mut pty: Vec<u8> = Vec::new();
            let resizer = RecResizer::default();
            let suspender = MockSuspender::disconnected();
            let code = run(&mut clock, &mut renderer, &mut term, &mut pty, &resizer, &suspender);
            (pty, renderer, code, resizer, term)
        }

        #[test]
        fn enter_chord_consumed_no_pty_write() {
            let mut ctx = Ctx::new(80, 24, 80, Width::Cols(80));
            ctx.enter();
            assert!(ctx.resize.active(), "the chord enters the mode");
            assert!(ctx.pty.is_empty(), "the chord must never reach the child");
        }

        #[test]
        fn enter_then_l_grows_one_column() {
            let mut ctx = Ctx::new(80, 24, 200, Width::Cols(80));
            ctx.enter();
            ctx.send(b"l");

            assert_eq!(ctx.renderer.width, 81);
            assert_eq!(ctx.renderer.width_config, Width::Cols(81));
            assert_eq!(*ctx.resizer.calls.borrow(), vec![(81, 24)]);
            assert_eq!(ctx.renderer.parser.screen().size(), (24, 81));
        }

        #[test]
        fn h_l_jump_ten() {
            let mut ctx = Ctx::new(80, 24, 200, Width::Cols(80));
            ctx.enter();
            ctx.send(b"L");
            assert_eq!(ctx.renderer.width, 90, "L jumps by ten");
            ctx.send(b"H");
            assert_eq!(ctx.renderer.width, 80, "H jumps back by ten");
        }

        #[test]
        fn percent_unit_preserved() {
            let mut ctx = Ctx::new(100, 24, 200, Width::Percent(50));
            ctx.enter();
            ctx.send(b"l");
            assert_eq!(ctx.renderer.width_config, Width::Percent(51), "unit stays percentage");
            assert_eq!(ctx.renderer.width, 102, "51% of 200 = 102");
        }

        #[test]
        fn shrink_clamps_at_min_w_silently() {
            let mut ctx = Ctx::new(20, 24, 200, Width::Cols(20));
            ctx.enter();
            ctx.send(b"h");
            assert_eq!(ctx.renderer.width, 20, "MIN_W floor holds");
            assert!(
                ctx.resizer.calls.borrow().is_empty(),
                "a clamped (no-op) step must skip the PTY/parser churn"
            );
        }

        #[test]
        fn grow_clamps_at_real_cols_silently() {
            let mut ctx = Ctx::new(200, 24, 200, Width::Cols(200));
            ctx.enter();
            ctx.send(b"l");
            assert_eq!(ctx.renderer.width, 200, "real_cols ceiling holds");
            assert!(ctx.resizer.calls.borrow().is_empty());
        }

        #[test]
        fn esc_exits_then_keys_pass_through() {
            let mut ctx = Ctx::new(80, 24, 200, Width::Cols(80));
            ctx.enter();
            // A bare Escape is ambiguous until the hold expires — it could still be
            // the start of a mouse report.
            ctx.send(b"\x1b");
            assert!(ctx.resize.active(), "the mode holds while the Escape is ambiguous");
            ctx.expire_hold();
            assert!(!ctx.resize.active(), "Esc exits the mode once the hold resolves it");
            ctx.send(b"l");
            assert_eq!(ctx.pty, b"l", "once exited, l reaches the child instead of stepping");
            assert_eq!(ctx.renderer.width, 80, "no step happened after exit");
        }

        #[test]
        fn chord_again_exits() {
            let mut ctx = Ctx::new(80, 24, 200, Width::Cols(80));
            ctx.enter();
            ctx.enter(); // the chord again, now in mode → exit
            assert!(!ctx.resize.active());
        }

        #[test]
        fn swallow_stays_in_mode_no_pty() {
            let mut ctx = Ctx::new(80, 24, 200, Width::Cols(80));
            ctx.enter();
            ctx.send(b"z");
            assert!(ctx.pty.is_empty(), "an unrecognised key must not leak to the child");
            assert!(ctx.resize.active(), "mode persists after a swallowed key");
            ctx.send(b"l");
            assert_eq!(ctx.renderer.width, 81, "still in mode: l still steps");
        }

        /// Under a relayed keyboard mode every in-mode key arrives as a CSI report, not
        /// as its bare byte — the step keys as much as the Escape. Matching only the
        /// exit would leave the mode open with the steps silently swallowed, which is
        /// worse than not widening it at all.
        #[test]
        fn step_keys_work_in_their_csi_report_forms() {
            let mut ctx = Ctx::new(80, 24, 200, Width::Cols(80));
            ctx.enter();
            // kitty: `l` is codepoint 108, unmodified.
            ctx.send(b"\x1b[108u");
            assert_eq!(ctx.renderer.width, 81, "kitty's `l` steps one column");
            // With Shift it is the same codepoint plus the shift bit — `L`, ten columns.
            ctx.send(b"\x1b[108;2u");
            assert_eq!(ctx.renderer.width, 91, "kitty's Shift-l is `L`, not `l`");
            // modifyOtherKeys spells `h`.
            ctx.send(b"\x1b[27;1;104~");
            assert_eq!(ctx.renderer.width, 90, "modifyOtherKeys' `h` shrinks one column");
            // Ctrl-l is a different key, so it is swallowed rather than stepping.
            ctx.send(b"\x1b[108;5u");
            assert_eq!(ctx.renderer.width, 90, "Ctrl-l is not `l`");
            assert!(ctx.pty.is_empty(), "none of it reaches the child");
        }

        /// Exiting on a key report consumes its release too. The press turns the mode
        /// off, so the release that follows would otherwise classify out-of-mode and be
        /// forwarded — handing the child a key-up with no key-down, exactly the state it
        /// asked for event types in order to track (ADR-021).
        #[test]
        fn the_release_of_a_consumed_exit_press_never_reaches_the_child() {
            for press in [&b"\x1b[27;1:1u"[..], &b"\x1b[92;5:1u"[..]] {
                let mut ctx = Ctx::new(80, 24, 200, Width::Cols(80));
                ctx.enter();
                ctx.send(press);
                assert!(!ctx.resize.active(), "{press:?} exits the mode");
                // The same report with its event sub-parameter turned into a release.
                let mut release = press.to_vec();
                let event = release.len() - 2;
                release[event] = b'3';
                ctx.send(&release);
                assert!(
                    ctx.pty.is_empty(),
                    "the release of the consumed press must not reach the child, got {:?}",
                    String::from_utf8_lossy(&ctx.pty)
                );
                // And the slot is one-shot: an ordinary key after it still passes.
                ctx.send(b"x");
                assert_eq!(ctx.pty, b"x", "the next key is forwarded as normal");
            }
        }

        /// The chord's kitty release form must not read as a second chord press and
        /// toggle straight back out. Only reachable once the child has pushed kitty's
        /// `REPORT_EVENT_TYPES`, which the relay carries out for it (ADR-021).
        #[test]
        fn chord_release_does_not_toggle() {
            let mut ctx = Ctx::new(80, 24, 200, Width::Cols(80));
            ctx.enter();
            ctx.send(b"\x1b[92;5:3u");
            assert!(ctx.resize.active(), "the chord's own release must not exit the mode");
            ctx.send(b"l");
            assert_eq!(ctx.renderer.width, 81, "l still steps after the release");
            assert!(ctx.pty.is_empty());
        }

        /// A held step key under kitty event reporting sends one press, then repeat
        /// reports, then a release. Every repeat is another step, as an auto-repeated
        /// bare byte would be.
        #[test]
        fn a_held_step_key_keeps_stepping_on_kitty_repeats() {
            let mut ctx = Ctx::new(80, 24, 200, Width::Cols(80));
            ctx.enter();
            ctx.send(b"\x1b[108u");
            for _ in 0..3 {
                ctx.send(b"\x1b[108;1:2u");
            }
            ctx.send(b"\x1b[108;1:3u");
            assert_eq!(ctx.renderer.width, 84, "the press and each of three repeats step");
            assert!(ctx.resize.active());
            assert!(
                ctx.pty.is_empty(),
                "nothing reaches the child, got {:?}",
                String::from_utf8_lossy(&ctx.pty)
            );
        }

        /// Holding Escape to leave the mode: the press exits, and the repeats and the
        /// release that follow belong to that consumed press, so none reach the child.
        #[test]
        fn a_held_exit_key_owes_the_child_nothing() {
            let mut ctx = Ctx::new(80, 24, 200, Width::Cols(80));
            ctx.enter();
            ctx.send(b"\x1b[27u");
            assert!(!ctx.resize.active(), "the Escape press exits");
            for _ in 0..3 {
                ctx.send(b"\x1b[27;1:2u");
            }
            ctx.send(b"\x1b[27;1:3u");
            assert!(
                ctx.pty.is_empty(),
                "the held Escape's repeats and release must not reach the child, got {:?}",
                String::from_utf8_lossy(&ctx.pty)
            );
            ctx.send(b"x");
            assert_eq!(ctx.pty, b"x", "the next key is forwarded as normal");
        }

        /// A step key held until the mode idles out: its release arrives out of mode,
        /// and is still owed to the press gutter consumed, not to the child.
        #[test]
        fn a_step_key_released_after_idle_exit_is_still_swallowed() {
            let script = vec![
                (0, Msg::Input(CHORD.to_vec())),
                (0, Msg::Input(b"\x1b[108u".to_vec())),
                (0, Msg::Input(b"\x1b[108;1:2u".to_vec())),
                (0, Msg::Input(b"\x1b[108;1:2u".to_vec())),
                (5000, Msg::Input(b"\x1b[108;1:3u".to_vec())),
                (0, Msg::ChildExited(ExitStatus::with_exit_code(0))),
            ];
            let (pty, renderer, code, _resizer, _term) =
                run_with_resizer(script, 80, 24, 200, Width::Cols(80));

            assert_eq!(code, Some(0));
            assert!(
                pty.is_empty(),
                "the release after idle-exit must not reach the child, got {:?}",
                String::from_utf8_lossy(&pty)
            );
            assert_eq!(renderer.width, 83, "the press and both repeats stepped");
        }

        /// A chunk can carry the chord and step keys together: the classifier
        /// re-reads the mode at every byte, so the mode flips mid-run and both
        /// steps land.
        #[test]
        fn mid_chunk_mode_flip_applies_the_following_steps() {
            let mut ctx = Ctx::new(80, 24, 200, Width::Cols(80));
            ctx.send(b"\x1cll");
            assert!(ctx.resize.active());
            assert_eq!(ctx.renderer.width, 82, "both step keys in the chunk applied");
            assert!(ctx.pty.is_empty(), "nothing in the chunk leaked to the child");
        }

        /// Out of mode every in-mode byte form is forwarded untouched.
        #[test]
        fn out_of_mode_the_in_mode_keys_pass_through() {
            let mut ctx = Ctx::new(80, 24, 200, Width::Cols(80));
            ctx.send(b"hlHL-+=");
            ctx.send(b"\x1b[D");
            ctx.send(b"\x1bOC");
            assert_eq!(ctx.pty, b"hlHL-+=\x1b[D\x1bOC");
            assert_eq!(ctx.renderer.width, 80, "no step happened");
        }

        /// Arrows step in both cursor-key modes; a modified arrow is swallowed
        /// rather than treated as a coarse step.
        #[test]
        fn arrows_step_in_both_cursor_key_modes() {
            let mut ctx = Ctx::new(80, 24, 200, Width::Cols(80));
            ctx.enter();
            ctx.send(b"\x1b[C");
            assert_eq!(ctx.renderer.width, 81, "CSI Right grows one");
            ctx.send(b"\x1bOD");
            assert_eq!(ctx.renderer.width, 80, "SS3 Left shrinks one");
            ctx.send(b"\x1b[1;2D");
            assert_eq!(ctx.renderer.width, 80, "Shift+Left is swallowed, not a coarse step");
            assert!(ctx.resize.active());
            assert!(ctx.pty.is_empty());
        }

        /// Mirrors `resize::resize_clears_the_gutter`: an in-mode shrink on the alt
        /// screen must still clear the gutter across `0..rows`.
        #[test]
        fn alt_screen_step_keeps_clear_gutter_parity() {
            let mut ctx = Ctx::new(100, 6, 120, Width::Cols(100));
            ctx.renderer.outer_alt_active = true;
            ctx.enter();
            ctx.send(b"h"); // shrink one step

            assert_eq!(ctx.renderer.width, 99);
            let expected_margin = geometry::margin(ctx.renderer.layout, 120, 99);
            assert!(
                ctx.term.calls.contains(&Call::ClearGutter(expected_margin, 99, 120, 0, 6)),
                "an in-mode shrink on the alt screen must clear the gutter, calls = {:?}",
                ctx.term.calls
            );
        }

        #[test]
        fn idle_auto_exit_via_virtual_clock() {
            let script = vec![
                (0, Msg::Input(CHORD.to_vec())), // enter at t0
                // Well past the ~3s idle window: the bounded Phase-A wait times
                // out at the deadline, exiting the mode, before this is delivered.
                (5000, Msg::Input(b"l".to_vec())),
                (0, Msg::ChildExited(ExitStatus::with_exit_code(0))),
            ];
            let (pty, renderer, code, resizer, _term) =
                run_with_resizer(script, 80, 24, 200, Width::Cols(80));

            assert_eq!(code, Some(0));
            assert!(resizer.calls.borrow().is_empty(), "idle-exit happened before any step");
            assert_eq!(renderer.width, 80);
            assert_eq!(pty, b"l", "the delayed key passes through once the mode has exited");
        }

        #[test]
        fn idle_refreshes_on_step() {
            let script = vec![
                (0, Msg::Input(CHORD.to_vec())), // enter at t0
                // A step at t0+2000 re-arms the idle deadline to ~t0+5000, not
                // ~t0+3000 — so this key, another 5s later, still finds the mode
                // active until the RE-ARMED deadline.
                (2000, Msg::Input(b"l".to_vec())),
                (5000, Msg::Input(b"l".to_vec())),
                (0, Msg::ChildExited(ExitStatus::with_exit_code(0))),
            ];
            let (pty, renderer, code, resizer, _term) =
                run_with_resizer(script, 80, 24, 200, Width::Cols(80));

            assert_eq!(code, Some(0));
            assert_eq!(renderer.width, 81, "the in-mode step before idle-exit applied");
            assert_eq!(*resizer.calls.borrow(), vec![(81, 24)], "exactly one resize, from the step");
            assert_eq!(pty, b"l", "the delayed key after idle-exit passes through to the child");
        }

        /// A paste span with the chord byte in it, once the child has asked for paste
        /// guards. Every byte reaches the child and none of it is read as input
        /// protocol: no resize mode, and a pasted mouse report arrives verbatim rather
        /// than margin-translated (or dropped for being malformed).
        fn paste_span() -> Vec<u8> {
            b"\x1b[200~ab\x1cc\x1b[<0;10;5M\x1b[<99Md\x1b[201~".to_vec()
        }

        /// Put the mirror into paste mode the way production does — the child asks,
        /// the frame mirrors.
        fn mirror_paste_on(ctx: &mut Ctx) {
            ctx.renderer.parser.process(b"\x1b[?2004h");
            render_once(&mut ctx.renderer, &mut ctx.term).unwrap();
            assert!(ctx.renderer.mode_mirror.bracketed_paste());
        }

        #[test]
        fn pasted_bytes_reach_the_child_untouched_under_the_guards() {
            let mut ctx = Ctx::new(80, 24, 200, Width::Cols(80));
            mirror_paste_on(&mut ctx);
            ctx.send(&paste_span());

            assert_eq!(ctx.pty, paste_span(), "every pasted byte, verbatim");
            assert!(!ctx.resize.active(), "a pasted 0x1C is text, not the chord");
        }

        /// The gate is a gate. Without the mirror the same span is ordinary input, so
        /// the `0x1C` enters resize mode and the mouse report is extracted and
        /// translated.
        #[test]
        fn the_same_span_is_scanned_normally_with_the_mirror_off() {
            let mut ctx = Ctx::new(80, 24, 200, Width::Cols(80));
            ctx.send(&paste_span());

            assert!(ctx.resize.active(), "the bare 0x1C enters resize mode");
            assert!(
                !ctx.pty.windows(6).any(|w| w == b"\x1b[<0;1"),
                "the mouse report was extracted, not forwarded: {:?}",
                ctx.pty
            );
        }

        /// A paste that arrives while the resize overlay is up. The mode swallows
        /// keystrokes, but a paste is not keystrokes: it reaches the child whole,
        /// guards and all, and leaves the mode where it found it.
        #[test]
        fn a_paste_arriving_in_resize_mode_still_reaches_the_child_whole() {
            let mut ctx = Ctx::new(80, 24, 200, Width::Cols(80));
            mirror_paste_on(&mut ctx);
            ctx.enter();
            ctx.send(&paste_span());

            assert_eq!(ctx.pty, paste_span(), "every pasted byte, verbatim");
            assert!(ctx.resize.active(), "the paste is the child's, not the mode's");
            assert_eq!(ctx.renderer.width, 80, "and it stepped nothing");
        }

        /// The child disabling paste mode mid-paste re-arms normal scanning at once.
        #[test]
        fn clearing_paste_mode_mid_paste_re_arms_the_scanner() {
            let mut ctx = Ctx::new(80, 24, 200, Width::Cols(80));
            mirror_paste_on(&mut ctx);
            ctx.send(b"\x1b[200~ab");
            assert_eq!(ctx.pty, b"\x1b[200~ab");

            ctx.renderer.parser.process(b"\x1b[?2004l");
            render_once(&mut ctx.renderer, &mut ctx.term).unwrap();
            ctx.send(&[0x1c]);
            assert!(ctx.resize.active(), "the tail is scanned normally again");
        }
    }

    /// The suspend/resume cycle (ADR-0019): park → self-stop → unpark →
    /// continue-child ordering, driven against a `MockTerminal` and `MockSuspender`
    /// that share one interleaved order log, so restore-before-self-stop and
    /// raw-first-on-resume are one assertable sequence.
    mod suspend_cycle_tests {
        use super::super::{
            dispatch, render_once, run, suspend_cycle, Flow, Renderer, ResizeCtl, SuspendOutcome,
        };
        use super::{left_renderer, NoopResizer, RecResizer, VirtualClock};
        use crate::msg::Msg;
        use crate::suspend::mock::{MockSuspender, OrderLog};
        use crate::terminal::mock::{Call, MockTerminal};
        use crate::terminal::OuterTerminal;
        use portable_pty::ExitStatus;
        use std::cell::RefCell;
        use std::rc::Rc;

        /// The lifecycle + suspend markers that carry the ordering; render-output
        /// noise (MoveTo/WriteRow/Flush/PlaceCursor/…) is filtered out.
        fn significant(log: &[Call]) -> Vec<Call> {
            log.iter()
                .filter(|c| {
                    matches!(
                        c,
                        Call::EnterAltScreen
                            | Call::LeaveAltScreen
                            | Call::EnableMouse
                            | Call::DisableMouse
                            | Call::EnableRawMode
                            | Call::DisableRawMode
                            | Call::ShowCursor
                            | Call::Relay(_)
                            | Call::SuspendSelf
                            | Call::ContinueChild
                    )
                })
                .cloned()
                .collect()
        }

        /// Build a shared-log terminal + renderer, simulate main.rs's eager mouse
        /// capture so park has something to disable, then clear the log so only the
        /// cycle is recorded.
        fn harness() -> (OrderLog, MockTerminal, Renderer) {
            let log: OrderLog = Rc::new(RefCell::new(Vec::new()));
            let mut term = MockTerminal::with_log(log.clone());
            // Report the renderer's own size so the resume resize-catch-up is a no-op
            // unless a test deliberately flips it.
            term.set_terminal_size(40, 10);
            term.enable_mouse().unwrap();
            let renderer = left_renderer(40, 10);
            log.borrow_mut().clear();
            term.calls.clear();
            (log, term, renderer)
        }

        fn drive(
            log: &OrderLog,
            term: &mut MockTerminal,
            renderer: &mut Renderer,
        ) -> SuspendOutcome {
            let mut clock = VirtualClock::new(vec![]);
            let mut resize = ResizeCtl::inactive();
            let mut pty: Vec<u8> = Vec::new();
            let suspender = MockSuspender::new(log.clone());
            suspend_cycle(
                &mut clock,
                renderer,
                &mut resize,
                term,
                &mut pty,
                &NoopResizer,
                &suspender,
            )
        }

        /// `dispatch(ChildStopped)` maps to `Flow::Suspend`.
        #[test]
        fn child_stopped_dispatches_to_suspend() {
            let mut renderer = left_renderer(40, 10);
            let mut term = MockTerminal::new();
            let mut pty: Vec<u8> = Vec::new();
            let flow = dispatch(
                Msg::ChildStopped { sig: 18 },
                &mut renderer,
                &mut pty,
                &NoopResizer,
                &mut term,
            );
            assert_eq!(flow, Flow::Suspend);
        }

        /// Child in the alt screen: outer leaves alt at park and re-enters at resume,
        /// with the full restore-before-self-stop / raw-first-on-resume ordering.
        #[test]
        fn ordering_child_in_alt() {
            let (log, mut term, mut renderer) = harness();
            // Child (and outer) already in the alt screen — no step-2 enter edge.
            renderer.parser.process(b"\x1b[?1049h");
            renderer.outer_alt_active = true;

            let outcome = drive(&log, &mut term, &mut renderer);
            assert!(matches!(outcome, SuspendOutcome::Resumed));

            assert_eq!(
                significant(&log.borrow()),
                vec![
                    Call::LeaveAltScreen,
                    Call::DisableMouse,
                    Call::ShowCursor,
                    Call::DisableRawMode,
                    Call::SuspendSelf,
                    Call::EnableRawMode,
                    Call::EnableMouse,
                    Call::EnterAltScreen,
                    Call::ContinueChild,
                ]
            );
        }

        /// Primary/inline child: no alt enter/leave; a Newline hand-back parks the
        /// shell below the band; base_row reseeds to the bottom; continue after
        /// re-setup.
        #[test]
        fn ordering_primary_inline() {
            let (log, mut term, mut renderer) = harness();
            renderer.ever_painted_inline = true;

            let outcome = drive(&log, &mut term, &mut renderer);
            assert!(matches!(outcome, SuspendOutcome::Resumed));

            assert_eq!(
                significant(&log.borrow()),
                vec![
                    Call::DisableMouse,
                    Call::ShowCursor,
                    Call::DisableRawMode,
                    Call::SuspendSelf,
                    Call::EnableRawMode,
                    Call::EnableMouse,
                    Call::ContinueChild,
                ],
                "no alt enter/leave on the primary screen"
            );
            assert!(
                !log.borrow().iter().any(|c| matches!(c, Call::Relay(_))),
                "a child that asked for no keyboard mode gets none relayed at park"
            );
            assert!(
                term.calls.contains(&Call::Newline),
                "the inline hand-back drops the shell a fresh line below the band"
            );
            assert_eq!(
                renderer.base_row, 9,
                "base_row reseeds to the bottom row (rows - 1) on resume"
            );
        }

        /// A child that negotiated keyboard modes adds one step at each end of the
        /// cycle (ADR-021): park undoes them before the shell gets the terminal, and
        /// unpark replays them after raw mode is back and before mouse capture. The
        /// replayed bytes are the canonical ones originally relayed, in the order the
        /// child issued them, so the terminal comes back at the same stack depth and
        /// a later pop from the child still lines up.
        #[test]
        fn ordering_relayed_modes_reset_at_park_and_replayed_at_resume() {
            let (log, mut term, mut renderer) = harness();
            renderer.ever_painted_inline = true;
            // The child pushed a kitty level and turned modifyOtherKeys on.
            renderer.parser.process(b"\x1b[>1u\x1b[>4;2m");

            let outcome = drive(&log, &mut term, &mut renderer);
            assert!(matches!(outcome, SuspendOutcome::Resumed));

            assert_eq!(
                significant(&log.borrow()),
                vec![
                    Call::Relay(b"\x1b[<1u\x1b[>4;0m".to_vec()),
                    Call::DisableMouse,
                    Call::ShowCursor,
                    Call::DisableRawMode,
                    Call::SuspendSelf,
                    Call::EnableRawMode,
                    Call::Relay(b"\x1b[>1u\x1b[>4;2m".to_vec()),
                    Call::EnableMouse,
                    Call::ContinueChild,
                ],
                "reset before the hand-back, replay after raw mode is re-taken"
            );
        }

        /// The mirrored input modes (ADR-022) come off before the shell gets the
        /// terminal and go back on after `fg` — and the re-assert is the ordinary
        /// per-frame poll finding the child's live modes disagreeing with a mirror park
        /// cleared, so there is no replay list and no `rearm` call anywhere.
        #[test]
        fn ordering_mirrored_modes_reset_at_park_and_re_polled_at_resume() {
            let (log, mut term, mut renderer) = harness();
            renderer.ever_painted_inline = true;
            // The child asked its terminal for application cursor keys and paste guards.
            renderer.parser.process(b"\x1b[?1h\x1b[?2004h");
            render_once(&mut renderer, &mut term).unwrap();
            log.borrow_mut().clear();

            let outcome = drive(&log, &mut term, &mut renderer);
            assert!(matches!(outcome, SuspendOutcome::Resumed));

            assert_eq!(
                significant(&log.borrow()),
                vec![
                    Call::Relay(b"\x1b[?1l\x1b[?2004l".to_vec()),
                    Call::DisableMouse,
                    Call::ShowCursor,
                    Call::DisableRawMode,
                    Call::SuspendSelf,
                    Call::EnableRawMode,
                    Call::EnableMouse,
                    Call::ContinueChild,
                    Call::Relay(b"\x1b[?1h\x1b[?2004h".to_vec()),
                ],
                "off before the hand-back, back on from the resume repaint's poll"
            );
        }

        /// Suspend while resize mode is active: the overlay is cleared (step 0)
        /// before the terminal is parked, and the mode is left.
        #[test]
        fn resize_mode_overlay_cleared_before_park() {
            let (log, mut term, mut renderer) = harness();
            renderer.ever_painted_inline = true;

            let mut clock = VirtualClock::new(vec![]);
            let mut resize = ResizeCtl::inactive();
            resize.arm(1_000); // in resize mode
            let mut pty: Vec<u8> = Vec::new();
            let suspender = MockSuspender::new(log.clone());
            suspend_cycle(
                &mut clock,
                &mut renderer,
                &mut resize,
                &mut term,
                &mut pty,
                &NoopResizer,
                &suspender,
            );

            assert!(!resize.active(), "resize mode is left before parking");
            let calls = log.borrow();
            let clear_idx = calls
                .iter()
                .position(|c| matches!(c, Call::ClearGutter(..)))
                .expect("the overlay clear ran");
            let stop_idx = calls
                .iter()
                .position(|c| *c == Call::SuspendSelf)
                .expect("the cycle self-stopped");
            assert!(clear_idx < stop_idx, "the overlay is cleared before the self-stop");
        }

        /// Abort: the child is SIGKILLed right after stopping, surfacing as a
        /// `ChildExited` in the pre-stop drain. The cycle returns `ChildExited`
        /// without ever self-stopping or parking the terminal (no double restore).
        #[test]
        fn abort_when_child_dies_in_drain() {
            let (log, mut term, mut renderer) = harness();
            renderer.ever_painted_inline = true;

            let mut clock =
                VirtualClock::new(vec![(0, Msg::ChildExited(ExitStatus::with_exit_code(7)))]);
            let mut resize = ResizeCtl::inactive();
            let mut pty: Vec<u8> = Vec::new();
            let suspender = MockSuspender::new(log.clone());
            let outcome = suspend_cycle(
                &mut clock,
                &mut renderer,
                &mut resize,
                &mut term,
                &mut pty,
                &NoopResizer,
                &suspender,
            );

            assert!(matches!(outcome, SuspendOutcome::ChildExited(7)));
            let calls = log.borrow();
            assert!(
                !calls.contains(&Call::SuspendSelf),
                "the abort must never self-stop"
            );
            assert!(
                !term.calls.contains(&Call::DisableRawMode),
                "the terminal was never parked, so raw mode was never dropped"
            );
        }

        /// Missed-resize catch-up: the outer terminal resized while gutter slept.
        /// Step 7 re-sizes the child PTY (to the band width at the new row count)
        /// BEFORE continue_child, so the child wakes to one SIGWINCH at the right
        /// size.
        #[test]
        fn missed_resize_catch_up_before_continue() {
            let (log, mut term, mut renderer) = harness();
            renderer.ever_painted_inline = true;

            // The shell resized the terminal to 50x12 while gutter was stopped.
            let size = term.size_cell();
            let suspender =
                MockSuspender::new(log.clone()).on_suspend(move || size.set((50, 12)));
            let resizer = RecResizer::default();
            let mut clock = VirtualClock::new(vec![]);
            let mut resize = ResizeCtl::inactive();
            let mut pty: Vec<u8> = Vec::new();
            let outcome = suspend_cycle(
                &mut clock,
                &mut renderer,
                &mut resize,
                &mut term,
                &mut pty,
                &resizer,
                &suspender,
            );
            assert!(matches!(outcome, SuspendOutcome::Resumed));

            // The PTY was resized once — to the band width W (40, unchanged for this
            // absolute width) at the new 12 rows.
            assert_eq!(*resizer.calls.borrow(), vec![(40, 12)]);
            assert_eq!(renderer.real_cols, 50, "the new real width is picked up");
            // And that resize ran before the child was continued.
            let calls = log.borrow();
            let first_clear = calls
                .iter()
                .position(|c| matches!(c, Call::ClearGutter(..)))
                .expect("the resize cleared the gutter");
            let cont = calls
                .iter()
                .position(|c| *c == Call::ContinueChild)
                .expect("the child was continued");
            assert!(
                first_clear < cont,
                "the resize (TIOCSWINSZ + clear) runs before continue_child"
            );
        }

        /// Regression: a terminal resized while gutter slept must not let the resume
        /// catch-up blank shell history. On the primary screen `base_row` is stale after
        /// suspension (the shell scrolled under the band), so the interior clear must run
        /// only after base_row is reseeded to the bottom — a single row, never the
        /// `[stale_base_row, rows)` span that still holds the user's shell output.
        #[test]
        fn resume_resize_interior_clear_spares_shell_history() {
            let (log, mut term, mut renderer) = harness();
            renderer.ever_painted_inline = true;
            // The band was anchored mid-screen before the stop; the shell then scrolled
            // under it, leaving base_row pointing into live shell output.
            renderer.base_row = 5;

            // The shell widened the terminal to 50 cols (same 10 rows) while gutter slept.
            let size = term.size_cell();
            let suspender =
                MockSuspender::new(log.clone()).on_suspend(move || size.set((50, 10)));
            let resizer = RecResizer::default();
            let mut clock = VirtualClock::new(vec![]);
            let mut resize = ResizeCtl::inactive();
            let mut pty: Vec<u8> = Vec::new();
            let outcome = suspend_cycle(
                &mut clock,
                &mut renderer,
                &mut resize,
                &mut term,
                &mut pty,
                &resizer,
                &suspender,
            );
            assert!(matches!(outcome, SuspendOutcome::Resumed));
            assert_eq!(
                renderer.base_row, 9,
                "base_row reseeds to the bottom before the catch-up interior clear"
            );

            // Every interior clear the resume emitted is confined to the reseeded bottom
            // row — never a span starting at the stale mid-screen anchor (which would
            // erase the shell history above the band).
            for c in term.calls.iter() {
                if let Call::ClearRowSpan(start, end) = c {
                    assert_eq!(
                        (*start, *end),
                        (9, 10),
                        "interior clear must stay at the reseeded bottom row, got ({start}, {end})"
                    );
                }
            }
            assert!(
                term.calls.iter().any(|c| matches!(c, Call::ClearRowSpan(..))),
                "the widen catch-up did run an interior clear"
            );
        }

        /// `ChildContinued` alone (external `kill -CONT`, or gutter's own SIGCONT on
        /// a non-macOS box): a repaint hint only — it never self-stops, and it forces
        /// the next frame to fully repaint.
        #[test]
        fn child_continued_forces_repaint_no_suspend() {
            let mut renderer = left_renderer(40, 3);
            let mut term = MockTerminal::new();
            let mut pty: Vec<u8> = Vec::new();

            // Paint "hi" and sync the baseline, so an unchanged frame would repaint
            // nothing.
            renderer.parser.process(b"hi");
            render_once(&mut renderer, &mut term).unwrap();
            term.calls.clear();

            let flow = dispatch(
                Msg::ChildContinued,
                &mut renderer,
                &mut pty,
                &NoopResizer,
                &mut term,
            );
            assert_eq!(flow, Flow::Continue, "a continue never suspends");

            // The baseline was reset, so the next frame re-emits the row.
            render_once(&mut renderer, &mut term).unwrap();
            assert!(
                term.calls.iter().any(|c| matches!(c, Call::WriteRow(_))),
                "ChildContinued forces a full repaint next frame"
            );
        }

        /// Cursor-shape rearm: park hands the shell a default cursor, and resume
        /// re-asserts the child's last requested shape (the watcher's mirrored state
        /// is stale after park).
        #[test]
        fn cursor_shape_reasserted_on_resume() {
            let (log, mut term, mut renderer) = harness();
            renderer.ever_painted_inline = true;

            // Child requested a steady bar (CSI 6 SP q); mirror it once, then isolate.
            renderer.parser.process(b"\x1b[6 q");
            render_once(&mut renderer, &mut term).unwrap();
            log.borrow_mut().clear();
            term.calls.clear();

            drive(&log, &mut term, &mut renderer);

            let shapes: Vec<Vec<u8>> = term
                .calls
                .iter()
                .filter_map(|c| match c {
                    Call::SetCursorShape(b) => Some(b.clone()),
                    _ => None,
                })
                .collect();
            assert_eq!(
                shapes,
                vec![b"\x1b[0 q".to_vec(), b"\x1b[6 q".to_vec()],
                "park resets to the default cursor, resume re-asserts the child's shape"
            );
        }

        /// End-to-end through `run` (not `suspend_cycle` directly): a scripted
        /// `ChildStopped` drives the whole cycle (SuspendSelf + ContinueChild fire),
        /// and a `Pty` message queued behind it — arriving 150ms later, past the 100ms
        /// pre-stop drain cap — is processed on the frame after resume, proving the
        /// `continue 'frames` path. Then the child exits cleanly and the code
        /// propagates.
        #[test]
        fn run_drives_suspend_cycle_then_processes_queued_message() {
            let log: OrderLog = Rc::new(RefCell::new(Vec::new()));
            let mut term = MockTerminal::with_log(log.clone());
            // Match the renderer's size so the resume resize-catch-up is a no-op.
            term.set_terminal_size(40, 10);
            let mut renderer = left_renderer(40, 10);
            let mut pty: Vec<u8> = Vec::new();
            let suspender = MockSuspender::new(log.clone());

            let mut clock = VirtualClock::new(vec![
                (0, Msg::ChildStopped { sig: 18 }),
                // Beyond SUSPEND_DRAIN_CAP (100ms): delivered post-resume, not in the drain.
                (150, Msg::Pty(b"hello".to_vec())),
                (0, Msg::ChildExited(ExitStatus::with_exit_code(0))),
            ]);

            let code = run(
                &mut clock,
                &mut renderer,
                &mut term,
                &mut pty,
                &NoopResizer,
                &suspender,
            );

            // The cycle ran end to end.
            let calls = log.borrow();
            assert!(
                calls.contains(&Call::SuspendSelf),
                "the suspend cycle self-stopped"
            );
            assert!(
                calls.contains(&Call::ContinueChild),
                "the child was continued on resume"
            );
            drop(calls);

            // The queued Pty was processed after resume: its bytes reached the grid.
            let row0 = renderer.screen().rows(0, 40).next().unwrap();
            assert!(
                row0.starts_with("hello"),
                "the post-resume Pty painted into the grid: {row0:?}"
            );

            assert_eq!(code, Some(0), "the child's clean exit propagated");
        }
    }
}

/// Hand one chunk of child bytes to the live parser and the scroll tracker, exactly as
/// the render loop's `Msg::Pty` dispatch does, so the tests exercise the real per-frame
/// scroll detection. Called once per chunk, it also splits a stream wherever a test
/// wants — including mid escape sequence, which only parses the same as the whole
/// stream if both parsers carry their vte state across the boundary.
#[cfg(test)]
fn feed(renderer: &mut Renderer, bytes: &[u8]) {
    renderer.parser.process(bytes);
    renderer.scroll_tracker.process(bytes);
}

/// Shared test helper for wide-char (CJK / emoji) edge-of-band correctness (ADR-006).
/// Build a renderer at `width × rows` with margin `margin`, feed `bytes` straight into
/// the parser, and paint one frame through the primary `rows_diff` path into a fresh
/// [`RecordingGrid`] sized to the physical outer terminal (`phys_cols`). Returns the
/// renderer (for child-grid assertions) and the painted physical grid — the cases assert
/// both on the child grid and on the physical outer cells at column `margin + W`, where a
/// real bleed would live.
#[cfg(test)]
fn render_primary(
    bytes: &[u8],
    width: u16,
    rows: u16,
    margin: u16,
    phys_cols: u16,
) -> (Renderer, crate::terminal::mock::RecordingGrid) {
    let mut renderer = Renderer::at_margin(width, rows, margin);
    renderer.parser.process(bytes);
    let mut grid = crate::terminal::mock::RecordingGrid::new(phys_cols, rows);
    render_once(&mut renderer, &mut grid).unwrap();
    (renderer, grid)
}

/// Paint one real frame per element of `frames` and hand back both sides the oracle's
/// painted-band checks compare (`src/oracle/band.rs`): the settled renderer, holding the
/// child's own `W`-column grid and the offset the band ended up at, and the exact bytes
/// gutter wrote to the terminal, in order.
///
/// A frame boundary is where the interesting paths live — the `prev` baseline diff,
/// `scroll_to_make_room` and `emit_scroll_stream` all describe what changed since the
/// last frame, and what a frame leaves behind on the real terminal is only visible to
/// the frame after it. One frame reaches none of them.
///
/// Lives here because it drives the production `render_once` through a private
/// renderer; the wezterm replay and the comparison are the oracle's.
#[cfg(all(test, feature = "oracle"))]
pub(crate) fn paint_frames_to_tape(
    frames: &[&[u8]],
    geom: crate::oracle::band::BandGeometry,
) -> (Renderer, Vec<u8>) {
    let mut renderer = Renderer::at_margin(geom.width, geom.rows, geom.margin);
    renderer.real_cols = geom.phys_cols;
    renderer.base_row = geom.base_row.min(geom.rows.saturating_sub(1));
    let mut tape = crate::oracle::band::Tape::new(geom.phys_cols, geom.phys_rows);
    for piece in frames {
        feed(&mut renderer, piece);
        render_once(&mut renderer, &mut tape).unwrap();
    }
    (renderer, tape.into_bytes())
}

#[cfg(test)]
mod cjk {
    use super::*;
    use crate::terminal::mock::RecordingGrid;

    /// U+4E00 (一), the canonical double-width CJK ideograph. Two grid cells: a
    /// lead cell holding "一" and a continuation cell (byte-length zero).
    const HAN: &str = "\u{4e00}";

    /// As [`render_primary`], but paint through the cell-walking fallback.
    fn render_fallback(
        bytes: &[u8],
        width: u16,
        rows: u16,
        margin: u16,
        phys_cols: u16,
    ) -> (Renderer, RecordingGrid) {
        let mut renderer = Renderer::at_margin(width, rows, margin);
        renderer.parser.process(bytes);
        let mut grid = RecordingGrid::new(phys_cols, rows);
        render_cell_walk(renderer.parser.screen(), margin, width, &mut grid).unwrap();
        (renderer, grid)
    }

    /// Where the wide glyph's lead cell landed in the child grid: `(row, col)`,
    /// or `None` if no wide cell is present.
    fn wide_lead(screen: &vt100::Screen, width: u16) -> Option<(u16, u16)> {
        let (rows, _) = screen.size();
        for row in 0..rows {
            for col in 0..width {
                if let Some(cell) = screen.cell(row, col)
                    && cell.is_wide()
                    && cell.contents() == HAN
                {
                    return Some((row, col));
                }
            }
        }
        None
    }

    /// Every physical outer cell at column `margin + W` (the first gutter column) must be
    /// blank on every row — the "no bleed past the band" assertion.
    fn assert_band_edge_blank(grid: &RecordingGrid, margin: u16, width: u16, rows: u16) {
        let edge = margin + width;
        for row in 0..rows {
            let c = grid.cell_contents(row, edge);
            assert!(
                c.is_empty() || c == " ",
                "physical band-edge cell (row {row}, col margin+W = {edge}) must be \
                 blank, found {c:?} — content bled past the band"
            );
        }
    }

    /// CJK case 1 — DECAWM on, wide glyph at child col `W-1`. Set autowrap, position the
    /// cursor at the last in-band column, emit U+4E00. vt100's margin rule wraps the glyph
    /// to the next row rather than placing it at `W-1`. The child grid shows the glyph
    /// wrapped (lead cell at col 0 of a later row, not `W-1`), and physical `margin + W`
    /// is blank.
    #[test]
    fn case1_decawm_on_wraps_inside_band() {
        let (w, rows, margin, phys) = (8u16, 4u16, 6u16, 20u16);
        // ?7h = DECAWM on; CSI 1;W H = cursor to row 0, col W-1 (1-based W).
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\x1b[?7h");
        bytes.extend_from_slice(format!("\x1b[1;{w}H").as_bytes());
        bytes.extend_from_slice(HAN.as_bytes());

        let (renderer, grid) = render_primary(&bytes, w, rows, margin, phys);

        // Child side: the glyph is in-grid and NOT at the original row's W-1.
        let lead = wide_lead(renderer.parser.screen(), w).expect("wide glyph present");
        assert!(lead.1 < w - 1, "glyph wrapped: lead col {} must be < W-1", lead.1);
        assert_ne!(lead, (0, w - 1), "glyph must not sit at the original row's W-1");

        // Physical side: nothing bled past the band.
        assert_band_edge_blank(&grid, margin, w, rows);
    }

    /// CJK case 2 — DECAWM off, wide glyph at child col `W-1`. Clear autowrap, same
    /// position, emit U+4E00 — a distinct vt100 code path. Whatever vt100 does with the
    /// glyph (clamp/drop/wrap), the invariant holds: the glyph stays inside the `W`-column
    /// grid and physical `margin + W` is blank.
    #[test]
    fn case2_decawm_off_stays_inside_band() {
        let (w, rows, margin, phys) = (8u16, 4u16, 6u16, 20u16);
        // ?7l = DECAWM off.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\x1b[?7l");
        bytes.extend_from_slice(format!("\x1b[1;{w}H").as_bytes());
        bytes.extend_from_slice(HAN.as_bytes());

        let (renderer, grid) = render_primary(&bytes, w, rows, margin, phys);
        let screen = renderer.parser.screen();

        // Child side: the grid is exactly W columns and the glyph (if present)
        // is inside it — never at an out-of-grid column.
        assert_eq!(screen.size().1, w, "child grid must stay exactly W columns");
        if let Some((_, col)) = wide_lead(screen, w) {
            assert!(col < w, "lead cell col {col} must stay inside W {w}");
        }
        // No continuation cell ever lands at the (nonexistent) column W; the
        // last in-grid column W-1 must not be a wide LEAD (its continuation
        // would fall out of grid).
        if let Some(cell) = screen.cell(0, w - 1) {
            assert!(
                !cell.is_wide(),
                "a wide lead at W-1 would push its continuation out of the band"
            );
        }

        // Physical side: nothing bled past the band.
        assert_band_edge_blank(&grid, margin, w, rows);
    }

    /// CJK case 3 — exactly one in-band cell remaining. Position so only the final column
    /// is free, then attempt a wide glyph that needs two columns. It can't fit at `W-1`
    /// (its continuation would be column `W`), so vt100 wraps/drops it inside the band —
    /// no half-glyph spill. Physical `margin + W` is blank.
    #[test]
    fn case3_one_cell_remaining_no_half_glyph() {
        let (w, rows, margin, phys) = (6u16, 4u16, 8u16, 20u16);
        // Fill cols 0..W-1 with ASCII so the cursor naturally rests at the last
        // free column (W-1), one cell remaining, then emit the wide glyph.
        let filler: String = "x".repeat((w - 1) as usize);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\x1b[?7h\x1b[1;1H");
        bytes.extend_from_slice(filler.as_bytes());
        bytes.extend_from_slice(HAN.as_bytes());

        let (renderer, grid) = render_primary(&bytes, w, rows, margin, phys);
        let screen = renderer.parser.screen();

        // No wide LEAD at the last column — that would spill its continuation.
        if let Some(cell) = screen.cell(0, w - 1) {
            assert!(!cell.is_wide(), "no wide lead at the last in-band column");
        }
        // Physical side: the half-glyph never reached the gutter.
        assert_band_edge_blank(&grid, margin, w, rows);
    }

    /// CJK case 4 — cell-walking fallback exercised. Drive a wide glyph, then repaint
    /// through `render_cell_walk`. The continuation cell is skipped (physical grid shows
    /// exactly one glyph, no double-glyph), there is no double-advance, and physical
    /// `margin + W` is blank. The one case that exercises gutter's own
    /// `is_wide_continuation` skip rather than vt100's margin rule.
    #[test]
    fn case4_fallback_skips_continuation_no_double_glyph() {
        let (w, rows, margin, phys) = (8u16, 4u16, 6u16, 20u16);
        // Two wide glyphs from col 0: 一 二 — lead/continuation/lead/continuation.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\x1b[1;1H");
        bytes.extend_from_slice(HAN.as_bytes());
        bytes.extend_from_slice("\u{4e8c}".as_bytes()); // 二

        let (_renderer, grid) = render_fallback(&bytes, w, rows, margin, phys);

        // Physical side: exactly the two glyphs, at physical cols margin+0 and
        // margin+2 — no double-glyph, no drift. The continuation columns
        // (margin+1, margin+3) hold the glyph's right half as painted by the
        // terminal's own wide-char advance, NOT a duplicate glyph.
        assert_eq!(
            grid.cell_contents(0, margin),
            HAN,
            "first glyph at physical col margin"
        );
        assert_eq!(
            grid.cell_contents(0, margin + 2),
            "\u{4e8c}",
            "second glyph at physical col margin+2 (no double-advance drift)"
        );
        // The continuation column of the first glyph is NOT a second copy of it.
        let cont = grid.cell_contents(0, margin + 1);
        assert!(
            cont.is_empty() || cont == " ",
            "continuation column must not carry a duplicate glyph, found {cont:?}"
        );

        // Count the painted glyphs across the whole physical grid: exactly two,
        // no double-glyph from a continuation being emitted.
        let mut glyphs = 0;
        for r in 0..rows {
            for c in 0..phys {
                let s = grid.cell_contents(r, c);
                if s == HAN || s == "\u{4e8c}" {
                    glyphs += 1;
                }
            }
        }
        assert_eq!(glyphs, 2, "fallback must paint exactly two glyphs, not four");

        assert_band_edge_blank(&grid, margin, w, rows);
    }

    /// The fallback's continuation-skip emits no `move_to` to a continuation column.
    /// Against [`MockTerminal`]'s recorded calls: with two wide glyphs the fallback issues
    /// exactly two `move_to`s (one per lead cell), never one per cell.
    #[test]
    fn case4_fallback_emits_no_move_for_continuation() {
        use crate::terminal::mock::{Call, MockTerminal};
        let (w, rows, margin) = (8u16, 4u16, 6u16);
        let mut renderer = Renderer::at_margin(w, rows, margin);
        renderer
            .parser
            .process(format!("\x1b[1;1H{HAN}\u{4e8c}").as_bytes());
        let mut term = MockTerminal::new();
        render_cell_walk(renderer.parser.screen(), margin, w, &mut term).unwrap();

        let moves: Vec<&Call> = term
            .calls
            .iter()
            .filter(|c| matches!(c, Call::MoveTo(..)))
            .collect();
        assert_eq!(
            moves,
            vec![&Call::MoveTo(margin, 0), &Call::MoveTo(margin + 2, 0)],
            "exactly one move_to per lead cell — no move to a continuation column"
        );
    }

    /// insta golden-master: a representative CJK line rendered through the primary path,
    /// snapshotting the physical outer grid. Freezes the in-band column alignment so any
    /// future wide-char drift surfaces as a snapshot diff.
    #[test]
    fn cjk_line_snapshot() {
        let (w, rows, margin, phys) = (20u16, 3u16, 5u16, 40u16);
        // A short mixed Han/Hiragana line plus trailing ASCII, from home.
        let line = "\u{4e00}\u{4e8c}\u{4e09}\u{3042}\u{3044}ok";
        let bytes = format!("\x1b[1;1H{line}");
        let (_r, grid) = render_primary(bytes.as_bytes(), w, rows, margin, phys);

        // Snapshot the physical grid as one trimmed String per row, so the
        // column each glyph occupies is frozen.
        let painted: Vec<String> = (0..rows)
            .map(|r| {
                let mut s = String::new();
                for c in 0..phys {
                    s.push_str(&grid.cell_contents(r, c));
                }
                s.trim_end().to_string()
            })
            .collect();
        insta::assert_debug_snapshot!(painted);

        // And the boundary still holds for the snapshotted fixture.
        assert_band_edge_blank(&grid, margin, w, rows);
    }

    /// Primary-path purity (ADR-006): the production `render_once` source must contain no
    /// `is_wide_continuation` call and no per-cell walk. Reading the source keeps the
    /// constraint from silently regressing.
    #[test]
    fn primary_path_does_not_walk_cells() {
        let src = include_str!("render.rs");
        // Isolate `render_once`'s body.
        let start = src
            .find("fn render_once<T: OuterTerminal>")
            .expect("render_once present");
        let after = &src[start..];
        // Cut at render_cell_walk's doc block (which legitimately mentions the
        // call) — we only want render_once's own body.
        let end = after
            .find("\n/// The cell-walking **fallback**")
            .expect("render_cell_walk's doc block follows render_once");
        let body = &after[..end];
        assert!(
            !body.contains("is_wide_continuation"),
            "the primary rows_diff path must NOT call is_wide_continuation \
             (right-edge safety is vt100's, ADR-006)"
        );
        assert!(
            !body.contains(".cell("),
            "the primary rows_diff path must NOT walk cells"
        );
    }

    /// U+4E00 at the band edge does not drift the child grid's reported width — the child
    /// still believes it has exactly `W` columns regardless of the wide glyph.
    #[test]
    fn wide_glyph_does_not_drift_band_width() {
        let (w, rows) = (10u16, 4u16);
        let mut renderer = Renderer::at_margin(w, rows, 0);
        renderer
            .parser
            .process(format!("\x1b[1;{w}H{HAN}").as_bytes());
        assert_eq!(renderer.parser.screen().size(), (rows, w));
    }
}

/// Row-run self-containment at the offset (ADR-014). The pure column maths lives in
/// `rowclip`; these prove the offset behaviour on a physical-sized [`RecordingGrid`].
/// A background-only flood erases the gutter cell (empty `contents()`) while its
/// reverse-video background bleeds, so the assertions read `cell_inverse`, the only
/// readback that can witness the corruption.
#[cfg(test)]
mod rowclip_paint {
    use super::*;
    use crate::terminal::mock::{Call, MockTerminal, RecordingGrid};

    /// Band-edge blank (the Bug B gate). A full-width reverse-video row (the nvim
    /// statusline: `ESC[7m` then a row-final `ESC[K`, attributed-but-empty) painted at a
    /// centred offset. The clip rewrites the unbounded erase into a `W`-bounded fill, so
    /// the highlight reaches the band edge (`margin + W - 1` is inverse) but the first
    /// gutter column (`margin + W`) is not.
    #[test]
    fn reverse_video_row_highlight_stops_at_band_edge() {
        let (w, rows, margin, phys) = (8u16, 3u16, 6u16, 20u16);
        let (_, grid) = render_primary(b"\x1b[7m\x1b[K", w, rows, margin, phys);

        // The highlight reaches the last in-band column.
        assert!(
            grid.cell_inverse(0, margin + w - 1),
            "the reverse-video highlight must reach the band edge (col margin+W-1)"
        );
        // …and stops there: the first gutter column is untouched.
        assert!(
            !grid.cell_inverse(0, margin + w),
            "the highlight must NOT flood the gutter (col margin+W must not be inverse)"
        );
        // The gutter cell carries no content either.
        let edge = grid.cell_contents(0, margin + w);
        assert!(
            edge.is_empty() || edge == " ",
            "the band-edge gutter cell must be blank, found {edge:?}"
        );
        // Every gutter column past the edge is clean of the highlight.
        for c in (margin + w)..phys {
            assert!(
                !grid.cell_inverse(0, c),
                "gutter col {c} must not carry the statusline highlight"
            );
        }
    }

    /// No cross-row bleed (the Bug A gate). A reverse-video row painted above a
    /// default-attribute row. The reverse row leaves inverse active across the bare
    /// `move_to`; the per-row `ESC[m` reset must contain it, so the lower row's leading
    /// cells render default. Without the reset the second row inherits the stale highlight.
    #[test]
    fn no_attribute_bleed_across_rows() {
        let (w, rows, margin, phys) = (12u16, 3u16, 5u16, 24u16);
        // Row 0: reverse "BAR" filled to the edge. Row 1: an explicit reset then
        // default "hello" — so the child grid's row 1 is genuinely default-attr.
        let bytes = b"\x1b[1;1H\x1b[7mBAR\x1b[K\x1b[2;1H\x1b[mhello";
        let (_, grid) = render_primary(bytes, w, rows, margin, phys);

        // Row 0 is reverse at its leading cell (the statusline).
        assert!(grid.cell_inverse(0, margin), "row 0 leading cell is the highlight");
        // Row 1's leading cells are default — the reset contained the bleed.
        assert_eq!(grid.cell_contents(1, margin), "h", "row 1 paints its own content");
        assert!(
            !grid.cell_inverse(1, margin),
            "row 1 leading cell must be default, not the stale reverse from row 0"
        );
        assert!(
            !grid.cell_inverse(1, margin + 1),
            "the bleed must not reach the second cell of row 1 either"
        );
    }

    /// A stream that fills a `W`-wide row and wraps one glyph past it, all inside a
    /// single `process` call — the shape that makes vt100 flip the row's wrapped flag
    /// and repair it with an absolute jump back to the row's last cell. Split across
    /// two calls the same content diffs to a harmless relative move, so the single
    /// call is load-bearing.
    const WRAP_FLIP: &[u8] = b"0123456789X";
    const WRAP_FLIP_WIDTH: u16 = 10;

    /// The wide-edge fixture, recorded at 80x24 — a real stream that drives the same
    /// repair through the frame path.
    const WIDE_EDGE: &[u8] = include_bytes!("../tests/fixtures/wide-edge.cast");

    /// Every CSI in `run` that addresses an absolute screen position, as
    /// `(byte offset, row, column)` in the terminal's 1-based coordinates. `CUP`
    /// (`H`/`f`) carries both; `CHA` (`G`) names a column only, so its row reads
    /// `None`. Only CSI final bytes count, so an `H` in the row's own text is not a
    /// match.
    fn absolute_moves(run: &[u8]) -> Vec<(usize, Option<u16>, u16)> {
        let mut found = Vec::new();
        let mut i = 0;
        while i < run.len() {
            if run[i] != 0x1b {
                i += 1;
                continue;
            }
            if run.get(i + 1) != Some(&b'[') {
                i += 2;
                continue;
            }
            let mut j = i + 2;
            while j < run.len() && !(0x40..=0x7e).contains(&run[j]) {
                j += 1;
            }
            let Some(&final_byte) = run.get(j) else {
                break;
            };
            let params: Vec<u16> = run[i + 2..j]
                .split(|&b| b == b';')
                .map(|field| {
                    std::str::from_utf8(field)
                        .ok()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(1)
                })
                .collect();
            match final_byte {
                b'H' | b'f' => {
                    found.push((i, Some(params[0]), params.get(1).copied().unwrap_or(1)));
                }
                b'G' => found.push((i, None, params[0])),
                _ => {}
            }
            i = j + 1;
        }
        found
    }

    /// Every row run a paint produced, paired with the physical row it was painted
    /// at — the `WriteRow` payloads a real frame emitted, read back off the preceding
    /// `MoveTo`. Feeds `bytes` in `chunk`-sized pieces, rendering a frame per piece,
    /// so the diff sees the same boundaries a throttled PTY read would hand it.
    fn painted_runs(
        bytes: &[u8],
        width: u16,
        rows: u16,
        margin: u16,
        base_row: u16,
        chunk: usize,
    ) -> Vec<(u16, Vec<u8>)> {
        let mut renderer = Renderer::at_margin(width, rows, margin);
        renderer.base_row = base_row;
        let mut term = MockTerminal::new();
        for piece in bytes.chunks(chunk) {
            feed(&mut renderer, piece);
            render_once(&mut renderer, &mut term).unwrap();
        }
        let mut row = 0;
        let mut runs = Vec::new();
        for call in &term.calls {
            match call {
                Call::MoveTo(_, r) => row = *r,
                Call::WriteRow(bytes) => runs.push((row, bytes.clone())),
                _ => {}
            }
        }
        runs
    }

    /// The trigger is still live. vt100's soft-wrap repair is what puts an absolute
    /// `CUP` in a row run at all; if a vt100 upgrade stopped emitting it, the two
    /// tests below would keep passing while covering nothing. This asserts the raw
    /// diff — the clipper's input, before any rewrite — still carries one.
    #[test]
    fn wrap_flip_diff_still_carries_an_absolute_move() {
        let mut renderer = Renderer::at_margin(WRAP_FLIP_WIDTH, 8, 0);
        renderer.parser.process(WRAP_FLIP);
        let screen = renderer.parser.screen();
        let found = screen
            .rows_diff(renderer.prev.screen(), 0, WRAP_FLIP_WIDTH)
            .any(|run| !absolute_moves(&run).is_empty());
        assert!(
            found,
            "vt100 no longer emits an absolute move for the wrap-flip repair — the \
             physical rewrite is untested until this stream is replaced"
        );
    }

    /// No row run carries a bare `\r` or `\n`. vt100's `MoveFromTo` writes the pair
    /// when the target is the start of the next row, and both bytes are meaningless in
    /// a run defined for one physical row — the clipper's arms rewrite the `\r` to the
    /// band's column 0 and drop the `\n`, but they are unreachable while the row
    /// writers keep every move inside the row they seeded. `rows_diff` seeds each row
    /// with its own number; `rows_formatted` seeds the wrapping state but guards the
    /// cross-row move out. This pins both, so a vt100 upgrade that relaxes either
    /// fails here rather than painting into the gutter or a row below the band.
    #[test]
    fn no_row_run_carries_a_bare_carriage_return_or_line_feed() {
        for (stream, width) in [(WRAP_FLIP, WRAP_FLIP_WIDTH), (WIDE_EDGE, 80)] {
            let mut renderer = Renderer::at_margin(width, 24, 0);
            renderer.parser.process(stream);
            let screen = renderer.parser.screen();
            let runs = screen
                .rows_diff(renderer.prev.screen(), 0, width)
                .chain(screen.rows_formatted(0, width));
            // Count only the runs that carry something: `rows_formatted` yields one
            // (often empty) run per grid row whatever the stream did, so counting every
            // run would satisfy the vacuity guard without inspecting a single byte.
            let mut seen = 0usize;
            for run in runs {
                seen += usize::from(!run.is_empty());
                assert!(
                    !run.contains(&b'\r') && !run.contains(&b'\n'),
                    "vt100 now writes a bare \\r or \\n into a row run: {:?}",
                    String::from_utf8_lossy(&run)
                );
            }
            assert!(seen > 0, "no non-empty runs inspected — the pin is vacuous");
        }
    }

    /// Every absolute move a painted run carries addresses the band's own rectangle
    /// (ADR-014). A run is painted after a bare `move_to(left_margin, offset + row)`
    /// and nothing inside it re-establishes that origin, so a position left in the
    /// run's band-local coordinates would be read as a raw screen one — outside the
    /// band, and on cells the diff baseline believes are untouched, so nothing ever
    /// repaints them. What must come out is the run's own physical row and a column
    /// inside `[margin, margin + W)`, `CUP` only: a `CHA` names no row, so it could
    /// only survive from the run's own coordinates. Both the synthetic wrap-flip and
    /// the wide-edge fixture drive the repair; the chunk sizes vary where the diff
    /// boundaries fall.
    ///
    /// Both bounds have to be able to fail. The margin is wider than the band, so the
    /// allowed physical columns `[margin + 1, margin + W]` sit clear of the band-local
    /// `[1, W]` a raw move would name — at a narrower margin the two ranges overlap and
    /// a raw column slips through. The wrap-flip band is anchored below the top of the
    /// screen for the same reason: at `base_row == 0` the physical row and the grid row
    /// are the same number and the row check asserts nothing. The fixture drives the
    /// child onto the alt screen, where the band always anchors at row 0 (ADR-012), so
    /// there the column bound is the one carrying the case.
    #[test]
    fn painted_runs_address_only_the_band() {
        for (bytes, width, rows, base_row) in [
            (WRAP_FLIP, WRAP_FLIP_WIDTH, 8u16, 3u16),
            (WIDE_EDGE, 80u16, 24u16, 0u16),
        ] {
            let margin = width + 2;
            for chunk in [1usize, 7, 64, bytes.len()] {
                for (phys_row, run) in painted_runs(bytes, width, rows, margin, base_row, chunk) {
                    for (at, row1, col1) in absolute_moves(&run) {
                        let shown = String::from_utf8_lossy(&run[at..]).to_string();
                        assert_eq!(
                            row1,
                            Some(phys_row + 1),
                            "the move at byte {at} of the run painted at physical row \
                             {phys_row} (W={width}, chunk={chunk}) names another row: \
                             {shown}"
                        );
                        assert!(
                            (margin + 1..=margin + width).contains(&col1),
                            "the move at byte {at} of the run painted at physical row \
                             {phys_row} (W={width}, chunk={chunk}) leaves the band's \
                             columns [{}, {}]: {shown}",
                            margin + 1,
                            margin + width
                        );
                    }
                }
            }
        }
    }

    /// Nothing paints outside the band rectangle, at a non-zero margin AND a non-zero
    /// `base_row` (ADR-006/013). The wrap-flip repair's absolute column and row are
    /// both chosen to fall clear of the band — column 9 sits left of `margin`, row 0
    /// sits above `base_row` — so a run that addressed them raw would stamp a glyph
    /// the readback can see and the diff baseline would never clean up.
    ///
    /// The readback is a `vt100` grid, which has no deferred wrap, and the band ends
    /// well short of the screen's right edge here: this pins down where the rewrite
    /// puts a cell, not how a real terminal behaves at its last column.
    ///
    /// The row is painted under reverse video so the readback can see a
    /// background-only stamp. A cell erased or filled under an attribute holds no
    /// glyph — `contents()` stays `""` while the highlight is on screen — so reading
    /// the character alone would miss a stray cell that carries only colour, which is
    /// exactly what a mis-columned move leaves behind on an attributed row.
    #[test]
    fn wrap_repair_paints_inside_the_band_at_an_offset() {
        let (w, rows, margin, phys, base_row) = (WRAP_FLIP_WIDTH, 8u16, 12u16, 30u16, 3u16);
        let mut renderer = Renderer::at_margin(w, rows, margin);
        renderer.base_row = base_row;
        renderer
            .parser
            .process(&[b"\x1b[7m".as_slice(), WRAP_FLIP].concat());
        let mut grid = RecordingGrid::new(phys, rows);
        render_once(&mut renderer, &mut grid).unwrap();

        for row in 0..rows {
            for col in 0..phys {
                let c = grid.cell_contents(row, col);
                let inverse = grid.cell_inverse(row, col);
                if (c.is_empty() || c == " ") && !inverse {
                    continue;
                }
                assert!(
                    row >= base_row && (margin..margin + w).contains(&col),
                    "painted {c:?} (inverse {inverse}) at physical (row {row}, col \
                     {col}), outside the band rectangle rows [{base_row}, {rows}) x \
                     cols [{margin}, {})",
                    margin + w
                );
            }
        }

        // The repair's restamped glyph belongs at the band's last column on the
        // band's own first row — proof the rewrite landed it, not just that the
        // stray cell is absent.
        assert_eq!(
            grid.cell_contents(base_row, margin + w - 1),
            "9",
            "the wrap-flip repair must restamp the row's last cell in-band"
        );
    }
}

/// Primary-screen scroll emit (ADR-013): the count-based scroll-delta (from the scroll
/// tracker, robust to a burst that turns the screen over in one frame) and the emit of
/// each departed top line into the real terminal's scrollback as a scrolling stream.
/// Drives `render_once` directly against a [`MockTerminal`] (to count the per-departed
/// `Newline`) and a physical-sized [`RecordingGrid`] with scrollback (to read back the
/// scrolled-off content), so the assertions land on the real emit, not loop plumbing.
#[cfg(test)]
mod primary_scroll {
    use super::*;
    use crate::terminal::mock::{Call, MockTerminal, RecordingGrid};

    /// A renderer whose grid holds `lines` (one per row, no trailing newline so
    /// nothing scrolled yet) with `prev` and the tracker synced to that settled
    /// grid — the start state for asserting a known scroll-delta on the next frame.
    fn renderer_primed(width: u16, rows: u16, lines: &[&str]) -> Renderer {
        let mut renderer = Renderer::at_margin(width, rows, 0);
        feed(&mut renderer, lines.join("\r\n").as_bytes());
        // One render to settle `prev` and reset the tracker to the primed grid
        // (whatever scrolled while filling is drained here, not in the test frame).
        let mut sink = MockTerminal::new();
        render_once(&mut renderer, &mut sink).unwrap();
        renderer
    }

    /// Count-based scroll-delta (the ADR-007/013 obligation). Prime the grid full, then
    /// advance it by three lines in a single frame and assert all three departed lines
    /// (L0, L1, L2) reach the recorder's scrollback, in order — the count is the number
    /// of lines advanced, not one-per-frame. A one-per-frame regression would carry only
    /// one line into scrollback and fail. L3/L4 plus the fresh lines are the visible band.
    #[test]
    fn scroll_delta_is_count_not_one_per_frame() {
        let (w, rows, phys) = (20u16, 5u16, 20u16);
        // Fill the 5-row screen: L0..L4, so the primed grid is exactly full.
        let mut renderer = renderer_primed(w, rows, &["L0", "L1", "L2", "L3", "L4"]);

        // Seed the scrollback recorder with the primed band (what is on screen).
        let mut grid = RecordingGrid::with_scrollback(phys, rows, 1000);
        for (r, line) in ["L0", "L1", "L2", "L3", "L4"].iter().enumerate() {
            grid.move_to(0, r as u16).unwrap();
            grid.write_row(line.as_bytes()).unwrap();
        }

        // Advance by three lines in ONE frame: from the last primed line, scroll
        // three times (L5, L6, L7), no trailing newline. Content scrolls up by 3 →
        // L0, L1, L2 depart the top. Grid ends [L3, L4, L5, L6, L7].
        feed(&mut renderer, b"\r\nL5\r\nL6\r\nL7");
        render_once(&mut renderer, &mut grid).unwrap();

        // All three departed lines reached the recorder's scrollback, in order.
        let history = grid.scrollback_top_rows(w);
        for tag in ["L0", "L1", "L2"] {
            assert!(
                history.iter().any(|r| r.contains(tag)),
                "departed line {tag} must reach scrollback (count-based, not \
                 one-per-frame), scrollback = {history:?}"
            );
        }
        // L3/L4 stayed on screen, so they are the visible band, not scrollback.
        let visible = grid.visible_band(0, w, rows);
        assert!(
            visible.iter().any(|r| r.contains("L7")),
            "the last line must be visible in the band, got {visible:?}"
        );
        // A Newline did fire (the emit ran) — guards against a no-op emit.
        let mut counter = MockTerminal::new();
        // (Re-run the same one frame against a call recorder to confirm the emit
        // advances the terminal at all.)
        let mut r2 = renderer_primed(w, rows, &["L0", "L1", "L2", "L3", "L4"]);
        feed(&mut r2, b"\r\nL5\r\nL6\r\nL7");
        render_once(&mut r2, &mut counter).unwrap();
        assert!(
            counter.calls.contains(&Call::Newline),
            "the scroll emit must advance the terminal (at least one Newline)"
        );
    }

    /// The whole-screen-turnover burst — the case a witnessed-overlap delta drops. Prime
    /// a 4-row band full, then advance it by eight lines in a single frame so the new grid
    /// shares no row with the old — no surviving overlap to witness the scroll, the case a
    /// grid-diff delta returned 0 for. Replay the frame into a [`RecordingGrid`] with
    /// scrollback and assert every one of the eight departed lines (including ones that
    /// arrived and left within the single frame) is recoverable, in order, and the last
    /// screenful is visible.
    #[test]
    fn full_turnover_burst_reaches_scrollback() {
        let (w, rows, phys) = (20u16, 4u16, 20u16);
        let mut renderer = renderer_primed(w, rows, &["L0", "L1", "L2", "L3"]);

        // Seed the recorder with the primed band so it mirrors what is on screen.
        let mut grid = RecordingGrid::with_scrollback(phys, rows, 1000);
        for (r, line) in ["L0", "L1", "L2", "L3"].iter().enumerate() {
            grid.move_to(0, r as u16).unwrap();
            grid.write_row(line.as_bytes()).unwrap();
        }

        // EIGHT lines in ONE frame: L4..L11. The grid ends [L8,L9,L10,L11]; L0..L7
        // all departed — and L4..L7 were never on a painted grid (they scrolled
        // through within the single coalesced frame). A grid-overlap delta sees
        // [L8..L11] vs [L0..L3], no overlap, and reports 0 — losing all eight.
        feed(&mut renderer, b"\r\nL4\r\nL5\r\nL6\r\nL7\r\nL8\r\nL9\r\nL10\r\nL11");
        render_once(&mut renderer, &mut grid).unwrap();

        // The eight departed lines are recoverable from the recorder's OWN
        // scrollback (what a real terminal stores), in order.
        let history = grid.scrollback_top_rows(w);
        for tag in ["L0", "L1", "L2", "L3", "L4", "L5", "L6", "L7"] {
            assert!(
                history.iter().any(|r| r.contains(tag)),
                "departed line {tag} must reach the recorder's scrollback under a \
                 full-screen-turnover burst, scrollback = {history:?}"
            );
        }
        // The order is preserved (L0 before L7 in history).
        let pos = |t: &str| history.iter().position(|r| r.contains(t));
        assert!(
            pos("L0") < pos("L7"),
            "scrollback order must be preserved (L0 before L7), got {history:?}"
        );

        // The last screenful is visible in the band; the mid-burst departed lines
        // are NOT in the visible frame (they live in scrollback only).
        let visible = grid.visible_band(0, w, rows);
        assert!(
            visible.iter().any(|r| r.contains("L11")),
            "the last line must be visible in the band, got {visible:?}"
        );
        assert!(
            !visible.iter().any(|r| r.contains("L4")),
            "a mid-burst departed line must NOT be in the visible band, got {visible:?}"
        );
    }

    /// A still screen emits nothing. With no advance between frames the scroll-delta is
    /// zero, so no Newline — the emit only fires on a real scroll.
    #[test]
    fn no_scroll_emits_no_newline() {
        let (w, rows) = (20u16, 5u16);
        let mut renderer = renderer_primed(w, rows, &["A", "B", "C"]);

        // Re-render the SAME grid (no new bytes) — nothing scrolled.
        let mut term = MockTerminal::new();
        render_once(&mut renderer, &mut term).unwrap();
        assert_eq!(
            term.calls.iter().filter(|c| **c == Call::Newline).count(),
            0,
            "an unchanged screen scrolls nothing → zero Newlines"
        );
    }

    /// The first primary frame, filling the screen, pushes nothing into scrollback. A
    /// fresh renderer fed less than one screenful is still filling — content grows
    /// downward, nothing departs — so no Newline.
    #[test]
    fn first_frame_filling_emits_nothing() {
        let (w, rows) = (20u16, 5u16);
        let mut renderer = Renderer::at_margin(w, rows, 0);
        feed(&mut renderer, b"only line\r\n");
        let mut term = MockTerminal::new();
        render_once(&mut renderer, &mut term).unwrap();
        assert_eq!(
            term.calls.iter().filter(|c| **c == Call::Newline).count(),
            0,
            "a screen still filling must not push anything into scrollback"
        );
    }

    /// The alt screen never scrolls the outer terminal. Even when the child's alt-screen
    /// content changes between frames, no Newline is emitted — the scroll emit is
    /// primary-only, and the tracker is drained-and-dropped without advancing the terminal.
    #[test]
    fn alt_screen_emits_no_newline() {
        let (w, rows) = (20u16, 5u16);
        let mut renderer = Renderer::at_margin(w, rows, 0);
        // Enter the alt screen, fill it, render (settles `outer_alt_active`).
        feed(&mut renderer, b"\x1b[?1049h");
        for line in ["X0", "X1", "X2", "X3", "X4"] {
            feed(&mut renderer, line.as_bytes());
            feed(&mut renderer, b"\r\n");
        }
        let mut sink = MockTerminal::new();
        render_once(&mut renderer, &mut sink).unwrap();

        // Advance the alt-screen content by several lines in one frame.
        feed(&mut renderer, b"X5\r\nX6\r\nX7\r\n");
        let mut term = MockTerminal::new();
        render_once(&mut renderer, &mut term).unwrap();
        assert_eq!(
            term.calls.iter().filter(|c| **c == Call::Newline).count(),
            0,
            "the alt screen must never scroll the outer terminal (no Newline)"
        );
    }

    /// Scrolled-off lines are emitted frame after frame, and the band stays the last
    /// screenful. Drive more lines than fit the band, one per frame, into a
    /// [`RecordingGrid`] with scrollback: the early lines that scroll off the top must
    /// each be recoverable from the recorder's scrollback, and the recorder ends with the
    /// last screenful visible.
    #[test]
    fn scrolled_off_lines_are_emitted_into_the_terminal() {
        let (w, rows, phys) = (20u16, 4u16, 30u16);
        let total = 10usize;

        // Run 1 — count the departed-line emits over the run on a MockTerminal.
        let mut counter = Renderer::at_margin(w, rows, 0);
        let mut total_newlines = 0usize;
        for i in 0..total {
            feed(&mut counter, format!("line{i}").as_bytes());
            let mut term = MockTerminal::new();
            render_once(&mut counter, &mut term).unwrap();
            total_newlines += term.calls.iter().filter(|c| **c == Call::Newline).count();
            feed(&mut counter, b"\r\n");
        }
        // Lines beyond the first screenful scroll off; with a 4-row band at least
        // `total - rows` lines must have departed into scrollback.
        assert!(
            total_newlines >= total - rows as usize,
            "early lines must be emitted into scrollback: {total_newlines} departed \
             is below the {} that scrolled off",
            total - rows as usize
        );

        // Run 2 — replay the identical frames into a scrollback-enabled recorder.
        let mut painter = Renderer::at_margin(w, rows, 0);
        let mut grid = RecordingGrid::with_scrollback(phys, rows, 1000);
        for i in 0..total {
            feed(&mut painter, format!("line{i}").as_bytes());
            render_once(&mut painter, &mut grid).unwrap();
            feed(&mut painter, b"\r\n");
        }

        // The early scrolled-off lines are recoverable from the recorder's own
        // scrollback; the final line is visible in the band.
        let history = grid.scrollback_top_rows(w);
        assert!(
            history.iter().any(|r| r.contains("line0")),
            "the earliest line must reach the recorder's scrollback, got {history:?}"
        );
        let visible = grid.visible_band(0, w, rows);
        assert!(
            visible.iter().any(|r| r.contains(&format!("line{}", total - 1))),
            "the last line must be visible in the band, got {visible:?}"
        );
    }

    /// A long OSC 52 clipboard write split across a frame boundary must not invent
    /// scrolls. The tracker is re-seeded every frame, and if that re-seed drops the
    /// vte state machine mid-sequence the tail of the payload is parsed as printable
    /// text: ~2100 characters typed onto the bottom row of an 80-column screen scroll
    /// the tracker's mirror by dozens of lines that the child never scrolled. Those
    /// phantom departures reach the real terminal as a scrolling stream (one `Newline`
    /// per line), so the same bytes must produce the same emit whole or split.
    #[test]
    fn split_osc52_does_not_invent_scrolls() {
        // A clipboard write the size of a real one: `ESC ] 52 ; c ; <base64> BEL`,
        // a little over 2KB.
        let payload = "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVo=".repeat(60);
        let osc = format!("\x1b]52;c;{payload}\x07");
        let osc = osc.as_bytes();

        // Cursor parked on the bottom row of a settled 24x80 primary screen.
        let (w, rows) = (80u16, 24u16);
        let lines: Vec<String> = (0..rows).map(|i| format!("line{i}")).collect();
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();

        let newlines = |pieces: &[&[u8]]| {
            let mut renderer = renderer_primed(w, rows, &lines);
            let mut count = 0usize;
            for piece in pieces {
                feed(&mut renderer, piece);
                let mut term = MockTerminal::new();
                render_once(&mut renderer, &mut term).unwrap();
                count += term.calls.iter().filter(|c| **c == Call::Newline).count();
            }
            count
        };

        // Split near the start, mid-sequence: the introducer in one chunk, the rest
        // in the next, with a frame in between.
        let (head, tail) = osc.split_at(5);
        assert_eq!(
            newlines(&[osc]),
            0,
            "an OSC 52 write delivered whole scrolls nothing"
        );
        assert_eq!(
            newlines(&[head, tail]),
            0,
            "the same write split across a frame boundary must scroll nothing either"
        );
    }

    /// Every frame reports its own departures and no others. The tracker keeps its
    /// screen for the whole session, so its scrollback holds every line that has ever
    /// departed; counting is the growth since the last frame (ADR-013). One line
    /// scrolled per frame must emit exactly one newline per frame, however many frames
    /// have gone before.
    #[test]
    fn each_frame_counts_only_its_own_departures() {
        let (w, rows) = (20u16, 4u16);
        let mut renderer = renderer_primed(w, rows, &["a", "b", "c", "d"]);

        for i in 0..50 {
            feed(&mut renderer, format!("\r\nfill{i}").as_bytes());
            let mut term = MockTerminal::new();
            render_once(&mut renderer, &mut term).unwrap();
            let newlines = term.calls.iter().filter(|c| **c == Call::Newline).count();
            assert_eq!(
                newlines, 1,
                "frame {i} scrolled one line and must advance the terminal by one"
            );
        }
    }

    /// A child scrolling inside a scroll region departs nothing. vt100 pushes a departed
    /// row into scrollback only while no region is active, so a tracker that had
    /// forgotten the child's `DECSTBM` would report a phantom departure for every scroll
    /// inside it — and one phantom departure collapses `base_row` and pins the band to
    /// the top of the screen for the rest of the session. The region is set in the first
    /// frame and never repeated, so only a tracker that kept it across frames stays
    /// silent.
    #[test]
    fn scroll_region_departs_nothing() {
        let (w, rows) = (20u16, 4u16);
        let mut renderer = renderer_primed(w, rows, &["a", "b", "c", "d"]);

        // Region = rows 2..4 (1-based), cursor on its bottom row.
        feed(&mut renderer, b"\x1b[2;4r\x1b[4;1H");
        render_once(&mut renderer, &mut MockTerminal::new()).unwrap();

        for i in 0..20 {
            feed(&mut renderer, format!("\nin-region {i}").as_bytes());
            let mut term = MockTerminal::new();
            render_once(&mut renderer, &mut term).unwrap();
            let newlines = term.calls.iter().filter(|c| **c == Call::Newline).count();
            assert_eq!(
                newlines, 0,
                "a scroll inside the child's region leaves the outer terminal alone \
                 (frame {i})"
            );
        }
    }

    /// Counting survives a full tracker. vt100 drops the oldest line once the scrollback
    /// deque is at its cap, so the length stops growing and a naive delta would read zero
    /// for every departure after that — the band would stop scrolling the real terminal
    /// altogether. Driven at a tiny cap: the frames either side of saturation each
    /// advance the terminal by the line they scrolled.
    #[test]
    fn counting_survives_a_full_tracker() {
        let (w, rows, cap) = (20u16, 4u16, 8usize);
        let mut renderer = renderer_primed(w, rows, &["a", "b", "c", "d"]);
        renderer.set_tracker_cap(cap);

        for i in 0..(cap * 3) {
            feed(&mut renderer, format!("\r\npast-cap {i}").as_bytes());
            let mut term = MockTerminal::new();
            render_once(&mut renderer, &mut term).unwrap();
            let newlines = term.calls.iter().filter(|c| **c == Call::Newline).count();
            assert_eq!(
                newlines, 1,
                "line {i} scrolled after the tracker filled, and still has to reach \
                 the terminal's scrollback"
            );
        }
    }

    /// The frame that runs the tracker out of headroom still reports every line it
    /// scrolled. vt100 stops growing the scrollback at the cap, so a re-seed triggered
    /// only once the deque is *full* leaves the saturating frame able to report just the
    /// room it had left — the rest of its departures never reach the terminal's
    /// scrollback at all. With multi-line frames against a tiny cap, a per-frame count
    /// short of the lines fed is exactly that loss.
    #[test]
    fn a_frame_that_fills_the_tracker_still_reports_every_line() {
        let (w, rows, cap, per_frame) = (20u16, 4u16, 8usize, 5usize);
        let mut renderer = renderer_primed(w, rows, &["a", "b", "c", "d"]);
        renderer.set_tracker_cap(cap);

        for frame in 0..10 {
            let mut bytes = Vec::new();
            for i in 0..per_frame {
                bytes.extend_from_slice(format!("\r\nf{frame}-{i}").as_bytes());
            }
            feed(&mut renderer, &bytes);
            let mut term = MockTerminal::new();
            render_once(&mut renderer, &mut term).unwrap();
            let newlines = term.calls.iter().filter(|c| **c == Call::Newline).count();
            assert_eq!(
                newlines, per_frame,
                "frame {frame} scrolled {per_frame} lines and must advance the \
                 terminal by all of them"
            );
        }
    }

    /// A single frame carrying more lines than the tracker can hold empties it, and the
    /// frames after it count from scratch rather than reading zero forever.
    #[test]
    fn a_burst_past_the_cap_leaves_the_tracker_counting() {
        let (w, rows, cap) = (20u16, 4u16, 8usize);
        let mut renderer = renderer_primed(w, rows, &["a", "b", "c", "d"]);
        renderer.set_tracker_cap(cap);

        let mut burst = Vec::new();
        for i in 0..(cap * 4) {
            burst.extend_from_slice(format!("\r\nB{i}").as_bytes());
        }
        feed(&mut renderer, &burst);
        let mut term = MockTerminal::new();
        render_once(&mut renderer, &mut term).unwrap();
        let newlines = term.calls.iter().filter(|c| **c == Call::Newline).count();
        assert!(
            newlines > 0,
            "a burst past the cap still advances the terminal, got {newlines} newlines"
        );

        feed(&mut renderer, b"\r\nafter");
        let mut term = MockTerminal::new();
        render_once(&mut renderer, &mut term).unwrap();
        assert_eq!(
            term.calls.iter().filter(|c| **c == Call::Newline).count(),
            1,
            "the frame after the burst counts its own single departure"
        );
    }

    /// The tracker is on the same screen as the live parser, so the alternate screen
    /// needs no masking: vt100 gives the alt grid no scrollback, nothing departs one, and
    /// the primary count waits untouched for the child to come back. A tracker rebuilt
    /// from `contents_formatted` would sit on the primary grid throughout and count every
    /// alt-screen scroll as a departure.
    #[test]
    fn alt_screen_scrolls_depart_nothing() {
        let (w, rows) = (20u16, 4u16);
        let mut renderer = renderer_primed(w, rows, &["a", "b", "c", "d"]);

        feed(&mut renderer, b"\x1b[?1049h");
        render_once(&mut renderer, &mut MockTerminal::new()).unwrap();

        for i in 0..20 {
            feed(&mut renderer, format!("\r\nalt {i}").as_bytes());
            let mut term = MockTerminal::new();
            render_once(&mut renderer, &mut term).unwrap();
            assert_eq!(
                renderer.scroll_tracker.screen().alternate_screen(),
                renderer.parser.screen().alternate_screen(),
                "the tracker and the live parser must agree on the active screen \
                 (frame {i})"
            );
            assert_eq!(
                term.calls.iter().filter(|c| **c == Call::Newline).count(),
                0,
                "the outer terminal never scrolls under the alt screen (frame {i})"
            );
        }

        feed(&mut renderer, b"\x1b[?1049l");
        render_once(&mut renderer, &mut MockTerminal::new()).unwrap();

        feed(&mut renderer, b"\r\nback on the primary");
        let mut term = MockTerminal::new();
        render_once(&mut renderer, &mut term).unwrap();
        assert_eq!(
            term.calls.iter().filter(|c| **c == Call::Newline).count(),
            1,
            "the first primary scroll after the alt excursion counts one line, not the \
             excursion's own"
        );
    }

    /// A burst bigger than the screen reaches the terminal's scrollback whole. Every line
    /// that left the top in one frame has to sit in the tracker's scrollback to be
    /// emitted at all, so the cap has to survive the frames before it — including the
    /// re-seed the saturation path performs, which builds its mirror at the tracker's own
    /// cap because the cap travels with the cloned `Screen`.
    #[test]
    fn a_burst_bigger_than_the_screen_is_emitted_whole() {
        let (w, rows) = (20u16, 4u16);
        let burst = 300usize;
        let mut renderer = renderer_primed(w, rows, &["a", "b", "c", "d"]);

        // Several ordinary frames first, so the tracker has been re-seeded repeatedly.
        for i in 0..10 {
            feed(&mut renderer, format!("\r\nwarm{i}").as_bytes());
            render_once(&mut renderer, &mut MockTerminal::new()).unwrap();
        }

        // One frame carrying far more lines than the screen holds: every line that left
        // the top has to sit in the tracker's scrollback to be emitted at all.
        let mut bytes = Vec::new();
        for i in 0..burst {
            bytes.extend_from_slice(format!("\r\nB{i}").as_bytes());
        }
        feed(&mut renderer, &bytes);
        let mut term = MockTerminal::new();
        render_once(&mut renderer, &mut term).unwrap();

        let newlines = term.calls.iter().filter(|c| **c == Call::Newline).count();
        assert!(
            newlines >= burst,
            "every departed line of a {burst}-line burst must be emitted (the cap held \
             through the re-seed), got {newlines} Newlines"
        );
    }

    /// The tracker's parser lives for the whole session, so nothing it buffers may grow
    /// without bound. It sees the child's device queries and answers them into a reply
    /// buffer nobody writes to the PTY, so that buffer has to be emptied every frame.
    #[test]
    fn tracker_replies_do_not_accumulate() {
        let mut renderer = renderer_primed(20, 4, &["a", "b", "c", "d"]);

        for _ in 0..100 {
            feed(&mut renderer, b"\x1b[6n");
            render_once(&mut renderer, &mut MockTerminal::new()).unwrap();
        }

        // The alternate screen leaves nothing to drain, and must not leave the replies
        // behind either.
        feed(&mut renderer, b"\x1b[?1049h");
        for _ in 0..100 {
            feed(&mut renderer, b"\x1b[6n");
            render_once(&mut renderer, &mut MockTerminal::new()).unwrap();
        }

        assert!(
            renderer
                .scroll_tracker
                .callbacks_mut()
                .drain_replies()
                .is_empty(),
            "the tracker must buffer no device-query replies between frames"
        );
    }
}

/// Inline primary-screen anchor (ADR-013): the `base_row` offset paint, the per-frame
/// make-room scroll that drives `base_row` to 0 as the band fills, the hand-off to the
/// scroll-emit engine, the alt-offset-0 invariant, and the teardown hand-back below the
/// band. Drives `render_once` / `run_teardown` directly against a [`MockTerminal`] (for
/// the offset / `Newline` counts) and a [`RecordingGrid`] with scrollback (to read back
/// the history the make-room scroll pushed off the top).
#[cfg(test)]
mod inline_anchor {
    use super::*;
    use crate::terminal::mock::{Call, MockTerminal, RecordingGrid};

    /// A margin-0 renderer anchored at `base_row` — the band launched `base_row`
    /// rows down the physical screen.
    fn renderer_at(width: u16, rows: u16, base_row: u16) -> Renderer {
        let mut r = Renderer::at_margin(width, rows, 0);
        r.base_row = base_row;
        r
    }

    fn move_rows(term: &MockTerminal) -> Vec<u16> {
        term.calls
            .iter()
            .filter_map(|c| if let Call::MoveTo(_, row) = c { Some(*row) } else { None })
            .collect()
    }

    fn last_placed_row(term: &MockTerminal) -> u16 {
        term.calls
            .iter()
            .rev()
            .find_map(|c| if let Call::PlaceCursor(_, row) = c { Some(*row) } else { None })
            .expect("a frame placed the cursor")
    }

    /// The offset paint targets `base_row + grid_row`. A band launched at row 10 paints
    /// its rows at 10, 11, 12 — never at absolute row 0, which would overpaint the
    /// scrollback above it — and the cursor tail follows to `base_row + grid_cursor_row`.
    #[test]
    fn primary_paint_offsets_rows_by_base_row() {
        let (w, rows) = (20u16, 24u16);
        let mut r = renderer_at(w, rows, 10);
        feed(&mut r, b"A\r\nB\r\nC"); // grid rows 0,1,2 — fits well within the screen
        let mut term = MockTerminal::new();
        render_once(&mut r, &mut term).unwrap();

        let rows_painted = move_rows(&term);
        for row in [10u16, 11, 12] {
            assert!(rows_painted.contains(&row), "row painted at base_row+r={row}, got {rows_painted:?}");
        }
        assert!(
            !rows_painted.contains(&0),
            "nothing paints at absolute row 0 (would overpaint history), got {rows_painted:?}"
        );
        assert_eq!(r.base_row, 10, "a band that fits never scrolls, base_row unchanged");
        assert_eq!(last_placed_row(&term), 12, "cursor tail at base_row + grid cursor row");
    }

    /// Make-room scrolls the overshoot and decrements `base_row` by the same delta. A band
    /// launched on the bottom row fits its first line with no scroll; the second line
    /// would sit one row past the bottom, so exactly one `Newline` scrolls the real
    /// terminal up and `base_row` drops by one.
    #[test]
    fn make_room_scrolls_and_decrements_base_row() {
        let (w, rows) = (20u16, 6u16);
        let mut r = renderer_at(w, rows, 5); // launched at the bottom row (5)
        feed(&mut r, b"L0");
        let mut t0 = MockTerminal::new();
        render_once(&mut r, &mut t0).unwrap();
        assert_eq!(r.base_row, 5, "the first line fits at the launch row, no make-room");
        assert_eq!(t0.calls.iter().filter(|c| **c == Call::Newline).count(), 0);

        feed(&mut r, b"\r\nL1"); // would land at physical row 6, one past the bottom
        let mut t1 = MockTerminal::new();
        render_once(&mut r, &mut t1).unwrap();
        assert_eq!(
            t1.calls.iter().filter(|c| **c == Call::Newline).count(),
            1,
            "one overshoot row scrolls the real terminal"
        );
        assert_eq!(r.base_row, 4, "base_row drops by the overshoot delta");
        // The band now paints at the decremented offset: L0 at 4, L1 at 5.
        let rows_painted = move_rows(&t1);
        assert!(rows_painted.contains(&5), "L1 lands on the bottom row, got {rows_painted:?}");
    }

    /// The make-room scroll pushes the history above the band into the real terminal's own
    /// scrollback. Seed five history rows above a band launched at the bottom, then grow
    /// the band a screenful: every seeded row reaches the recorder's scrollback and
    /// `base_row` is driven to 0.
    #[test]
    fn make_room_pushes_history_into_scrollback() {
        let (w, rows, phys) = (20u16, 6u16, 20u16);
        let mut grid = RecordingGrid::with_scrollback(phys, rows, 1000);
        for (rr, line) in ["H0", "H1", "H2", "H3", "H4"].iter().enumerate() {
            grid.move_to(0, rr as u16).unwrap();
            grid.write_row(line.as_bytes()).unwrap();
        }

        let mut r = renderer_at(w, rows, 5); // band launched at the bottom row
        for i in 0..6 {
            feed(&mut r, format!("B{i}").as_bytes());
            render_once(&mut r, &mut grid).unwrap();
            feed(&mut r, b"\r\n");
        }

        assert_eq!(r.base_row, 0, "a screenful of growth drives base_row to 0");
        let history = grid.scrollback_top_rows(w);
        for tag in ["H0", "H1", "H2", "H3", "H4"] {
            assert!(
                history.iter().any(|h| h.contains(tag)),
                "history {tag} must reach the recorder's scrollback, got {history:?}"
            );
        }
    }

    /// The `base_row → 0` transition hands over to the scroll-emit engine. Filling the
    /// screen drives `base_row` to exactly 0 via make-room (no internal vt100 scroll yet);
    /// the next line then scrolls the grid internally and the `emit_scroll_stream` branch
    /// runs — provably at `base_row == 0`.
    #[test]
    fn base_row_reaches_zero_then_scroll_emit_takes_over() {
        let (w, rows) = (20u16, 5u16);
        let mut r = renderer_at(w, rows, 4); // launched at the bottom of a 5-row screen
        for i in 0..5 {
            feed(&mut r, format!("F{i}").as_bytes());
            let mut t = MockTerminal::new();
            render_once(&mut r, &mut t).unwrap();
            // While filling, make-room scrolls but the grid never scrolls internally.
            if i < 4 {
                feed(&mut r, b"\r\n");
            }
        }
        assert_eq!(r.base_row, 0, "the grid filled — base_row driven to exactly 0");

        // One more line scrolls the grid internally → emit_scroll_stream, base_row 0.
        feed(&mut r, b"\r\nF5");
        let mut t = MockTerminal::new();
        render_once(&mut r, &mut t).unwrap();
        assert!(
            t.calls.contains(&Call::Newline),
            "the scroll-emit engine advanced the terminal once full"
        );
        assert_eq!(r.base_row, 0, "base_row stays 0 once the band fills the screen");
    }

    /// The alt paint is always at offset 0, regardless of `base_row`. A band anchored
    /// mid-screen that enters the alt screen paints its frame at row 0, not `base_row`,
    /// and `base_row` is frozen across the alt excursion (no make-room).
    #[test]
    fn alt_paint_ignores_base_row() {
        let (w, rows) = (20u16, 6u16);
        let mut r = renderer_at(w, rows, 5);
        feed(&mut r, b"\x1b[?1049h\x1b[1;1HALT");
        let mut term = MockTerminal::new();
        render_once(&mut r, &mut term).unwrap();

        let rows_painted = move_rows(&term);
        assert!(rows_painted.contains(&0), "alt paints at row 0, got {rows_painted:?}");
        assert!(
            !rows_painted.iter().any(|&row| row >= 5),
            "alt must ignore the base_row offset, got {rows_painted:?}"
        );
        assert_eq!(r.base_row, 5, "base_row is frozen while in the alt screen");
        assert_eq!(last_placed_row(&term), 0, "alt cursor tail uses offset 0");
    }

    /// Teardown hands back below the inline band, status gated on the exit code. A band
    /// anchored at row 10 with two lines hands back at the band's last physical row (11):
    /// a zero exit drops a `Newline` below it (no status); a non-zero exit rides the dim
    /// status line there instead.
    #[test]
    fn hand_back_drops_below_inline_band() {
        let (w, rows) = (20u16, 24u16);
        let mut r = renderer_at(w, rows, 10);
        feed(&mut r, b"one\r\ntwo"); // grid rows 0,1 → physical 10,11
        let mut paint = MockTerminal::new();
        render_once(&mut r, &mut paint).unwrap();
        assert!(r.ever_painted_inline, "inline content sets the hand-back gate");

        // Zero exit: a Newline below the band, no status.
        let mut t0 = MockTerminal::new();
        run_teardown(&r, &mut t0, 0).unwrap();
        assert!(
            t0.calls.contains(&Call::MoveTo(0, 11)),
            "hand-back targets the band's last physical row, calls = {:?}",
            t0.calls
        );
        assert!(t0.calls.contains(&Call::Newline), "zero exit drops a fresh line below the band");
        assert!(
            content_rows(&t0.calls).is_empty(),
            "zero exit writes no status line"
        );

        // Non-zero exit: the dim status rides below the band instead.
        let mut t1 = MockTerminal::new();
        run_teardown(&r, &mut t1, 7).unwrap();
        assert!(t1.calls.contains(&Call::MoveTo(0, 11)));
        assert_eq!(
            content_rows(&t1.calls),
            vec![b"\x1b[m\r\n\x1b[2mExited with: 7\x1b[0m".as_slice()],
            "a non-zero exit hands back the dim status line below the band"
        );
    }

    /// A coalesced scroll-then-clear launched mid-screen preserves the history above and
    /// lands at `base_row == 0` (BUG[0] regression). In one frame vt100's W-window fills
    /// and scrolls internally (so the scroll tracker holds the departed lines), then a
    /// `clear` blanks the settled grid — so the make-room overshoot reads near-zero and
    /// would leave `base_row` mid-screen while the departed branch runs. `emit_scroll_stream`
    /// paints `[0, rows)`, so running it at `base_row > 0` overwrites the pre-launch
    /// history. The departed signal proves the band reached full screen, so the frame must
    /// drive `base_row` to 0, pushing that history into scrollback first.
    #[test]
    fn coalesced_scroll_then_clear_preserves_history_at_base_row_zero() {
        let (w, rows, phys) = (20u16, 6u16, 20u16);
        let mut grid = RecordingGrid::with_scrollback(phys, rows, 1000);
        // Three history rows ABOVE a band launched at row 3.
        for (rr, line) in ["H0", "H1", "H2"].iter().enumerate() {
            grid.move_to(0, rr as u16).unwrap();
            grid.write_row(line.as_bytes()).unwrap();
        }

        let mut r = renderer_at(w, rows, 3);
        // One coalesced burst: ten lines overflow the 6-row W-window (the tracker
        // captures the departed lines), then clear+home blanks the settled grid.
        let mut burst = String::new();
        for i in 0..10 {
            burst.push_str(&format!("L{i}\r\n"));
        }
        burst.push_str("\x1b[2J\x1b[H");
        feed(&mut r, burst.as_bytes());
        render_once(&mut r, &mut grid).unwrap();

        assert_eq!(r.base_row, 0, "departed proves full screen — base_row must reach 0");
        let history = grid.scrollback_top_rows(w);
        for tag in ["H0", "H1", "H2"] {
            assert!(
                history.iter().any(|h| h.contains(tag)),
                "pre-launch history {tag} must survive in scrollback, got {history:?}"
            );
        }
    }

    /// An attribute-only row counts as live for make-room (BUG[2] regression). A
    /// full-width reverse-video status bar erased under a background SGR carries no glyphs,
    /// so a row-text read calls it blank and make-room under-scrolls, clipping the bar off
    /// the bottom. Launch on the bottom row, paint the bar on the deepest grid row, and
    /// move the cursor home so it does not itself mark the row: make-room must still scroll
    /// the bar fully on-screen (`base_row` → 0).
    #[test]
    fn attribute_only_row_counts_as_live_for_make_room() {
        let (w, rows, phys) = (8u16, 6u16, 12u16);
        let mut grid = RecordingGrid::new(phys, rows);
        let mut r = renderer_at(w, rows, 5); // launched on the bottom row
        // Reverse-video erase across the bottom grid row (attribute-only, no
        // glyphs), then reset and move the cursor home.
        feed(&mut r, b"\x1b[6;1H\x1b[7m\x1b[K\x1b[m\x1b[1;1H");
        render_once(&mut r, &mut grid).unwrap();

        assert_eq!(r.base_row, 0, "the attribute-only bar drives make-room to the top");
        assert!(
            grid.cell_inverse(rows - 1, 0),
            "the reverse-video bar lands fully on-screen at the bottom row"
        );
    }
}

/// Resize (ADR-008 / ADR-011): the SIGWINCH ordering, the proportional `--width Npct`
/// recompute, the centred-offset recompute, the gutter clear, and the stress / floor-cap
/// criteria. Drives `handle_resize` directly (every dependency injected) so the
/// intermediate-invariant assertions land in the window before the child's repaint — the
/// assertion the settled-grid test structurally cannot make.
#[cfg(test)]
mod resize {
    use super::*;
    use crate::geometry::{Layout, Width, MIN_W};
    use crate::terminal::mock::{Call, MockTerminal, RecordingGrid};
    use std::cell::RefCell;

    /// A recording [`PtyResizer`] capturing each `master.resize(cols, rows)` in order.
    /// The ADR-008 test checks what the PTY was told and where the parser ended up.
    #[derive(Default)]
    struct RecResizer {
        calls: RefCell<Vec<(u16, u16)>>,
    }
    impl PtyResizer for RecResizer {
        fn resize(&self, cols: u16, rows: u16) -> Result<(), String> {
            self.calls.borrow_mut().push((cols, rows));
            Ok(())
        }
    }

    /// Build a renderer for the resize tests with an explicit layout + width
    /// config, sized to `width × rows` in a `real_cols`-wide terminal.
    fn renderer(
        width: u16,
        rows: u16,
        real_cols: u16,
        layout: Layout,
        cfg: Width,
    ) -> Renderer {
        Renderer::new(width, rows, real_cols, layout, cfg, Box::new(std::io::sink()), 0)
    }

    /// Resize ordering (ADR-008). Drive one resize and assert `master.resize` was told
    /// the band width `W`, once, inside the one `handle_resize` invocation, and that
    /// `set_size` left the parser at `(rows, W)`. The order between the two is held by
    /// the handler body, not observed here.
    #[test]
    fn ordering_master_resize_then_set_size() {
        let mut r = renderer(80, 24, 80, Layout::Center, Width::Cols(80));
        let resizer = RecResizer::default();
        let mut term = MockTerminal::new();

        // The grid is still at the OLD size when handle_resize starts; capture it.
        assert_eq!(r.parser.screen().size(), (24, 80));
        handle_resize(&mut r, &resizer, &mut term, 100, 30);

        // master.resize recorded exactly once, with the band width W (= 80, an
        // absolute width is unchanged) and the new rows.
        assert_eq!(
            *resizer.calls.borrow(),
            vec![(80, 30)],
            "master.resize(cols=W=80, rows=30) recorded once"
        );
        // set_size ran AFTER (the grid is now at the new size). If set_size had
        // run before master.resize, the recorded resize would have observed a
        // different state — the ordering is enforced by the handler body, and
        // this asserts the post-state the ordered turn produced.
        assert_eq!(
            r.parser.screen().size(),
            (30, 80),
            "set_size left the parser at (rows=30, cols=W=80)"
        );
    }

    /// Mid-burst case A — intermediate invariant. After `set_size`, before any child
    /// repaint is fed in, the grid must be internally consistent: `COLUMNS == W`, cursor
    /// column in `[0, W)`, every row length `== W`, no panic — the degraded-but-consistent
    /// contract (ADR-008).
    #[test]
    fn mid_burst_case_a_intermediate_invariant() {
        let (w, rows, real) = (80u16, 24u16, 200u16);
        let mut r = renderer(w, rows, real, Layout::Center, Width::Cols(w));
        // Old-width content still on the grid (the in-flight backlog).
        r.parser.process(b"\x1b[1;1Hold content at old width \x1b[10;80Hedge");
        let resizer = RecResizer::default();
        let mut term = MockTerminal::new();

        // Shrink the terminal; absolute width → W stays 80, rows → 40.
        handle_resize(&mut r, &resizer, &mut term, 120, 40);

        // NO child repaint fed in yet — inspect the transient grid directly.
        let screen = r.parser.screen();
        let (grows, gcols) = screen.size();
        assert_eq!(gcols, w, "COLUMNS must equal W after set_size");
        assert_eq!(grows, 40, "rows must equal the new outer rows");
        let (_crow, ccol) = screen.cursor_position();
        assert!(ccol < w, "cursor col {ccol} must be in [0, W={w})");
        // Every row is exactly W cells (no transposition, no ragged row).
        for row in 0..grows {
            let mut count = 0u16;
            while screen.cell(row, count).is_some() {
                count += 1;
            }
            assert_eq!(count, w, "row {row} must have exactly W={w} cells");
        }
    }

    /// Mid-burst case B — settled grid. Feed `[old bytes at old W → set_size(rows, W) →
    /// new bytes at new W]` and assert the settled grid equals a reference parser fed only
    /// the post-resize stream at the new size, with `COLUMNS == W` and no panic. (The
    /// child's clear+repaint after SIGWINCH overwrites the transient grid wholesale,
    /// modelled by the new bytes starting with a clear.)
    #[test]
    fn mid_burst_case_b_settled_grid() {
        let (w, rows, real) = (60u16, 20u16, 100u16);
        let mut r = renderer(w, rows, real, Layout::Center, Width::Cols(w));
        r.parser.process(b"\x1b[1;1Hstale old-width line one\r\nstale two");
        let resizer = RecResizer::default();
        let mut term = MockTerminal::new();

        handle_resize(&mut r, &resizer, &mut term, 140, 24);

        // The child's post-SIGWINCH clear + repaint at the new size.
        let new_bytes: &[u8] = b"\x1b[2J\x1b[H\x1b[1;1Hfresh line\r\nsecond fresh line";
        r.parser.process(new_bytes);

        // Reference: a fresh parser at the new size fed only the post-resize
        // stream (the clear wipes the transient, so the settled grids match).
        let mut reference: vt100::Parser<GutterCallbacks> =
            vt100::Parser::new_with_callbacks(24, w, 0, GutterCallbacks::baseline());
        reference.process(new_bytes);

        assert_eq!(r.parser.screen().size(), (24, w), "COLUMNS == W after settle");
        let got: Vec<String> = r
            .parser
            .screen()
            .rows(0, w)
            .map(|s| s.trim_end().to_string())
            .collect();
        let want: Vec<String> = reference
            .screen()
            .rows(0, w)
            .map(|s| s.trim_end().to_string())
            .collect();
        assert_eq!(got, want, "settled grid must equal the reference");
    }

    /// Mid-burst case C — physical gutter has no stale cells. Paint a wide left-aligned
    /// frame in the alt screen, then resize so the band shrinks and the margin moves;
    /// after the gutter clear + repaint, every physical cell outside the band must be
    /// blank. Proves the explicit gutter clear (ADR-008 step 5 / ADR-016) — the
    /// `rows_diff` repaint alone touches only `[margin, margin+W)`.
    #[test]
    fn mid_burst_case_c_physical_gutter_clear() {
        let (w0, rows, phys) = (100u16, 6u16, 120u16);
        // Start left-aligned, 100-wide, so cols 0..100 carry content.
        let mut r = renderer(w0, rows, phys, Layout::Left, Width::Cols(w0));
        // Enter the alt screen (a TUI owns its viewport) and fill row 0 across the
        // whole band so a shrink would strand cells.
        let filler: String = "X".repeat(w0 as usize);
        r.parser.process(format!("\x1b[?1049h\x1b[1;1H{filler}").as_bytes());

        // First paint the wide frame into the physical grid (this enters the outer
        // alt screen on the child's edge, so `outer_alt_active` is now true).
        let mut grid = RecordingGrid::new(phys, rows);
        render_once(&mut r, &mut grid).unwrap();
        // Sanity: a cell near the right of the old band is painted.
        assert_eq!(grid.cell_contents(0, 90), "X");

        // Now resize: shrink the band to 40 columns, still left-aligned.
        r.width_config = Width::Cols(40);
        let resizer = RecResizer::default();
        handle_resize(&mut r, &resizer, &mut grid, phys, rows);
        // Repaint the (now smaller) frame.
        render_once(&mut r, &mut grid).unwrap();

        // The new band is [0, 40); everything from col 40 on must be blank —
        // the stranded "X"es from the old 100-wide band are cleared.
        for c in 40..phys {
            let s = grid.cell_contents(0, c);
            assert!(
                s.is_empty() || s == " ",
                "physical gutter cell (0, {c}) must be blank after resize, found {s:?}"
            );
        }
    }

    /// Resize stress. A scripted sequence of rapid resizes, including shrinking
    /// `real_cols` below the band width so the centred margin clamps to 0 (the
    /// `saturating_sub` path). Must never panic and always settle to a consistent grid
    /// (`COLUMNS == W`, all rows length `W`).
    #[test]
    fn resize_stress_never_panics_settles_consistent() {
        let (w, rows0) = (80u16, 24u16);
        let mut r = renderer(w, rows0, 200, Layout::Center, Width::Cols(w));
        let resizer = RecResizer::default();
        let mut term = MockTerminal::new();

        // Adversarial sequence: wide, narrow (below W → margin clamps to 0),
        // wide again, exactly W, then back and forth. (A degenerate 1-column
        // terminal is out of spec — vt100 itself can't lay a glyph in 1 column —
        // so the floor here is a realistic narrow terminal, not 1×1.)
        let seq: &[(u16, u16)] = &[
            (200, 50),
            (40, 10), // real_cols 40 < W 80 → centred margin clamps to 0
            (300, 80),
            (80, 80), // exactly W
            (120, 24),
            (24, 5), // narrow again
        ];
        for &(cols, rows) in seq {
            r.parser.process(b"some in-flight bytes\x1b[5;40Hmore");
            handle_resize(&mut r, &resizer, &mut term, cols, rows);

            // An absolute band can never be wider than the screen, so on a
            // terminal narrower than `w` the band caps at the terminal width.
            let expected_w = w.min(cols.max(1));
            let screen = r.parser.screen();
            let (grows, gcols) = screen.size();
            assert_eq!(gcols, expected_w, "W = min(W, real_cols) for an absolute width");
            assert_eq!(grows, rows, "grid rows track the outer rows");
            assert_eq!(r.width, expected_w, "renderer.width matches the resolved W");
            // Margin never exceeds the terminal and clamps to 0 when the band is
            // as wide as (or wider than) the terminal — the saturating_sub path.
            assert!(r.left_margin <= cols, "margin {} <= cols {cols}", r.left_margin);
            if cols <= expected_w {
                assert_eq!(r.left_margin, 0, "margin clamps to 0 when real_cols <= W");
            }
            // Internally consistent: every row exactly W cells.
            for row in 0..grows {
                assert!(
                    screen.cell(row, expected_w - 1).is_some(),
                    "row {row} reaches W-1"
                );
                assert!(
                    screen.cell(row, expected_w).is_none(),
                    "row {row} has no cell at W"
                );
            }
        }
    }

    /// Proportional resize (ADR-011). With `Width::Percent(50)`, drive a resize from
    /// `real_cols = 200` to `160`; assert `W` is recomputed (100 → 80), and that both
    /// `master.resize` and `set_size` used the new `W` (not `real_cols`, not the old `W`).
    /// The same resize with `Width::Cols(100)` keeps `W` at 100 — the absolute path is
    /// untouched.
    #[test]
    fn proportional_resize_tracks_width_absolute_unchanged() {
        // --- Proportional: 50% tracks the terminal. ---
        let mut r = renderer(100, 24, 200, Layout::Center, Width::Percent(50));
        assert_eq!(r.width, 100, "startup W = 50% of 200");
        let resizer = RecResizer::default();
        let mut term = MockTerminal::new();

        handle_resize(&mut r, &resizer, &mut term, 160, 24);

        assert_eq!(r.width, 80, "W recomputed to 50% of 160");
        assert_eq!(
            *resizer.calls.borrow(),
            vec![(80, 24)],
            "master.resize used the new W=80 (not real_cols=160, not old W=100)"
        );
        assert_eq!(
            r.parser.screen().size(),
            (24, 80),
            "set_size used the new W=80"
        );
        // Centred margin tracks too: (160 - 80) / 2 = 40.
        assert_eq!(r.left_margin, 40, "centred margin recomputed for the new W");

        // --- Absolute: same resize, W stays fixed, step 0 a no-op. ---
        let mut r2 = renderer(100, 24, 200, Layout::Center, Width::Cols(100));
        let resizer2 = RecResizer::default();
        let mut term2 = MockTerminal::new();
        handle_resize(&mut r2, &resizer2, &mut term2, 160, 24);
        assert_eq!(r2.width, 100, "absolute W stays 100 across the resize");
        assert_eq!(
            *resizer2.calls.borrow(),
            vec![(100, 24)],
            "master.resize used the unchanged W=100"
        );
        assert_eq!(r2.parser.screen().size(), (24, 100), "set_size used W=100");
    }

    /// Floor/cap on a proportional resize. Shrinking the terminal below the point where
    /// the percentage would yield less than `MIN_W` floors the band at `MIN_W`; a terminal
    /// narrower than `MIN_W` caps the band at the terminal — never `0`, never
    /// `> real_cols`, no panic.
    #[test]
    fn proportional_resize_floors_and_caps() {
        let mut r = renderer(100, 24, 200, Layout::Center, Width::Percent(50));
        let resizer = RecResizer::default();
        let mut term = MockTerminal::new();

        // 50% of 30 = 15 < MIN_W (20) → floors at MIN_W.
        handle_resize(&mut r, &resizer, &mut term, 30, 24);
        assert_eq!(r.width, MIN_W, "W floors at MIN_W when the percentage is small");

        // Terminal narrower than MIN_W → cap at real_cols.
        handle_resize(&mut r, &resizer, &mut term, 12, 24);
        assert_eq!(r.width, 12, "W caps at real_cols when narrower than MIN_W");
        assert!(r.width > 0, "W never collapses to zero");
    }

    /// The gutter clear is invoked with the live band geometry every resize (ADR-016:
    /// uniform across screen modes, only the row span differs). In the alt screen the
    /// span is `0..rows`, since gutter owns the whole viewport there. Proven on the
    /// `MockTerminal` call record.
    #[test]
    fn resize_clears_the_gutter() {
        let mut r = renderer(80, 24, 200, Layout::Center, Width::Cols(80));
        // The child is in the alt screen at the resize (a TUI): the clear spans
        // `0..rows`; see `resize_clears_band_row_span_preserving_history` for the
        // primary-screen span (`base_row..rows`).
        r.outer_alt_active = true;
        let resizer = RecResizer::default();
        let mut term = MockTerminal::new();
        handle_resize(&mut r, &resizer, &mut term, 120, 30);

        // (margin, width, real_cols, row_start, row_end) for the new geometry: margin =
        // (120-80)/2 = 20, W = 80, real_cols = 120, span 0..30 (alt screen).
        assert!(
            term.calls.contains(&Call::ClearGutter(20, 80, 120, 0, 30)),
            "gutter clear must run with the recomputed geometry, calls = {:?}",
            term.calls
        );
    }

    /// Primary-aware resize clears the live band's row span only (ADR-013/ADR-0017). On
    /// the primary screen a resize must clear the gutter starting at `base_row`, never at
    /// 0 (that would blank rows holding real shell history), and must still force a full
    /// band repaint so the band tracks the new margin/width. Driving `handle_resize` then
    /// one `render_once`: `ClearGutter` fires with `row_start == base_row`, and the band
    /// content is repainted in full at the new margin.
    #[test]
    fn primary_resize_clears_band_span_and_repaints() {
        // A primary-screen renderer (never entered the alt screen) with content.
        let mut r = renderer(40, 6, 100, Layout::Center, Width::Cols(40));
        r.base_row = 2; // 2 < 9, so the height clamp (min(rows - 1)) is a no-op.
        r.parser.process(b"\x1b[1;1Hbanded primary content");
        // Settle `prev` to the current grid so a plain re-render would diff to
        // nothing — the resize must be what forces the repaint below.
        let mut sink = MockTerminal::new();
        render_once(&mut r, &mut sink).unwrap();

        let resizer = RecResizer::default();
        let mut term = MockTerminal::new();
        handle_resize(&mut r, &resizer, &mut term, 120, 10);

        // The gutter clear fires, but only across the band's row span: row_start ==
        // base_row (2), never 0 — rows [0, 2) of real history are untouched.
        assert!(
            term.calls.contains(&Call::ClearGutter(40, 40, 120, 2, 10)),
            "primary resize must clear the gutter from base_row, not 0, calls = {:?}",
            term.calls
        );

        // The band still repaints in full at the new margin: render once and assert
        // the content row was re-emitted at the recomputed margin ((120-40)/2 = 40).
        let mut term2 = MockTerminal::new();
        render_once(&mut r, &mut term2).unwrap();
        let painted_band = term2.calls.iter().any(|c| {
            matches!(c, Call::WriteRow(b) if String::from_utf8_lossy(b).contains("banded"))
        });
        assert!(
            painted_band,
            "primary resize must force a full band repaint at the new geometry, \
             calls = {:?}",
            term2.calls
        );
        // The primary paint targets base_row + grid_row, so row 2 (not 0).
        let repainted_at_margin = term2.calls.contains(&Call::MoveTo(40, 2));
        assert!(
            repainted_at_margin,
            "the repainted band row must land at the recomputed margin 40, row base_row=2, \
             calls = {:?}",
            term2.calls
        );
    }

    /// Resize width-keeps / height-clamps the inline anchor (ADR-013). A width-only drag
    /// (rows unchanged) leaves `base_row` where it was. A height shrink could otherwise
    /// strand grid row 0 below the new bottom, so `base_row` is clamped back onto the new
    /// screen (`rows - 1`); the next frame's make-room finishes pushing any overshoot up.
    #[test]
    fn resize_keeps_base_row_on_width_clamps_on_height() {
        let mut r = renderer(40, 24, 100, Layout::Center, Width::Cols(40));
        r.base_row = 18; // band anchored 18 rows down a 24-row screen
        let resizer = RecResizer::default();
        let mut term = MockTerminal::new();

        // Width-only resize (rows stay 24): base_row is kept.
        handle_resize(&mut r, &resizer, &mut term, 120, 24);
        assert_eq!(r.base_row, 18, "a width-only resize keeps base_row");

        // Height shrink to 10 rows: base_row clamps onto the new screen (rows - 1).
        handle_resize(&mut r, &resizer, &mut term, 120, 10);
        assert_eq!(r.base_row, 9, "a height shrink clamps base_row to rows - 1");

        // Height grow back to 30: base_row is already on-screen, so it is kept.
        handle_resize(&mut r, &resizer, &mut term, 120, 30);
        assert_eq!(r.base_row, 9, "a height grow keeps the (already on-screen) base_row");
    }

    /// A resize that WIDENS the band must physically clear the band INTERIOR, not just
    /// the gutters. The old, narrower band's glyphs now sit inside the new band's
    /// columns, where `clear_gutter` never reaches and the blank diff baseline never
    /// repaints over (a now-blank cell yields no diff run) — so without the interior
    /// clear the stale glyphs would survive forever. Left-aligned + proportional so
    /// widening `real_cols` widens W.
    #[test]
    fn resize_widen_clears_band_interior() {
        // Old geometry: W = 50% of 80 = 40, left band [0, 40).
        let mut r = renderer(40, 24, 80, Layout::Left, Width::Percent(50));
        let resizer = RecResizer::default();
        let mut grid = RecordingGrid::new(120, 24);

        // Stale glyph a prior frame left at an interior column of the OLD band. Column
        // 25 is inside [0, 40) now and inside the widened band [0, 60) after — so
        // `clear_gutter` (only ever [W, real_cols)) can never reach it.
        grid.move_to(25, 0).unwrap();
        grid.write_row(b"X").unwrap();
        assert_eq!(grid.cell_contents(0, 25), "X");

        // Widen the terminal to 120 cols: W = 50% = 60, band [0, 60).
        handle_resize(&mut r, &resizer, &mut grid, 120, 24);

        assert_eq!(
            grid.cell_contents(0, 25),
            "",
            "widen must blank the band interior — a stale glyph inside the new band \
             would otherwise survive the gutter-only clear"
        );
    }

    /// The interior clear honours the ADR-017 row span: on the primary screen it starts
    /// at `base_row`, never 0, so shell history above the inline band is untouched; on
    /// the alt screen it spans the whole viewport from 0.
    #[test]
    fn resize_interior_clear_respects_screen_span() {
        let mut r = renderer(40, 24, 80, Layout::Center, Width::Cols(40));
        r.base_row = 4;
        let resizer = RecResizer::default();

        let mut term = MockTerminal::new();
        handle_resize(&mut r, &resizer, &mut term, 120, 24);
        assert!(
            term.calls.contains(&Call::ClearRowSpan(4, 24)),
            "primary interior clear must span [base_row, rows), calls = {:?}",
            term.calls
        );

        r.outer_alt_active = true;
        let mut term = MockTerminal::new();
        handle_resize(&mut r, &resizer, &mut term, 120, 24);
        assert!(
            term.calls.contains(&Call::ClearRowSpan(0, 24)),
            "alt interior clear must span the whole viewport, calls = {:?}",
            term.calls
        );
    }

    /// The in-mode grow path (`apply_resize_step`) has the same stale-inside-new-band
    /// exposure as a SIGWINCH widen, so it must clear the band interior too.
    #[test]
    fn resize_step_grow_clears_band_interior() {
        // Left band, W = 40 in a 120-col terminal; band [0, 40).
        let mut r = renderer(40, 24, 120, Layout::Left, Width::Cols(40));
        let resizer = RecResizer::default();
        let mut grid = RecordingGrid::new(120, 24);

        // Stale glyph at col 25 — inside [0, 40) now and inside the grown band [0, 60).
        grid.move_to(25, 0).unwrap();
        grid.write_row(b"X").unwrap();
        assert_eq!(grid.cell_contents(0, 25), "X");

        // Grow the band by 20 columns → W = 60, band [0, 60).
        apply_resize_step(&mut r, &resizer, &mut grid, 20);
        assert_eq!(r.width, 60, "the step grew W by 20");
        assert_eq!(
            grid.cell_contents(0, 25),
            "",
            "an in-mode grow must blank the band interior, like a SIGWINCH widen"
        );
    }

    /// The interior clear must run BEFORE `repaint_margins`'s gutter clear (and, in
    /// resize mode, the rails) — otherwise it would wipe the freshly drawn chrome.
    #[test]
    fn interior_clear_precedes_gutter_clear() {
        let mut r = renderer(40, 24, 80, Layout::Center, Width::Cols(40));
        let resizer = RecResizer::default();
        let mut term = MockTerminal::new();

        handle_resize(&mut r, &resizer, &mut term, 120, 24);

        let span = term
            .calls
            .iter()
            .position(|c| matches!(c, Call::ClearRowSpan(..)))
            .expect("the interior clear ran");
        let gutter = term
            .calls
            .iter()
            .position(|c| matches!(c, Call::ClearGutter(..)))
            .expect("the gutter clear ran");
        assert!(
            span < gutter,
            "interior clear must precede the gutter clear, calls = {:?}",
            term.calls
        );
    }
}

/// `repaint_margins` unit tests (uniform margin management, ADR-0017): the row-span
/// clear on both screen modes, the rails/readout paint, and the exit-clear gap the
/// in-band readout fallback needs. Mock/`RecordingGrid`, no threads, no real PTY.
#[cfg(test)]
mod margins {
    use super::*;
    use crate::geometry::{Layout, Width};
    use crate::terminal::mock::{Call, MockTerminal, RecordingGrid};

    /// Build a renderer for the margins tests with an explicit layout + width
    /// config, sized to `width × rows` in a `real_cols`-wide terminal.
    fn renderer(width: u16, rows: u16, real_cols: u16, layout: Layout, cfg: Width) -> Renderer {
        Renderer::new(width, rows, real_cols, layout, cfg, Box::new(std::io::sink()), 0)
    }

    /// Alt screen: the clear spans the whole grid, `0..rows` (gutter owns the whole
    /// viewport there).
    #[test]
    fn repaint_margins_alt_clears_full_span() {
        let mut r = renderer(80, 24, 120, Layout::Center, Width::Cols(80));
        r.outer_alt_active = true;
        let mut term = MockTerminal::new();

        repaint_margins(&r, &mut term, false).unwrap();

        // margin = (120-80)/2 = 20.
        assert!(
            term.calls.contains(&Call::ClearGutter(20, 80, 120, 0, 24)),
            "alt clear must span [0, rows), calls = {:?}",
            term.calls
        );
    }

    /// Primary screen: the clear starts at `base_row`, never at 0, so rows above the
    /// inline band (real shell history) are untouched.
    #[test]
    fn repaint_margins_primary_clears_from_base_row() {
        let mut r = renderer(80, 24, 120, Layout::Center, Width::Cols(80));
        r.base_row = 5;
        let mut term = MockTerminal::new();

        repaint_margins(&r, &mut term, false).unwrap();

        assert!(
            term.calls.contains(&Call::ClearGutter(20, 80, 120, 5, 24)),
            "primary clear must span [base_row, rows), calls = {:?}",
            term.calls
        );
    }

    /// Active resize mode draws the faint rails at the gutter columns adjoining the
    /// band, plus the width readout in the right gutter's bottom row.
    #[test]
    fn repaint_margins_draws_rails_when_active() {
        let r = renderer(80, 24, 120, Layout::Center, Width::Cols(80));
        let mut grid = RecordingGrid::new(120, 24);

        repaint_margins(&r, &mut grid, true).unwrap();

        // margin = 20, band_end = 100.
        assert_eq!(grid.cell_contents(0, 19), "\u{258f}", "left rail at margin - 1");
        assert_eq!(grid.cell_contents(0, 100), "\u{2595}", "right rail at band_end");
        assert!(grid.cell_dim(0, 19), "left rail must be faint");
        assert!(grid.cell_dim(0, 100), "right rail must be faint");

        // The readout ("80") right-aligns in the 20-wide right gutter, at row 23.
        assert_eq!(grid.cell_contents(23, 118), "8");
        assert_eq!(grid.cell_contents(23, 119), "0");
    }

    /// A `--left` band has no left gutter, so no left rail — only the right rail.
    #[test]
    fn repaint_margins_left_band_no_left_rail() {
        let r = renderer(80, 24, 100, Layout::Left, Width::Cols(80));
        let mut grid = RecordingGrid::new(100, 24);

        repaint_margins(&r, &mut grid, true).unwrap();

        assert_ne!(grid.cell_contents(0, 0), "\u{258f}", "no left rail on a left-aligned band");
        assert_eq!(grid.cell_contents(0, 80), "\u{2595}", "right rail still drawn at band_end");
    }

    /// The exit-clear gap (2c): a full-width band leaves the readout in its in-band
    /// fallback position, over the child's blank bottom row. `clear_gutter` never
    /// touches in-band cells, and a diff-based repaint against an unchanged child
    /// screen would never re-emit that row — so the exit clear must blank the
    /// readout's cells explicitly.
    #[test]
    fn repaint_margins_exit_blanks_in_band_readout() {
        let r = renderer(80, 24, 80, Layout::Center, Width::Cols(80));
        let mut grid = RecordingGrid::new(80, 24);

        // Enter: the readout falls back inside the band's bottom-right (no gutter).
        repaint_margins(&r, &mut grid, true).unwrap();
        assert_eq!(grid.cell_contents(23, 78), "8");
        assert_eq!(grid.cell_contents(23, 79), "0");

        // Exit: the readout span must be blanked explicitly (an actual space
        // character, not the digit — `cell_contents` reports exactly what's there).
        repaint_margins(&r, &mut grid, false).unwrap();
        assert_eq!(grid.cell_contents(23, 78), " ", "readout must be blanked on exit");
        assert_eq!(grid.cell_contents(23, 79), " ", "readout must be blanked on exit");
    }

    /// Growing the band while in mode must not strand the old rail glyphs inside
    /// the new band: a centred W=80 band in a 120-col terminal has rails at columns
    /// 19/100; growing to W=82 moves the band to `[19, 101)`, so both old rail
    /// columns land INSIDE the new band, past the reach of the new-geometry
    /// `clear_gutter`/`draw_rails`. `refresh_resize_overlay` must blank them via the
    /// `prev` snapshot.
    #[test]
    fn refresh_resize_overlay_grow_clears_stranded_rails() {
        let mut r = renderer(80, 24, 120, Layout::Center, Width::Cols(80));
        let mut grid = RecordingGrid::new(120, 24);

        // Enter at W=80: margin 20, rails at 19 (left) and 100 (right).
        repaint_margins(&r, &mut grid, true).unwrap();
        assert_eq!(grid.cell_contents(0, 19), "\u{258f}");
        assert_eq!(grid.cell_contents(0, 100), "\u{2595}");

        // Grow to W=82: new margin 19, new band [19, 101). Both old rail columns
        // are now inside the band.
        let prev = BandGeom::of(&r);
        r.width = 82;
        r.width_config = Width::Cols(82);
        r.left_margin = geometry::margin(r.layout, r.real_cols, r.width);

        refresh_resize_overlay(&r, &mut grid, Some(prev)).unwrap();

        assert_eq!(grid.cell_contents(0, 19), " ", "old left rail must not strand inside the new band");
        assert_eq!(grid.cell_contents(0, 100), " ", "old right rail must not strand inside the new band");
        // The new rails land at the new edges: margin - 1 = 18, band_end = 101.
        assert_eq!(grid.cell_contents(0, 18), "\u{258f}", "new left rail");
        assert_eq!(grid.cell_contents(0, 101), "\u{2595}", "new right rail");
    }

    /// `blank_vacated_chrome` must neutralise SGR before it paints its blanks. If a
    /// previous band frame left a reverse-video run dangling (nvim does), the blank
    /// spaces would otherwise land as an inverse block at the vacated rail columns.
    #[test]
    fn refresh_resize_overlay_grow_resets_sgr_before_blanking() {
        let mut r = renderer(80, 24, 120, Layout::Center, Width::Cols(80));
        let mut grid = RecordingGrid::new(120, 24);

        repaint_margins(&r, &mut grid, true).unwrap();

        // Simulate the prior band paint leaving reverse-video active.
        grid.write_row(b"\x1b[7m").unwrap();

        let prev = BandGeom::of(&r);
        r.width = 82;
        r.width_config = Width::Cols(82);
        r.left_margin = geometry::margin(r.layout, r.real_cols, r.width);

        refresh_resize_overlay(&r, &mut grid, Some(prev)).unwrap();

        assert!(
            !grid.cell_inverse(0, 19),
            "vacated left rail must not carry a leftover reverse-video run"
        );
        assert!(
            !grid.cell_inverse(0, 100),
            "vacated right rail must not carry a leftover reverse-video run"
        );
    }
}
