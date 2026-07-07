//! Thread 2 — the render loop, the vt100 grid, and the offset repaint.
//!
//! Owns the `vt100::Parser` exclusively (ADR-009) and the outer-terminal handle,
//! and is the only thread that writes the PTY master. Runs the fixed-deadline
//! 60fps coalescing loop (ADR-007), generic over an injectable [`Clock`] so the
//! timing tests run on virtual time.
//!
//! Each message is dispatched by [`dispatch`]: PTY bytes feed the parser, keys
//! re-encode at the child's kitty level (ADR-002/003), mouse events route through
//! the forwarding gate (ADR-005), and resize runs the ordered handler
//! (ADR-008/011).

use std::io::Write;
use std::time::Duration;

use crossterm::event::{Event, KeyEvent};

use crate::callbacks::GutterCallbacks;
use crate::clock::{Clock, Recv};
use crate::geometry::{self, Layout, Width};
use crate::keyboard::{self, KeyChord};
use crate::mouse::{MouseDecision, MouseGate};
use crate::msg::Msg;
use crate::pty::PtyResizer;
use crate::rowclip::clip_row_to_width_into;
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

/// The scroll-tracker's bounded scrollback (ADR-013). The tracker is reset to the
/// live grid each `render_once`, so this only caps a single coalesced frame's
/// advance — sized well above any realistic per-frame line count.
/// If an extreme frame exceeds it, the oldest lines fall off the tracker's bound,
/// never the live parser's memory.
const SCROLL_TRACKER_SCROLLBACK: usize = 4096;

/// The render thread's state: the parsers, the diff baseline, and the band geometry.
pub struct Renderer {
    parser: vt100::Parser<GutterCallbacks>,
    /// The previous-frame screen the `rows_diff` is computed against.
    prev: vt100::Parser<GutterCallbacks>,
    /// Scroll-off tracker (ADR-013): a second grid at the band's size, fed the same
    /// PTY bytes as `parser` but with bounded scrollback, so vt100's scroll machinery
    /// records which lines left the top of the W-window each frame — the count the
    /// live `parser` (scrollback 0) can't reconstruct once a burst scrolls past a
    /// screenful in one frame. Reset to the live grid every `render_once`
    /// ([`Renderer::drain_scrolled_off`]), so it never holds more than one frame's
    /// advance.
    scroll_tracker: vt100::Parser<GutterCallbacks>,
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
    /// each `Event::Mouse` dispatch, not cached here.
    mouse_gate: MouseGate,
    /// The reserved resize-mode enter chord (`--resize-key`, default Ctrl-\). The one
    /// key gutter ever withholds from the child, and only as the enter chord or while
    /// in the mode (PRD 0001, Feature 2).
    resize_key: KeyChord,
}

impl Renderer {
    /// Build a renderer for a `width × rows` virtual grid in a `real_cols`-wide
    /// outer terminal.
    ///
    /// `width` is the resolved initial `W`; `width_config` is kept so resize can
    /// recompute it for the proportional path (ADR-011). `outer_supports_kitty`
    /// clamps the child's kitty negotiation (ADR-003); `clipboard_out` is the OSC-52
    /// sink (ADR-004). Both ride only the live `parser` — `prev` is a diff-only
    /// baseline that runs neither the kitty nor the clipboard path. `base_row` is the
    /// launch cursor row (ADR-013).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        width: u16,
        rows: u16,
        real_cols: u16,
        layout: Layout,
        width_config: Width,
        outer_supports_kitty: bool,
        clipboard_out: Box<dyn Write + Send>,
        base_row: u16,
    ) -> Self {
        Self {
            parser: vt100::Parser::new_with_callbacks(
                rows,
                width,
                0,
                GutterCallbacks::with_clipboard(outer_supports_kitty, clipboard_out),
            ),
            prev: vt100::Parser::new_with_callbacks(
                rows,
                width,
                0,
                GutterCallbacks::new(false),
            ),
            // Mirrors the band's geometry with a bounded scrollback so vt100 records
            // the lines that scroll off the top (ADR-013). Diff-only like `prev`.
            scroll_tracker: vt100::Parser::new_with_callbacks(
                rows,
                width,
                SCROLL_TRACKER_SCROLLBACK,
                GutterCallbacks::new(false),
            ),
            width,
            width_config,
            layout,
            real_cols,
            left_margin: geometry::margin(layout, real_cols, width),
            // vt100 starts with the cursor visible; mirror that initial state.
            cursor_visible: true,
            // gutter never forces the alt screen (ADR-012); start false.
            outer_alt_active: false,
            // The inline anchor (ADR-013), clamped into the grid.
            base_row: base_row.min(rows.saturating_sub(1)),
            ever_painted_inline: false,
            pty_eof_seen: false,
            mouse_gate: MouseGate::default(),
            resize_key: KeyChord::default(),
        }
    }

    /// Override the resize-mode enter chord (from `--resize-key`). Called once at
    /// startup from `main`; tests keep the default.
    pub fn set_resize_key(&mut self, chord: KeyChord) {
        self.resize_key = chord;
    }

    /// Read-only view of the virtual screen — for the insta snapshot and the
    /// equivalence gate.
    #[allow(dead_code)]
    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
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
            false,
            Box::new(std::io::sink()),
            0,
        );
        r.left_margin = left_margin;
        r.real_cols = left_margin.saturating_add(width);
        r
    }
}

/// Apply one message to the render state. Returns the child's exit code when the
/// message is `ChildExited` (the loop then tears down and stops), else `None`.
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
) -> Option<i32>
where
    P: Write,
    R: PtyResizer,
    T: OuterTerminal,
{
    match msg {
        Msg::Pty(bytes) => {
            renderer.parser.process(&bytes);
            // Answer the child's device queries: parser.process surfaced any
            // CSI c / CSI 5 n / CSI 6 n / CSI ? u through unhandled_csi, which
            // buffered a spec-correct reply; drain it to the PTY master.
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
            None
        }
        Msg::Input(crossterm::event::Event::Key(key)) => {
            // Re-encode at the child's current kitty level (ADR-002/003). The level
            // lives on the parser's callbacks — read lock-free, same thread.
            let level: keyboard::KittyLevel = renderer.parser.callbacks().kitty_state.current();
            let bytes = keyboard::encode_key(&key, level);
            if !bytes.is_empty() {
                let _ = pty_writer.write_all(&bytes);
                let _ = pty_writer.flush();
            }
            None
        }
        Msg::Input(crossterm::event::Event::Resize(cols, rows)) => {
            // The resize handler (ADR-008/011) runs on THIS thread, the only parser
            // owner. Param-order trap: (cols, rows) here, set_size(rows, cols) inside.
            handle_resize(renderer, resizer, term, cols, rows);
            None
        }
        Msg::Input(crossterm::event::Event::Mouse(ev)) => {
            // The mouse forwarding gate (ADR-005). Read the child's (mode, encoding)
            // from the live screen FIRST: this runs after the frame's Msg::Pty bytes
            // applied, so a DECSET the child just sent is already visible. The gate
            // translates the coordinate, down-filters motion, and re-encodes SGR.
            let screen = renderer.parser.screen();
            let mode = screen.mouse_protocol_mode();
            let encoding = screen.mouse_protocol_encoding();
            match renderer
                .mouse_gate
                .forward(&ev, mode, encoding, renderer.left_margin, renderer.width)
            {
                MouseDecision::Forward(bytes) => {
                    let _ = pty_writer.write_all(&bytes);
                    let _ = pty_writer.flush();
                }
                MouseDecision::Swallow => {}
                MouseDecision::BailNonSgr => {
                    // A reporting mode with a non-SGR encoding is out of v1 scope.
                    // Fail loud rather than feed the child a malformed SGR event that
                    // would desync its mouse parser (ADR-005).
                    panic!(
                        "gutter: child negotiated an unsupported non-SGR mouse \
                         encoding; SGR 1006 is the only supported encoding (v1)"
                    );
                }
            }
            None
        }
        // Other input events (focus/paste) are swallowed here.
        Msg::Input(_) => None,
        Msg::ChildExited(status) => Some(status.exit_code() as i32),
        // PTY path reached EOF — record it so the shutdown drain knows the child's
        // final bytes have all landed (ADR-013). Never triggers shutdown.
        Msg::PtyEof => {
            renderer.pty_eof_seen = true;
            None
        }
    }
}

/// What the render loop should do with a decoded key while resize mode is / isn't
/// active. `PassThrough` is the only variant that reaches `encode_key`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyAction {
    Enter,        // the chord, not in mode → enter resize mode
    Exit,         // Esc or the chord, in mode → leave
    Step(i32),    // in mode → nudge width by this many units (±1 / ±10)
    Swallow,      // in mode, unrecognised key → consume, stay in mode
    PassThrough,  // forward to the child (the normal path)
}

/// Classify a key against the reserved chord and the current mode.
///
/// Releases are filtered FIRST: with kitty REPORT_EVENT_TYPES active on the outer
/// terminal, every press is followed by a release event — without this guard the
/// chord's own release would match again and instantly toggle the mode back
/// (enter → exit on key-up). A release is inert: swallowed in mode (without
/// refreshing idle), passed through otherwise (`encode_key` already returns empty
/// bytes for releases, so passthrough preserves today's behaviour byte-for-byte).
///
/// The chord itself fires on `Press` only — an auto-repeating held chord must not
/// toggle enter/exit every repeat. Step keys DO act on repeats (hold `h` to keep
/// shrinking). The chord is checked in BOTH states: not-in-mode it enters, in-mode
/// it exits — so a chord that happens to be a letter can never collide with an
/// in-mode command.
fn classify_key(ev: &KeyEvent, chord: KeyChord, in_mode: bool) -> KeyAction {
    use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
    if ev.kind == KeyEventKind::Release {
        return if in_mode { KeyAction::Swallow } else { KeyAction::PassThrough };
    }
    if chord.matches(ev) && ev.kind == KeyEventKind::Press {
        return if in_mode { KeyAction::Exit } else { KeyAction::Enter };
    }
    if !in_mode {
        return KeyAction::PassThrough;
    }
    // In mode: interpret the adjustment keys, swallow everything else. A step key
    // carrying CONTROL or ALT is NOT a step (Ctrl-h in mode must not resize) —
    // swallow it like any other stray key.
    if ev
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return if matches!(ev.code, KeyCode::Esc) { KeyAction::Exit } else { KeyAction::Swallow };
    }
    let shift = ev.modifiers.contains(KeyModifiers::SHIFT);
    match ev.code {
        KeyCode::Esc => KeyAction::Exit,
        KeyCode::Left | KeyCode::Char('h') if !shift => KeyAction::Step(-1),
        KeyCode::Right | KeyCode::Char('l') if !shift => KeyAction::Step(1),
        KeyCode::Char('-') => KeyAction::Step(-1),
        KeyCode::Char('+') | KeyCode::Char('=') => KeyAction::Step(1),
        KeyCode::Char('H') => KeyAction::Step(-10),
        KeyCode::Char('L') => KeyAction::Step(10),
        // Shifted forms (kitty reports `Char('H')`+SHIFT; some legacy paths report
        // SHIFT + lowercase; Shift+arrows mirror H/L for symmetry):
        KeyCode::Char('h') | KeyCode::Left if shift => KeyAction::Step(-10),
        KeyCode::Char('l') | KeyCode::Right if shift => KeyAction::Step(10),
        _ => KeyAction::Swallow,
    }
}

/// The resize handler — the ADR-008 ordering plus the ADR-011 proportional-width
/// recompute, in one render-thread turn. Recompute `W` → resize the PTY → resize the
/// parser (`set_size(rows, W)` — param order is the trap) → recompute the margin →
/// reset the diff baseline, clearing the band's row-span across both screen modes
/// (ADR-0017, generalising ADR-008 step 5's alt-only clear).
///
/// The clear is uniform now (ADR-0017): alt clears `0..rows`, primary clears
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
        let offset = if r.outer_alt_active { 0 } else { r.base_row };
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
        // Step 3 — live geometry (real_cols unchanged).
        renderer.width = w;
        renderer.left_margin = geometry::margin(renderer.layout, real, w);
    }
    // Step 4 — clear the vacated strip + (re)paint rails.
    let _ = refresh_resize_overlay(renderer, term, Some(prev));
    // Step 5 — force a full band repaint next frame.
    if changed {
        renderer.reset_prev_baseline();
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

/// Handle one live-loop message: intercept resize-mode keys BEFORE `encode_key`
/// (PRD 0001, Feature 2), else delegate to `dispatch`. Returns the child exit code
/// exactly as `dispatch` does.
fn apply_message<C, T, P, R>(
    m: Msg,
    clock: &mut C,
    renderer: &mut Renderer,
    resize: &mut ResizeCtl<C::Instant>,
    term: &mut T,
    pty_writer: &mut P,
    resizer: &R,
) -> Option<i32>
where
    C: Clock<Msg = Msg>,
    T: OuterTerminal,
    P: Write,
    R: PtyResizer,
{
    if let Msg::Input(Event::Key(key)) = &m {
        match classify_key(key, renderer.resize_key, resize.active()) {
            KeyAction::PassThrough => {} // fall through to dispatch (encode + write)
            KeyAction::Enter => {
                // Two statements, NOT `clock.deadline(clock.now(), ..)`: `deadline`
                // takes `&self` and `now` takes `&mut self`, so nesting them in one
                // expression is an E0502 overlapping borrow.
                let now = clock.now();
                resize.arm(clock.deadline(now, RESIZE_IDLE));
                let _ = enter_resize_overlay(renderer, term);
                return None; // consumed, no PTY
            }
            KeyAction::Step(delta) => {
                let now = clock.now();
                resize.arm(clock.deadline(now, RESIZE_IDLE)); // a resize key = activity
                apply_resize_step(renderer, resizer, term, delta);
                return None;
            }
            KeyAction::Exit => {
                resize.disarm();
                let _ = clear_resize_overlay(renderer, term);
                renderer.reset_prev_baseline();
                return None;
            }
            KeyAction::Swallow => {
                // Consumed but NOT counted as activity: a swallowed stray key must
                // not keep the mode alive forever (PRD: idle = "no resize key").
                return None;
            }
        }
    }
    // A terminal resize (SIGWINCH) while in mode moved the band — repaint the
    // overlay after handle_resize has updated the geometry. Capture the geometry
    // BEFORE dispatch mutates it, so a grow can blank the vacated rail columns
    // handle_resize's own (rails-blind) clear left behind.
    let was_resize = matches!(m, Msg::Input(Event::Resize(..)));
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

    // Scroll emit (ADR-013): on the primary screen, advance each line that left the
    // top of the W-window this frame into the terminal's own scrollback. The alt
    // screen never scrolls the outer terminal, so this is gated on the primary branch.
    let departed = if renderer.outer_alt_active {
        // Reset the tracker without emitting — an alt frame must drop any scroll the
        // tracker saw, never advance the outer terminal.
        renderer.drain_scrolled_off();
        Vec::new()
    } else {
        renderer.drain_scrolled_off()
    };

    // Make room as the band grows inline (ADR-013, primary only). If the deepest live
    // row would run past the bottom, scroll the real terminal up by the overshoot (a
    // newline per line, pushing history into the terminal's own scrollback) and drop
    // base_row by the same delta. Scrolling up by delta shifts every already-painted
    // row up too, so the diff-skipped rows are already in place and only changed rows
    // repaint. When the grid is full this has driven base_row to 0.
    if !renderer.outer_alt_active && renderer.base_row > 0 {
        let real_rows = renderer.parser.screen().size().0;
        // A non-empty `departed` proves vt100's W-window filled and scrolled this
        // frame, which only happens once the band spans the whole screen — so base_row
        // MUST reach 0 before the scroll-emit branch paints [0, rows), or it overwrites
        // the pre-launch history. The settled grid can read near-blank (a coalesced
        // seq … clear in one frame), so the overshoot would under-scroll; the departed
        // signal is the authority, and scrolling the full base_row drives it to 0.
        let delta = if departed.is_empty() {
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
    }

    // The primary band is offset by `base_row`; the alt screen always paints at 0.
    let offset = if renderer.outer_alt_active { 0 } else { renderer.base_row };

    if departed.is_empty() {
        // No scroll: the ordinary per-row diff paint. Only rows changed since the last
        // frame are re-emitted, at offset + row.
        let mut painted = false;
        let screen = renderer.parser.screen();
        let prev_screen = renderer.prev.screen();
        for (row, line) in screen.rows_diff(prev_screen, 0, renderer.width).enumerate() {
            if line.is_empty() {
                continue;
            }
            let row = row as u16;
            term.move_to(renderer.left_margin, offset.saturating_add(row))?;
            term.write_row(&prepare_row(&line, renderer.width))?;
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

    // Capture the cursor state from the live screen for the tail below.
    let screen = renderer.parser.screen();
    let visible = !screen.hide_cursor();
    let (crow, ccol) = screen.cursor_position();

    // Mirror DECTCEM: only emit a show/hide when the state actually changed.
    if visible != renderer.cursor_visible {
        term.set_cursor_visible(visible)?;
        renderer.cursor_visible = visible;
    }

    // Mirror DECSCUSR cursor shape: the callbacks watcher recorded any CSI Ps SP q the
    // child emitted; emit it to the outer terminal, de-duped by the watcher.
    if let Some(shape) = renderer.parser.callbacks_mut().cursor_shape.take_pending() {
        term.set_cursor_shape(&shape)?;
    }

    // Reposition the real cursor inside the band at `offset + grid_cursor_row`.
    term.place_cursor(
        geometry::physical_col(renderer.left_margin, ccol),
        offset.saturating_add(crow),
    )?;

    term.flush()?;

    // The current screen becomes the next frame's diff baseline. vt100 has no clone,
    // so re-process the formatted state into prev — a cheap in-memory replay.
    renderer.sync_prev();
    Ok(())
}

/// Make a vt100 row run self-contained within its `W`-wide, margin-offset rectangle
/// before painting (ADR-014): prepend `ESC[m` so it doesn't inherit the previous row's
/// trailing attribute across the bare `move_to`, and clip the row-final `ESC[K` to
/// column `W` so its erase can't flood the gutter.
fn prepare_row(line: &[u8], width: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(line.len() + 3);
    out.extend_from_slice(b"\x1b[m");
    clip_row_to_width_into(line, width, &mut out);
    out
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

    for (i, line) in departed.iter().chain(band.iter()).enumerate() {
        let i = i as u16;
        if i <= bottom {
            // Still filling the screen top-down — overwrite row i in place, no scroll.
            term.move_to(renderer.left_margin, i)?;
        } else {
            // Past the bottom: scroll one row into scrollback, then write at the bottom.
            term.newline()?;
            term.move_to(renderer.left_margin, bottom)?;
        }
        term.write_row(&prepare_row(line, renderer.width))?;
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

/// Repaint the band chrome after a geometry change (ADR-008 step 5, generalised).
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
    let offset = if renderer.outer_alt_active { 0 } else { renderer.base_row };
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
    /// Advance the `prev` baseline to match the current screen, so the next
    /// frame's `rows_diff` is against what was just painted. vt100 exposes no
    /// `clone`, so feed `prev` the current screen's `contents_formatted()` —
    /// a full state replay that leaves `prev` cell-identical to `parser`.
    fn sync_prev(&mut self) {
        let formatted = self.parser.screen().contents_formatted();
        // Reset prev to a blank grid of the same size before replaying, so stale cells
        // from a shrunk region don't linger.
        let (rows, cols) = self.parser.screen().size();
        self.prev = vt100::Parser::new_with_callbacks(rows, cols, 0, GutterCallbacks::new(false));
        self.prev.process(&formatted);
    }

    /// Drop the diff baseline to a blank grid of the live size, so the next rows_diff
    /// differs on every non-empty row and forces a full repaint (resize, ADR-008 step 5;
    /// the alt→primary edge). Re-seeds the scroll tracker to the live grid too, so its
    /// scroll detection stays sound across the change.
    fn reset_prev_baseline(&mut self) {
        let (rows, cols) = self.parser.screen().size();
        self.prev = vt100::Parser::new_with_callbacks(rows, cols, 0, GutterCallbacks::new(false));
        self.reset_scroll_tracker();
    }

    /// Drain the lines that left the top of the W-window this frame from the scroll
    /// tracker (ADR-013), then reset the tracker to the live grid so it starts the next
    /// frame with empty scrollback. Returns them formatted, oldest first.
    ///
    /// vt100 exposes no scrollback-length accessor, so probe it by clamping the offset
    /// to its max. At offset `k` the row `k` lines above the current top sits at grid
    /// row 0, so reading the top row at offsets `n..=1` yields the `n` departed lines in
    /// order.
    fn drain_scrolled_off(&mut self) -> Vec<Vec<u8>> {
        let width = self.width;
        // The tracker started this frame with empty scrollback, so its current length is
        // exactly the lines that departed. No length accessor — clamp to probe it.
        self.scroll_tracker.screen_mut().set_scrollback(usize::MAX);
        let n = self.scroll_tracker.screen().scrollback();

        let mut departed = Vec::with_capacity(n);
        for offset in (1..=n).rev() {
            self.scroll_tracker.screen_mut().set_scrollback(offset);
            // The top row at this offset is the next-oldest departed line.
            if let Some(line) = self.scroll_tracker.screen().rows_formatted(0, width).next() {
                departed.push(line);
            }
        }
        self.scroll_tracker.screen_mut().set_scrollback(0);

        // Reset the tracker to the live grid: empty scrollback again for the next frame.
        self.reset_scroll_tracker();
        departed
    }

    /// Re-seed the scroll tracker from the live grid, leaving its scrollback
    /// empty. Used after a drain and after a resize/baseline reset so the tracker
    /// always mirrors the live grid's content at the band's current size.
    fn reset_scroll_tracker(&mut self) {
        let formatted = self.parser.screen().contents_formatted();
        let (rows, cols) = self.parser.screen().size();
        self.scroll_tracker = vt100::Parser::new_with_callbacks(
            rows,
            cols,
            SCROLL_TRACKER_SCROLLBACK,
            GutterCallbacks::new(false),
        );
        self.scroll_tracker.process(&formatted);
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
pub fn run<C, T, P, R>(
    clock: &mut C,
    renderer: &mut Renderer,
    term: &mut T,
    pty_writer: &mut P,
    resizer: &R,
) -> Option<i32>
where
    C: Clock<Msg = Msg>,
    T: OuterTerminal,
    P: Write,
    R: PtyResizer,
{
    let mut exit_code: Option<i32> = None;
    let mut resize = ResizeCtl::inactive();

    'frames: loop {
        // Top-of-frame idle check (handles a flooding child that never lets Phase A
        // block): if the idle deadline has already passed, exit the mode and repaint
        // before doing anything else this frame.
        if let Some(dl) = resize.idle_deadline {
            if clock.now() >= dl {
                resize.disarm();
                let _ = clear_resize_overlay(renderer, term);
                renderer.reset_prev_baseline();
                let _ = render_once(renderer, term); // erase rails this frame
                continue 'frames;
            }
        }

        // --- Phase A: block for the first message (zero idle CPU when not in mode;
        // a bounded wait while in mode, so a quiet child still wakes for auto-exit) ---
        let first = if let Some(dl) = resize.idle_deadline {
            match clock.recv_until(dl) {
                Recv::Msg(m) => m,
                Recv::Timeout => {
                    // ~3 s idle elapsed.
                    resize.disarm();
                    let _ = clear_resize_overlay(renderer, term);
                    renderer.reset_prev_baseline();
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
        if let Some(code) = apply_message(first, clock, renderer, &mut resize, term, pty_writer, resizer) {
            exit_code = Some(code);
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
                        if let Some(code) =
                            apply_message(m, clock, renderer, &mut resize, term, pty_writer, resizer)
                        {
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

/// The explicit, ordered terminal restore (ADR-010), mode-aware (ADR-012/013):
/// conditional alt-leave / inline hand-back → pop kitty flags → disable mouse → show
/// cursor → disable raw mode. Each step undoes only what was actually set up.
///
/// The discriminator is the live `outer_alt_active`: a child that exits in the alt
/// screen takes the leave-alt path; one that exits inline hands back below the band. The
/// hand-back is gated on `ever_painted_inline` alone, so a TUI that dropped back to the
/// primary screen without ever painting inline (even on a non-zero exit) leaves no stray
/// status line. The exit code is consulted only inside the hand-back.
fn run_teardown<T: OuterTerminal>(
    renderer: &Renderer,
    term: &mut T,
    exit_code: i32,
) -> std::io::Result<()> {
    if renderer.outer_alt_active {
        term.leave_alt_screen()?;
    } else if renderer.ever_painted_inline {
        hand_back_inline(renderer, term, exit_code)?;
    }
    term.pop_keyboard_flags()?;
    term.disable_mouse()?;
    term.show_cursor()?;
    term.disable_raw_mode()?;
    Ok(())
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
/// are drained by the `show_cursor` flush later in `run_teardown`.
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
        term.newline()?;
    } else {
        write_exit_status(term, exit_code)?;
    }
    Ok(())
}

/// Emit the dim `Exited with: N` status line below the band, on a non-zero exit only
/// (success is silent). The leading `\r\n` drops it onto a fresh line below the band's
/// last content, scrolling the primary screen if it was at the bottom. Called only from
/// the inline hand-back (ADR-013).
fn write_exit_status<T: OuterTerminal>(term: &mut T, exit_code: i32) -> std::io::Result<()> {
    if exit_code == 0 {
        return Ok(());
    }
    let line = format!("\r\n\x1b[2mExited with: {exit_code}\x1b[0m");
    term.write_row(line.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn left_renderer(width: u16, rows: u16, outer_kitty: bool) -> Renderer {
        Renderer::new(
            width,
            rows,
            width, // real_cols == width → margin 0 for both Left and Center
            Layout::Left,
            Width::Cols(width),
            outer_kitty,
            Box::new(std::io::sink()),
            0, // base_row 0 → absolute paint, the baseline
        )
    }

    fn run_with(
        script: Vec<(u64, Msg)>,
        width: u16,
        rows: u16,
    ) -> (usize, MockTerminal, Vec<u8>, Renderer, Option<i32>) {
        let mut clock = VirtualClock::new(script);
        let mut renderer = left_renderer(width, rows, false);
        let mut term = MockTerminal::new();
        let mut pty: Vec<u8> = Vec::new();
        let resizer = NoopResizer;
        let code = run(&mut clock, &mut renderer, &mut term, &mut pty, &resizer);
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
            vt100::Parser::new_with_callbacks(24, 80, 0, GutterCallbacks::new(false));
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
        let mut renderer = left_renderer(80, 24, false);
        let mut term = MockTerminal::new();
        let mut pty: Vec<u8> = Vec::new();

        let code = run(&mut clock, &mut renderer, &mut term, &mut pty, &NoopResizer);

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
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

        // ~64KB chunk every 1ms for 200ms = a multi-MB burst; inject one key at
        // the 100ms mark (the 100th chunk).
        let chunk = vec![b'x'; 64 * 1024];
        let mut script: Vec<(u64, Msg)> = Vec::new();
        for i in 0..200 {
            if i == 100 {
                script.push((
                    0,
                    Msg::Input(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))),
                ));
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
        let mut renderer = left_renderer(80, 24, false);
        let mut term = MockTerminal::new();
        let mut pty: Vec<u8> = Vec::new();
        let code = run(&mut clock, &mut renderer, &mut term, &mut pty, &NoopResizer);

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
    /// On a non-kitty outer terminal `PopKeyboardFlags` must not fire — we only pop what
    /// we pushed (ADR-003).
    #[test]
    fn child_exit_restores_in_order_no_kitty_pop_when_not_pushed() {
        use crate::terminal::OuterTerminal;
        let mut clock = VirtualClock::new(vec![
            (0u64, Msg::Pty(b"report".to_vec())),
            (1, Msg::ChildExited(ExitStatus::with_exit_code(42))),
        ]);
        let mut renderer = left_renderer(80, 24, false);
        let mut term = MockTerminal::new();
        // Model the eager startup mouse capture main.rs performs (no kitty push
        // on this non-kitty path). `DisableMouse` must then fire in teardown.
        term.enable_mouse().unwrap();
        let mut pty: Vec<u8> = Vec::new();

        let code = run(&mut clock, &mut renderer, &mut term, &mut pty, &NoopResizer);

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
                Call::DisableMouse,
                Call::ShowCursor,
                Call::DisableRawMode,
            ],
            "restore order: NO LeaveAltScreen (never entered), NO kitty pop"
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
            status_rows.contains(&b"\r\n\x1b[2mExited with: 42\x1b[0m".as_slice()),
            "non-zero exit hands back the dim status line, got {status_rows:?}"
        );

        // …and the status slots before the remaining restore steps: it comes after
        // the inline paint (during the loop) but before DisableMouse (teardown).
        let status = term
            .calls
            .iter()
            .position(|c| matches!(c, Call::WriteRow(b) if b.starts_with(b"\r\n\x1b[2m")))
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

    /// Child-exit restore with a kitty-capable outer terminal, for a TUI in the alt
    /// screen at exit (`?1049h` then exit): the startup kitty push must be paired with a
    /// `PopKeyboardFlags` in the right slot — after the leave-alt-screen, before
    /// disable-raw-mode (ADR-010/003/012). Because the child exits in the alt screen,
    /// `LeaveAltScreen` fires and the inline hand-back is skipped, so teardown emits no
    /// hand-back rows and no status line, even on a zero exit.
    #[test]
    fn child_exit_pops_kitty_flags_when_pushed_at_startup() {
        use crate::terminal::OuterTerminal;
        let mut clock = VirtualClock::new(vec![
            // The child enters the alt screen (a TUI), then exits while still in it.
            (0u64, Msg::Pty(b"\x1b[?1049h".to_vec())),
            (1, Msg::ChildExited(ExitStatus::with_exit_code(0))),
        ]);
        let mut renderer = left_renderer(80, 24, true);
        let mut term = MockTerminal::kitty_capable();
        // Simulate the startup probe + push + eager mouse capture main.rs
        // performs, in that order (kitty push, then EnableMouse).
        assert!(term.supports_keyboard_enhancement().unwrap());
        term.push_keyboard_flags().unwrap();
        term.enable_mouse().unwrap();
        let mut pty: Vec<u8> = Vec::new();

        let code = run(&mut clock, &mut renderer, &mut term, &mut pty, &NoopResizer);

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
                Call::PopKeyboardFlags,
                Call::DisableMouse,
                Call::ShowCursor,
                Call::DisableRawMode,
            ],
            "an alt-screen TUI leaves the alt screen, then pops kitty in the ADR-010 slot"
        );

        // A TUI exits in alt → the leave-alt path runs, the hand-back is skipped:
        // no hand-back write_row and no status line on the primary screen.
        assert_eq!(
            term.calls
                .iter()
                .filter(|c| matches!(c, Call::WriteRow(_)))
                .count(),
            0,
            "an alt-screen TUI replays nothing onto the primary screen"
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
        use crate::terminal::OuterTerminal;
        let mut clock = VirtualClock::new(vec![
            // The waiter's ChildExited lands first; the alt-screen bytes the child
            // wrote just before exiting are still queued behind it, then PtyEof.
            (0u64, Msg::ChildExited(ExitStatus::with_exit_code(7))),
            (0, Msg::Pty(b"\x1b[?1049h".to_vec())),
            (0, Msg::PtyEof),
        ]);
        let mut renderer = left_renderer(80, 24, true);
        let mut term = MockTerminal::kitty_capable();
        assert!(term.supports_keyboard_enhancement().unwrap());
        term.push_keyboard_flags().unwrap();
        term.enable_mouse().unwrap();
        let mut pty: Vec<u8> = Vec::new();

        let code = run(&mut clock, &mut renderer, &mut term, &mut pty, &NoopResizer);

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
        use crate::terminal::OuterTerminal;
        let grace_ms = TEARDOWN_DRAIN_GRACE.as_millis() as u64;
        let mut clock = VirtualClock::new(vec![
            (0u64, Msg::ChildExited(ExitStatus::with_exit_code(0))),
            // A straggler arriving well past the grace window: never reached.
            (grace_ms + 500, Msg::Pty(b"\x1b[?1049h".to_vec())),
        ]);
        let mut renderer = left_renderer(80, 24, true);
        let mut term = MockTerminal::kitty_capable();
        term.push_keyboard_flags().unwrap();
        let mut pty: Vec<u8> = Vec::new();

        let code = run(&mut clock, &mut renderer, &mut term, &mut pty, &NoopResizer);

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
        use crate::terminal::OuterTerminal;
        let mut clock = VirtualClock::new(vec![
            (0u64, Msg::Pty(b"\x1b[?1049h".to_vec())),
            (1, Msg::PtyEof),
            (1, Msg::ChildExited(ExitStatus::with_exit_code(0))),
        ]);
        let mut renderer = left_renderer(80, 24, true);
        let mut term = MockTerminal::kitty_capable();
        term.push_keyboard_flags().unwrap();
        let mut pty: Vec<u8> = Vec::new();

        let code = run(&mut clock, &mut renderer, &mut term, &mut pty, &NoopResizer);

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
        let mut renderer = left_renderer(80, 24, false); // outer_alt_active == false
        renderer.ever_painted_inline = true; // it printed inline content this run
        let mut term = MockTerminal::new();
        run_teardown(&renderer, &mut term, 1).unwrap();

        assert!(
            !term.calls.contains(&Call::LeaveAltScreen),
            "a never-alt renderer must not leave an alt screen it never entered"
        );

        let status_rows: Vec<&[u8]> = term
            .calls
            .iter()
            .filter_map(|c| match c {
                Call::WriteRow(b) => Some(b.as_slice()),
                _ => None,
            })
            .collect();
        assert_eq!(
            status_rows,
            vec![b"\r\n\x1b[2mExited with: 1\x1b[0m".as_slice()],
            "exit_code = 1 hands back the dim status line via write_row"
        );

        let status = term
            .calls
            .iter()
            .position(|c| matches!(c, Call::WriteRow(_)))
            .unwrap();
        let show_cursor = term
            .calls
            .iter()
            .position(|c| *c == Call::ShowCursor)
            .unwrap();
        assert!(
            status < show_cursor,
            "status emission slots before the remaining restore steps"
        );

        // Zero exit: no status-line write_row at all — just a Newline below the band.
        let mut renderer0 = left_renderer(80, 24, false);
        renderer0.ever_painted_inline = true;
        let mut term0 = MockTerminal::new();
        run_teardown(&renderer0, &mut term0, 0).unwrap();
        assert_eq!(
            term0
                .calls
                .iter()
                .filter(|c| matches!(c, Call::WriteRow(_)))
                .count(),
            0,
            "exit_code = 0 hands back no status line"
        );
        assert!(
            term0.calls.contains(&Call::Newline),
            "exit_code = 0 still drops the cursor to a fresh line below the band"
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
            writes.contains(&b"\r\n\x1b[2mExited with: 5\x1b[0m".as_slice()),
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
            !term.calls[leave_idx..].iter().any(|c| matches!(c, Call::WriteRow(_))),
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
            !teardown_writes.iter().any(|w| w.starts_with(b"\r\n\x1b[2mExited with:")),
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
            !teardown_writes.iter().any(|w| w.starts_with(b"\r\n\x1b[2mExited with:")),
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
            !writes.iter().any(|w| w.starts_with(b"\r\n\x1b[2mExited with:")),
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
        let mut renderer = left_renderer(20, 5, false);
        let mut term = MockTerminal::new();
        let mut pty: Vec<u8> = Vec::new();
        run(&mut clock, &mut renderer, &mut term, &mut pty, &NoopResizer);

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
        use crossterm::event::Event;

        // --- Primary (plain) stream resized: clear starts at base_row (5), not 0. ---
        let script = vec![
            (0u64, Msg::Pty(b"primary content".to_vec())),
            // A resize arrives mid-run, BEFORE the child exits.
            (20, Msg::Input(Event::Resize(100, 30))),
            (20, Msg::ChildExited(ExitStatus::with_exit_code(0))),
        ];
        let mut clock = VirtualClock::new(script);
        let mut renderer = left_renderer(20, 24, false);
        // The scripted content sits on grid row 0, so make-room never scrolls and
        // base_row stays 5 through the resize (5 < 29, the height clamp is a no-op).
        renderer.base_row = 5;
        let mut term = MockTerminal::new();
        let mut pty: Vec<u8> = Vec::new();
        run(&mut clock, &mut renderer, &mut term, &mut pty, &NoopResizer);

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
            (20, Msg::Input(Event::Resize(100, 30))),
            (20, Msg::ChildExited(ExitStatus::with_exit_code(0))),
        ];
        let mut clock = VirtualClock::new(script);
        let mut renderer = left_renderer(20, 24, false);
        let mut term = MockTerminal::new();
        let mut pty: Vec<u8> = Vec::new();
        run(&mut clock, &mut renderer, &mut term, &mut pty, &NoopResizer);

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

    /// Through the render loop's dispatch arm: once the child has enabled kitty on its
    /// output (`CSI > 1 u` arrives as a `Msg::Pty`), a subsequent Enter and Shift+Enter
    /// re-encode at the negotiated level and reach the PTY writer as the distinct kitty
    /// `CSI 13 u` / `CSI 13 ; 2 u` sequences, with a kitty-capable outer.
    #[test]
    fn dispatch_reencodes_at_child_kitty_level() {
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

        let script = vec![
            // Child enables kitty on its output.
            (0u64, Msg::Pty(b"\x1b[>1u".to_vec())),
            // Then the user presses Enter, then Shift+Enter.
            (
                1,
                Msg::Input(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))),
            ),
            (
                1,
                Msg::Input(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT))),
            ),
            (1, Msg::ChildExited(ExitStatus::with_exit_code(0))),
        ];
        let mut clock = VirtualClock::new(script);
        // Kitty-capable outer: the child's enable is honoured (not clamped).
        let mut renderer = left_renderer(80, 24, true);
        let mut term = MockTerminal::kitty_capable();
        let mut pty: Vec<u8> = Vec::new();
        run(&mut clock, &mut renderer, &mut term, &mut pty, &NoopResizer);

        assert_eq!(
            pty, b"\x1b[13u\x1b[13;2u",
            "Enter → CSI 13 u, Shift+Enter → CSI 13 ; 2 u, distinct"
        );
    }

    /// With the outer terminal unable to source kitty (`outer_supports_kitty = false`),
    /// the child's `CSI > 1 u` enable is clamped to a no-op, so Enter and Shift+Enter both
    /// degrade to the same legacy byte (`\r`).
    #[test]
    fn dispatch_clamps_to_legacy_when_outer_unsupported() {
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

        let script = vec![
            (0u64, Msg::Pty(b"\x1b[>1u".to_vec())),
            (
                1,
                Msg::Input(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))),
            ),
            (
                1,
                Msg::Input(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT))),
            ),
            (1, Msg::ChildExited(ExitStatus::with_exit_code(0))),
        ];
        let mut clock = VirtualClock::new(script);
        // Non-kitty outer: the child's enable is neutralised.
        let mut renderer = left_renderer(80, 24, false);
        let mut term = MockTerminal::new();
        let mut pty: Vec<u8> = Vec::new();
        run(&mut clock, &mut renderer, &mut term, &mut pty, &NoopResizer);

        assert_eq!(
            pty, b"\r\r",
            "both Enters degrade to the same legacy byte under the clamp"
        );
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

    // --- Mouse forwarding through the render-loop dispatch arm (ADR-005) ---
    //
    // These exercise the full Thread-2 wiring: a `Msg::Pty` carrying the child's DECSET
    // negotiation, the per-cycle poll that refreshes the gate from the live screen, then
    // a `Msg::Input(Event::Mouse)` the gate translates and re-encodes onto the PTY writer.
    // They assert on the child-received bytes, never the grid — proven without a PTY.

    use crossterm::event::{
        Event as CtEvent, MouseButton, MouseEvent, MouseEventKind,
    };

    fn mouse_ev(kind: MouseEventKind, column: u16, row: u16) -> Msg {
        Msg::Input(CtEvent::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }))
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
        run(&mut clock, &mut renderer, &mut term, &mut pty, &NoopResizer);
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
                    mouse_ev(MouseEventKind::Down(MouseButton::Left), margin + 5, 3),
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
                (1, mouse_ev(MouseEventKind::Down(MouseButton::Left), 5, 0)),
                // Right gutter (col margin+w = 70, >= band end).
                (
                    1,
                    mouse_ev(MouseEventKind::Down(MouseButton::Left), margin + w, 0),
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
                    mouse_ev(MouseEventKind::Down(MouseButton::Left), margin + 1, 0),
                ),
                // Motion mid-drag — must be dropped in PressRelease.
                (
                    1,
                    mouse_ev(MouseEventKind::Drag(MouseButton::Left), margin + 2, 0),
                ),
                (
                    1,
                    mouse_ev(MouseEventKind::Up(MouseButton::Left), margin + 2, 0),
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
                (1, mouse_ev(MouseEventKind::Moved, margin + 1, 0)),
                // Press → held.
                (
                    1,
                    mouse_ev(MouseEventKind::Down(MouseButton::Left), margin + 1, 0),
                ),
                // Drag (motion with button) → delivered.
                (
                    1,
                    mouse_ev(MouseEventKind::Drag(MouseButton::Left), margin + 2, 0),
                ),
                // Release → not held.
                (
                    1,
                    mouse_ev(MouseEventKind::Up(MouseButton::Left), margin + 2, 0),
                ),
                // Motion after release → dropped.
                (1, mouse_ev(MouseEventKind::Moved, margin + 3, 0)),
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
    /// `?1006h` → Default encoding), a click must not produce a malformed SGR event.
    /// gutter fails loud — the dispatch arm panics rather than forwarding garbage.
    #[test]
    #[should_panic(expected = "non-SGR mouse encoding")]
    fn mouse_non_sgr_encoding_panics_rather_than_forwarding_garbage() {
        // No `?1006h`, so the encoding stays Default while the mode is reporting.
        run_mouse(
            10,
            40,
            vec![
                (0u64, Msg::Pty(b"\x1b[?1000h".to_vec())),
                (1, mouse_ev(MouseEventKind::Down(MouseButton::Left), 15, 0)),
            ],
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
            vec![(1, mouse_ev(MouseEventKind::Down(MouseButton::Left), 15, 0))],
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
        run(&mut clock, &mut renderer, &mut term, &mut pty, &NoopResizer);

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
        let mut renderer = left_renderer(width, 5, false);
        let mut term = MockTerminal::new();
        let mut pty: Vec<u8> = Vec::new();
        run(&mut clock, &mut renderer, &mut term, &mut pty, &NoopResizer);

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
            vec![b"\x1b[6 q".to_vec(), b"\x1b[4 q".to_vec()],
            "each DECSCUSR change mirrored once, verbatim"
        );
    }

    /// Modal resize-mode state machine. `VirtualClock`, `MockTerminal`,
    /// `mode_renderer` and `run_with_resizer` are nested here (rather than a
    /// top-level sibling module) so the suite can reuse `VirtualClock`/`MockTerminal`,
    /// which are private to this `mod tests` — a sibling module cannot see them.
    /// `RecResizer` is a local copy of the one in the (sibling) `mod resize`, which
    /// is likewise private to that module.
    mod resize_mode {
        use super::*;
        use crossterm::event::{KeyCode, KeyEventKind, KeyEventState, KeyModifiers};
        use std::cell::RefCell;

        /// A recording [`PtyResizer`], local to this module (the sibling `mod
        /// resize`'s copy is private to it).
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

        fn mode_renderer(width: u16, rows: u16, real_cols: u16, cfg: Width) -> Renderer {
            Renderer::new(
                width,
                rows,
                real_cols,
                Layout::Center,
                cfg,
                false,
                Box::new(std::io::sink()),
                0,
            )
        }

        fn press(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
            KeyEvent { code, modifiers: mods, kind: KeyEventKind::Press, state: KeyEventState::NONE }
        }

        fn release_key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
            KeyEvent { code, modifiers: mods, kind: KeyEventKind::Release, state: KeyEventState::NONE }
        }

        /// Bundles the state `apply_message` needs so tests read as a script of
        /// `send`/`enter` calls rather than repeating six `&mut` arguments.
        struct Ctx {
            clock: VirtualClock,
            renderer: Renderer,
            resize: ResizeCtl<u64>,
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
                    term: MockTerminal::new(),
                    pty: Vec::new(),
                    resizer: RecResizer::default(),
                }
            }

            fn send(&mut self, ev: KeyEvent) -> Option<i32> {
                apply_message(
                    Msg::Input(Event::Key(ev)),
                    &mut self.clock,
                    &mut self.renderer,
                    &mut self.resize,
                    &mut self.term,
                    &mut self.pty,
                    &self.resizer,
                )
            }

            fn enter(&mut self) {
                let c = KeyChord::default();
                self.send(press(c.code, c.mods));
            }
        }

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
            let code = run(&mut clock, &mut renderer, &mut term, &mut pty, &resizer);
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
            ctx.send(press(KeyCode::Char('l'), KeyModifiers::NONE));

            assert_eq!(ctx.renderer.width, 81);
            assert_eq!(ctx.renderer.width_config, Width::Cols(81));
            assert_eq!(*ctx.resizer.calls.borrow(), vec![(81, 24)]);
            assert_eq!(ctx.renderer.parser.screen().size(), (24, 81));
        }

        #[test]
        fn h_l_jump_ten() {
            let mut ctx = Ctx::new(80, 24, 200, Width::Cols(80));
            ctx.enter();
            ctx.send(press(KeyCode::Char('L'), KeyModifiers::SHIFT));
            assert_eq!(ctx.renderer.width, 90, "L jumps by ten");
            ctx.send(press(KeyCode::Char('H'), KeyModifiers::SHIFT));
            assert_eq!(ctx.renderer.width, 80, "H jumps back by ten");
        }

        #[test]
        fn percent_unit_preserved() {
            let mut ctx = Ctx::new(100, 24, 200, Width::Percent(50));
            ctx.enter();
            ctx.send(press(KeyCode::Char('l'), KeyModifiers::NONE));
            assert_eq!(ctx.renderer.width_config, Width::Percent(51), "unit stays percentage");
            assert_eq!(ctx.renderer.width, 102, "51% of 200 = 102");
        }

        #[test]
        fn shrink_clamps_at_min_w_silently() {
            let mut ctx = Ctx::new(20, 24, 200, Width::Cols(20));
            ctx.enter();
            ctx.send(press(KeyCode::Char('h'), KeyModifiers::NONE));
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
            ctx.send(press(KeyCode::Char('l'), KeyModifiers::NONE));
            assert_eq!(ctx.renderer.width, 200, "real_cols ceiling holds");
            assert!(ctx.resizer.calls.borrow().is_empty());
        }

        #[test]
        fn esc_exits_then_keys_pass_through() {
            let mut ctx = Ctx::new(80, 24, 200, Width::Cols(80));
            ctx.enter();
            ctx.send(press(KeyCode::Esc, KeyModifiers::NONE));
            assert!(!ctx.resize.active(), "Esc exits the mode");
            ctx.send(press(KeyCode::Char('l'), KeyModifiers::NONE));
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
            ctx.send(press(KeyCode::Char('z'), KeyModifiers::NONE));
            assert!(ctx.pty.is_empty(), "an unrecognised key must not leak to the child");
            assert!(ctx.resize.active(), "mode persists after a swallowed key");
            ctx.send(press(KeyCode::Char('l'), KeyModifiers::NONE));
            assert_eq!(ctx.renderer.width, 81, "still in mode: l still steps");
        }

        #[test]
        fn chord_release_does_not_toggle() {
            let mut ctx = Ctx::new(80, 24, 200, Width::Cols(80));
            ctx.enter();
            let c = KeyChord::default();
            ctx.send(release_key(c.code, c.mods));
            assert!(ctx.resize.active(), "the chord's own release must not exit the mode");
            ctx.send(press(KeyCode::Char('l'), KeyModifiers::NONE));
            assert_eq!(ctx.renderer.width, 81, "l still steps after the release");
            assert!(ctx.pty.is_empty());
        }

        /// Mirrors `resize::resize_clears_the_gutter`: an in-mode shrink on the alt
        /// screen must still clear the gutter across `0..rows`.
        #[test]
        fn alt_screen_step_keeps_clear_gutter_parity() {
            let mut ctx = Ctx::new(100, 6, 120, Width::Cols(100));
            ctx.renderer.outer_alt_active = true;
            ctx.enter();
            ctx.send(press(KeyCode::Char('h'), KeyModifiers::NONE)); // shrink one step

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
            let c = KeyChord::default();
            let script = vec![
                (0, Msg::Input(Event::Key(press(c.code, c.mods)))), // enter at t0
                // Well past the ~3s idle window: the bounded Phase-A wait times
                // out at the deadline, exiting the mode, before this is delivered.
                (5000, Msg::Input(Event::Key(press(KeyCode::Char('l'), KeyModifiers::NONE)))),
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
            let c = KeyChord::default();
            let script = vec![
                (0, Msg::Input(Event::Key(press(c.code, c.mods)))), // enter at t0
                // A step at t0+2000 re-arms the idle deadline to ~t0+5000, not
                // ~t0+3000 — so this key, another 5s later, still finds the mode
                // active until the RE-ARMED deadline.
                (2000, Msg::Input(Event::Key(press(KeyCode::Char('l'), KeyModifiers::NONE)))),
                (5000, Msg::Input(Event::Key(press(KeyCode::Char('l'), KeyModifiers::NONE)))),
                (0, Msg::ChildExited(ExitStatus::with_exit_code(0))),
            ];
            let (pty, renderer, code, resizer, _term) =
                run_with_resizer(script, 80, 24, 200, Width::Cols(80));

            assert_eq!(code, Some(0));
            assert_eq!(renderer.width, 81, "the in-mode step before idle-exit applied");
            assert_eq!(*resizer.calls.borrow(), vec![(81, 24)], "exactly one resize, from the step");
            assert_eq!(pty, b"l", "the delayed key after idle-exit passes through to the child");
        }
    }
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
                if let Some(cell) = screen.cell(row, col) {
                    if cell.is_wide() && cell.contents() == HAN {
                        return Some((row, col));
                    }
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

    /// Feed bytes to the renderer exactly as the render loop's `Msg::Pty` dispatch does —
    /// the live parser and the scroll tracker — so the tests exercise the real per-frame
    /// scroll detection rather than a parser the tracker never saw.
    fn feed(renderer: &mut Renderer, bytes: &[u8]) {
        renderer.parser.process(bytes);
        renderer.scroll_tracker.process(bytes);
    }

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

    /// The tracker's bounded scrollback does not accumulate across frames. The tracker is
    /// reset to the live grid every `render_once`, so after many scrolling frames its
    /// scrollback length is back to zero between frames (ADR-013). Probes the tracker
    /// length directly after a render.
    #[test]
    fn tracker_scrollback_is_reset_each_frame() {
        let (w, rows) = (20u16, 4u16);
        let mut renderer = renderer_primed(w, rows, &["a", "b", "c", "d"]);

        for i in 0..50 {
            feed(&mut renderer, format!("\r\nfill{i}").as_bytes());
            let mut term = MockTerminal::new();
            render_once(&mut renderer, &mut term).unwrap();
            // After the drain+reset the tracker holds no scrollback.
            renderer.scroll_tracker.screen_mut().set_scrollback(usize::MAX);
            let len = renderer.scroll_tracker.screen().scrollback();
            renderer.scroll_tracker.screen_mut().set_scrollback(0);
            assert_eq!(
                len, 0,
                "the tracker scrollback must reset to 0 after each frame (frame {i})"
            );
        }
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

    /// Feed bytes to the live parser and the scroll tracker, exactly as the loop's
    /// `Msg::Pty` dispatch does, so the per-frame scroll detection is exercised.
    fn feed(renderer: &mut Renderer, bytes: &[u8]) {
        renderer.parser.process(bytes);
        renderer.scroll_tracker.process(bytes);
    }

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
            !t0.calls.iter().any(|c| matches!(c, Call::WriteRow(_))),
            "zero exit writes no status line"
        );

        // Non-zero exit: the dim status rides below the band instead.
        let mut t1 = MockTerminal::new();
        run_teardown(&r, &mut t1, 7).unwrap();
        assert!(t1.calls.contains(&Call::MoveTo(0, 11)));
        let writes: Vec<&[u8]> = t1
            .calls
            .iter()
            .filter_map(|c| if let Call::WriteRow(b) = c { Some(b.as_slice()) } else { None })
            .collect();
        assert_eq!(
            writes,
            vec![b"\r\n\x1b[2mExited with: 7\x1b[0m".as_slice()],
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
    /// The ADR-008 gate asserts `master.resize` preceded `set_size`.
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
        Renderer::new(
            width,
            rows,
            real_cols,
            layout,
            cfg,
            false,
            Box::new(std::io::sink()),
            0,
        )
    }

    /// Resize ordering (ADR-008 gate). Drive one resize and assert `master.resize` was
    /// recorded before `set_size` ran, both inside the one `handle_resize` invocation, and
    /// that `set_size` left the parser at the band width `W`.
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
            vt100::Parser::new_with_callbacks(24, w, 0, GutterCallbacks::new(false));
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
    /// blank. Proves the explicit gutter clear (ADR-008 step 4 / ADR-016) — the
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
        Renderer::new(width, rows, real_cols, layout, cfg, false, Box::new(std::io::sink()), 0)
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
}
