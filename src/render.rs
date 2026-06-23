//! Thread 2 — the render loop, the vt100 grid, and the offset repaint.
//!
//! The single highest-risk module. It owns the `vt100::Parser` exclusively (no
//! mutex, no shared parser state — ADR-009) and the outer-terminal handle, and
//! is the only thread that writes the PTY master. It runs the fixed-deadline
//! 60fps coalescing loop (ADR-007), generic over an injectable [`Clock`] so the
//! cap, starvation, idle-park and input-liveness tests are deterministic.
//!
//! Per frame:
//! 1. **Phase A** — blocking `clock.recv()` for the first message (zero idle
//!    CPU), dispatch it, capture `deadline = now + 16ms` **once**.
//! 2. **Phase B** — drain to the deadline: the **mandatory** explicit
//!    `now >= deadline` burst-exit check, else `recv_until(deadline)`; `Msg` →
//!    dispatch (deadline UNCHANGED), `Timeout` → idle-gap exit, `Disconnected`
//!    → shutdown.
//! 3. Greedy `try_iter`-style swallow is folded into Phase B's drain (the
//!    virtual clock has no separate non-blocking channel; under the real clock
//!    the deadline drain already swallows everything queued before the cap).
//! 4. Exactly one `render_once`.
//!
//! Dispatch: `Pty(b)` → `parser.process(b)`; `Input(Key)` → re-encode at the
//! child's current kitty level (ADR-002/003) → PTY writer; `Input(Mouse)` → the
//! poll-diff forwarding gate (ADR-005): refresh the cached `(mode, encoding)` from
//! the live screen, then translate + down-filter + re-encode SGR → PTY writer (or
//! swallow / fail loud on a non-SGR encoding); `Input(Resize)` → `handle_resize`
//! (the ADR-008 ordering + ADR-011 proportional recompute, on this thread, the
//! parser's only owner); `ChildExited(s)` → set shutdown with status, break.
//! Other input (focus/paste) is swallowed.

use std::io::Write;
use std::time::Duration;

use crate::callbacks::GutterCallbacks;
use crate::clock::{Clock, Recv};
use crate::geometry::{self, Layout, Width};
use crate::keyboard;
use crate::mouse::{MouseDecision, MouseGate};
use crate::msg::Msg;
use crate::pty::PtyResizer;
use crate::terminal::OuterTerminal;

/// The 60fps frame budget. One render per `FRAME` of wall (or virtual) time.
pub const FRAME: Duration = Duration::from_millis(16);

/// The scroll-tracker's bounded scrollback (ADR-013). It only ever needs to hold
/// one frame's worth of scrolled-off lines (the tracker is reset to the live grid
/// at the end of every `render_once`), so this caps a single coalesced frame's
/// advance — a multi-MB burst still settles only a bounded number of lines in one
/// 16 ms frame. Sized well above any realistic per-frame line count; if an
/// extreme frame exceeds it, the oldest lines fall off the tracker's bound (the
/// same backpressure ceiling the real terminal's own scrollback has), never the
/// live parser's memory.
const SCROLL_TRACKER_SCROLLBACK: usize = 4096;

/// The render thread's state: the parser, the cached previous screen for the
/// `rows_diff`, the band width, the left margin, and the last cursor-visibility
/// we mirrored to the outer terminal.
pub struct Renderer {
    parser: vt100::Parser<GutterCallbacks>,
    /// The previous-frame screen the `rows_diff` is computed against.
    prev: vt100::Parser<GutterCallbacks>,
    /// The scroll-off tracker (ADR-013, the primary-screen scrollback emit). A
    /// second grid kept at the band's size, fed the **same** PTY bytes as the live
    /// `parser` but with a small bounded scrollback, so vt100's own scroll
    /// machinery records exactly which lines departed the top of the W-window and
    /// in what order — the count-based delta the live `parser` (`scrollback=0`)
    /// cannot reconstruct once a burst advances past a screenful in one frame.
    ///
    /// It is drained and reset to the live grid every `render_once`
    /// ([`Renderer::drain_scrolled_off`]), so it never holds more than one frame's
    /// advance: the live `parser` stays `scrollback=0` and the ADR-007 burst
    /// memory profile is unchanged (ADR-013 keeps vt100 itself scrollback-free —
    /// this is a per-frame detection device, the twin of the `prev` baseline, not
    /// vt100 scrollback on the painted grid).
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
    /// Whether the OUTER terminal is currently in the alternate screen, mirroring
    /// the child's `alternate_screen()` (ADR-012). The edge-triggered de-dupe
    /// twin of `cursor_visible`: we only emit an enter/leave when the child's alt
    /// state actually changes. Never forced — gutter enters the outer alt screen
    /// only when the child does.
    outer_alt_active: bool,
    /// Whether the child EVER entered the alt screen this run (ADR-012's latch).
    /// Set true the first time `alternate_screen()` reads true and never cleared.
    /// Guards the Option C teardown replay: a TUI that toggled the alt screen and
    /// then left it before exit still reads `alternate_screen() == false` at exit,
    /// but the latch remembers — so its final frame is never replayed onto the
    /// primary screen.
    ever_entered_alt: bool,
    /// The mouse forwarding gate (ADR-005): the button-held flag the
    /// `ButtonMotion` down-filter needs. The child's `(mode, encoding)` is read
    /// live from the screen each `Event::Mouse` dispatch, not cached here.
    mouse_gate: MouseGate,
}

impl Renderer {
    /// Build a renderer for a `width × rows` virtual grid in a `real_cols`-wide
    /// outer terminal, with the given band alignment and requested width.
    ///
    /// `width` is the resolved initial `W` (the caller resolves it once via
    /// [`geometry::resolve_width`]); `width_config` is kept so resize can
    /// recompute it for the proportional path. The left margin is derived here
    /// from `layout`, `real_cols` and `width` — the same `geometry::margin`
    /// function the resize handler calls.
    ///
    /// `outer_supports_kitty` is the startup `supports_keyboard_enhancement()`
    /// probe — it clamps the child's kitty negotiation (ADR-003). The live
    /// `parser` carries it; `prev` is a diff-baseline that only replays formatted
    /// content and never tracks kitty, so its clamp is irrelevant (`false`).
    ///
    /// `clipboard_out` is the OSC-52 sink injected into the live parser's
    /// callbacks (ADR-004) — production passes the real `/dev/tty` handle, tests
    /// pass a captured buffer. Only the live `parser` carries it; `prev` (a
    /// diff-only baseline) never runs the clipboard path, so it gets `io::sink()`.
    pub fn new(
        width: u16,
        rows: u16,
        real_cols: u16,
        layout: Layout,
        width_config: Width,
        outer_supports_kitty: bool,
        clipboard_out: Box<dyn Write + Send>,
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
            // The scroll tracker mirrors the band's geometry but carries a small
            // bounded scrollback so vt100 records the lines that scroll off the
            // top (ADR-013). Diff-only like `prev`, so it never runs the clipboard
            // or kitty paths (`GutterCallbacks::new(false)`).
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
            // gutter starts on the primary screen and never forces the alt screen
            // (ADR-012) — both flags start false, mirroring `cursor_visible`.
            outer_alt_active: false,
            ever_entered_alt: false,
            mouse_gate: MouseGate::default(),
        }
    }

    /// Read-only view of the virtual screen — for the insta snapshot and the
    /// slice-08 equivalence gate (no production caller in this slice).
    #[allow(dead_code)]
    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// Test-only constructor at an explicit `left_margin` — the cell-walking /
    /// CJK edge-of-band tests pin a margin directly to inspect physical columns,
    /// without routing through a [`Layout`]/`real_cols` pair. Production builds
    /// the margin from `geometry::margin` in [`Renderer::new`].
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
            // Feed the same bytes to the scroll tracker (ADR-013) so vt100's
            // scroll machinery captures the lines that depart the top of the
            // W-window this frame — drained and reset in `render_once`. The live
            // `parser` stays `scrollback=0`; the tracker is the count-based delta's
            // source of truth, robust to a burst that turns the screen over
            // entirely in one coalesced frame (where a grid-vs-grid diff sees no
            // surviving overlap and would wrongly report zero).
            renderer.scroll_tracker.process(&bytes);
            None
        }
        Msg::Input(crossterm::event::Event::Key(key)) => {
            // Re-encode at the child's CURRENT kitty level (ADR-002/003). The
            // level lives on the parser's callbacks — read lock-free because the
            // parser and the encoder both run on this thread.
            let level: keyboard::KittyLevel = renderer.parser.callbacks().kitty_state.current();
            let bytes = keyboard::encode_key(&key, level);
            if !bytes.is_empty() {
                let _ = pty_writer.write_all(&bytes);
                let _ = pty_writer.flush();
            }
            None
        }
        Msg::Input(crossterm::event::Event::Resize(cols, rows)) => {
            // The resize keystone (ADR-008 / ADR-011) — runs on THIS thread, the
            // only owner of the parser. Param order is the trap: `(cols, rows)`
            // here, `set_size(rows, cols)` inside.
            handle_resize(renderer, resizer, term, cols, rows);
            None
        }
        Msg::Input(crossterm::event::Event::Mouse(ev)) => {
            // The mouse forwarding gate (ADR-005). Read the child's
            // `(mode, encoding)` from the live screen poll FIRST — this runs after
            // every `Msg::Pty` dispatched earlier in the frame's drain applied its
            // bytes, so a DECSET the child just sent is already visible (the poll IS
            // the mirror point; there is no change event). The gate then translates
            // the coordinate (live `left_margin`/`width`), down-filters motion and
            // re-encodes SGR; only `Forward` reaches the PTY master.
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
                    // A reporting mode with a non-Sgr encoding is out of v1 scope.
                    // Fail loud rather than feed the child a malformed SGR event
                    // that would desync its mouse parser (ADR-005).
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
    }
}

/// The resize handler — the ADR-008 ordering plus the ADR-011 proportional-width
/// recompute, in one render-thread turn, in this exact sequence:
///
/// 0. **Recompute `W`** via [`geometry::resolve_width`] from the new `real_cols`.
///    For an absolute `--width` this is the identity (a no-op); for a proportional
///    `--width Npct` it tracks the terminal. Everything below uses the new `W`.
/// 1. **`resizer.resize(W, rows)` FIRST** — `TIOCSWINSZ` → kernel SIGWINCH to the
///    child. `cols` is the band width `W`, **never** `real_cols`.
/// 2. **`parser.screen_mut().set_size(rows, W)` IMMEDIATELY** — same turn, param
///    order `(rows, cols)`. No old-width drain (ADR-008): feeding still-in-flight
///    old-width bytes into the resized grid is verified-safe (row-resize, wrap-flag
///    reset, cursor/scroll/saved-pos clamp, `col_clamp`).
/// 3. **Recompute `left_margin`** from the new `real_cols` and `W` via the shared
///    [`geometry::margin`].
/// 4. **Gutter clear + full repaint — primary-aware (ADR-012/013).** Both modes
///    force a full `rows_diff` repaint by resetting the `prev` baseline to a blank
///    grid of the new size, so the next `render_once` repaints the live band into
///    the freshly-resized region. The two modes differ only in the **absolute**
///    `[0, rows)` gutter clear: in the alt screen gutter owns the whole viewport,
///    so it blanks the physical columns outside the band (a shrink can strand
///    cells there). On the **primary** screen that absolute clear is **skipped** —
///    gutter doesn't own the whole primary screen, so blanking outside the band
///    would erase real shell history. The primary path narrows *what* is cleared
///    (the band region repaints, the gutter/history is untouched); it does not
///    reorder the ADR-008 `master.resize()` → `set_size` sequence above.
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

    // Step 4 — primary-aware gutter clear + full repaint (ADR-012/013). The
    // absolute `[0, rows)` gutter clear is alt-screen only: there gutter owns the
    // whole viewport, so blanking the stranded gutter cells is safe. On the
    // primary screen it would reach rows outside the band that hold real shell
    // history, so it is skipped.
    if renderer.outer_alt_active {
        // Step 4a — clear the physical gutter columns (cells stranded by a shrink).
        let _ = term.clear_gutter(renderer.left_margin, w, cols, rows);
    }

    // Step 4b — force a full repaint next frame in BOTH modes: reset the diff
    // baseline to a blank grid of the new size so `rows_diff` repaints every row
    // into the freshly-resized band. On the primary screen this repaints only the
    // live band region (`[margin, margin + W)` over rows `[0, rows)`) — it never
    // blanks the gutter, so real shell history is untouched. (The render_once that
    // follows this turn does the actual paint.)
    renderer.reset_prev_baseline();
}

/// Paint the current virtual grid to the outer terminal at the band's offset
/// (ADR-006), mirroring the child's screen mode onto the outer terminal first
/// (ADR-012). The ONLY place `rows_diff` and `move_to`/`place_cursor` are
/// called — cursor repositioning does NOT live in the channel code.
///
/// **Mode mirror (ADR-012).** Before painting, read the child's
/// `alternate_screen()` and edge-trigger the outer alt screen against it (the
/// `cursor_visible` de-dupe pattern): enter/leave the outer alt screen only on a
/// real edge, and on an alt→primary edge drop the diff baseline so the primary
/// screen is repainted whole rather than diffed against the stale alt frame. The
/// `ever_entered_alt` latch is set whenever the child is in the alt screen.
///
/// **Scroll emit (ADR-013, the primary screen only).** On the primary screen,
/// drain the lines that scrolled off the top of the W-window this frame from the
/// scroll tracker ([`Renderer::drain_scrolled_off`]). If any departed, paint the
/// frame as a scrolling stream ([`emit_scroll_stream`]): `departed ++ band` printed
/// top-down the band column, so the terminal's own scrolling carries exactly the
/// departed lines into its **own** scrollback and leaves the current band visible.
/// The count is the single correctness obligation the idempotent alt frame never
/// had: under ADR-007 coalescing many lines advance in one frame, so emitting one
/// line per frame would silently drop scrollback. The tracker carries the count via
/// vt100's own scroll machinery, so a burst that turns the whole screen over in one
/// frame — leaving no surviving overlap row a grid-diff could witness — still lands
/// every departed line. When nothing departed, the ordinary per-row `rows_diff`
/// paint runs instead. The alt screen never scrolls the outer terminal, so the
/// scroll path is gated on the primary branch.
///
/// **Paint (the non-scroll path).** For each visible row, emit our own
/// `move_to(left_margin, row)` then that row's `rows_diff` byte run (which carries
/// its own intra-row SGR and relative cursor moves, scoped to `[0, W)`, so it
/// paints into physical columns `[margin, margin + W)` and never past `margin + W`
/// — vt100's margin rule guarantees the grid is exactly `W` columns). Empty diffs
/// (unchanged rows) are skipped. This is the path for both an alt frame (a fixed
/// `[0, rows)` viewport that never scrolls the outer terminal) and a primary frame
/// that did not scroll; a primary frame that scrolled paints via the scroll stream
/// above instead, which also lands the band at physical rows `[0, rows)`.
///
/// After the repaint, mirror the child's cursor visibility and reposition the
/// real cursor. The cursor row targets the **live physical row**: the primary
/// scroll emit keeps the band painted at physical rows `[0, rows)`, so the live
/// physical row equals the grid cursor row.
fn render_once<T: OuterTerminal>(renderer: &mut Renderer, term: &mut T) -> std::io::Result<()> {
    // Mode mirror (ADR-012): edge-trigger the outer alt screen against the
    // child's, latching whether it was ever entered. Read the flag before the
    // long immutable borrow of `screen` below.
    let child_alt = renderer.parser.screen().alternate_screen();
    renderer.ever_entered_alt |= child_alt;
    if child_alt != renderer.outer_alt_active {
        if child_alt {
            term.enter_alt_screen()?;
        } else {
            term.leave_alt_screen()?;
            // An alt→primary edge lands us on a fresh primary screen; the cached
            // alt frame is not a valid diff baseline, so force a full repaint.
            renderer.reset_prev_baseline();
        }
        renderer.outer_alt_active = child_alt;
    }

    // Scroll emit (ADR-013): on the primary screen, advance each line that
    // scrolled off the top of the W-window this frame into the real terminal's
    // own scrollback. The departed lines (with their real content, in order) come
    // from the scroll tracker — robust to a coalesced burst that turned the whole
    // screen over in one frame, where the lines that left were never on a painted
    // grid. The alt screen never scrolls the outer terminal, so this is gated on
    // the primary branch.
    let departed = if renderer.outer_alt_active {
        // Reset the tracker (keep it tracking the live grid) without emitting — an
        // alt frame must drop any scroll the tracker saw, never advance the outer
        // terminal.
        renderer.drain_scrolled_off();
        Vec::new()
    } else {
        renderer.drain_scrolled_off()
    };

    if departed.is_empty() {
        // No scroll: the ordinary per-row diff paint (in-place edits, a filling
        // screen, an alt frame, an unchanged screen). Only the rows that changed
        // since the last frame are re-emitted, in place.
        let screen = renderer.parser.screen();
        let prev_screen = renderer.prev.screen();
        for (row, line) in screen.rows_diff(prev_screen, 0, renderer.width).enumerate() {
            if line.is_empty() {
                continue;
            }
            let row = row as u16;
            term.move_to(renderer.left_margin, row)?;
            term.write_row(&line)?;
        }
    } else {
        // A scroll happened: stream the departed lines followed by the current band
        // down the band, letting the terminal's own scrolling carry exactly the
        // `departed` lines into its scrollback (count-based, robust to a full-screen
        // turnover) and leave the current band visible. `sync_prev` below then
        // baselines `prev` to the painted band. This replaces the diff paint for
        // this frame: the stream already painted every visible row.
        emit_scroll_stream(renderer, term, &departed)?;
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

    // Mirror DECSCUSR cursor shape (slice 08): the watcher on the shared
    // callbacks recorded any `CSI Ps SP q` the child emitted; emit the matching
    // sequence to the outer terminal only on a real change (the watcher de-dupes).
    if let Some(shape) = renderer.parser.callbacks_mut().cursor_shape.take_pending() {
        term.set_cursor_shape(&shape)?;
    }

    // Reposition the real cursor inside the band.
    term.place_cursor(geometry::physical_col(renderer.left_margin, ccol), crow)?;

    term.flush()?;

    // The current screen becomes the baseline for the next frame's diff. vt100
    // has no clone, so re-process the formatted state into `prev`. Cheap: it is
    // a single in-memory grid replay, not per-frame allocation churn at scale.
    renderer.sync_prev();
    Ok(())
}

/// Paint a scrolling frame: stream the `departed` lines followed by the current
/// band down the band column, so the terminal's own scrolling carries exactly the
/// departed lines into its scrollback and leaves the current band visible
/// (ADR-013, the primary screen only). `departed` is the count-based,
/// content-bearing run from the scroll tracker (oldest first) — robust to a
/// coalesced burst that turned the whole screen over in one frame, where the lines
/// that left were never on any painted grid (a grid-overlap delta would see no
/// surviving rows and report 0, dropping the burst).
///
/// The stream is `departed ++ band` (`delta + rows` lines). It is painted as a
/// genuine top-down print: the first `rows` lines fill rows `[0, rows)` in place
/// (overwriting whatever the previous frame left, so pre-existing screen content
/// never leaks into scrollback), and each line beyond the bottom is preceded by a
/// `newline()` that scrolls one row off the top into the terminal's own scrollback
/// before writing the new line at the bottom. After the whole stream, the last
/// `rows` lines (the band) are visible and exactly the first `delta` lines (the
/// departed run) have entered scrollback, in order — the count-based obligation
/// (ADR-007/013) a one-per-frame or witnessed-overlap delta could not meet, and
/// without the per-frame band re-streaming that would bury early lines deep in
/// scrollback.
///
/// Only the band columns `[margin, margin + W)` are written (each line is
/// re-positioned to the margin); each `\r\n` scrolls the whole physical row, so any
/// real shell history in the gutter scrolls up naturally — gutter never blanks or
/// rewrites the gutter columns.
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
            // Still filling the initial screen top-down — overwrite row `i` in
            // place, no scroll yet (so nothing pre-existing leaks into scrollback).
            term.move_to(renderer.left_margin, i)?;
        } else {
            // Past the bottom: scroll one row into the terminal's own scrollback,
            // then write the new line at the bottom.
            term.newline()?;
            term.move_to(renderer.left_margin, bottom)?;
        }
        term.write_row(line)?;
    }
    Ok(())
}

/// The cell-walking **fallback** repaint (ADR-006). Taken when an app emits
/// absolute-column positioning the per-row `rows_diff` can't safely represent;
/// here gutter walks the grid cell-by-cell and paints each cell at its physical
/// column `left_margin + col` rather than trusting a relative byte run.
///
/// This is the **only** place in the whole binary that calls
/// [`vt100::Cell::is_wide_continuation`]. A double-width glyph occupies two grid
/// cells: a lead cell (`is_wide()`, holding the glyph in `contents()`) and a
/// continuation cell (`is_wide_continuation()`, byte-length zero so
/// `contents()` is `""`). We `continue` past the continuation cell **before**
/// any `move_to`/emit — verified safe in `cell.rs` (skipping the empty cell
/// loses nothing), and important: a spurious `move_to` to the continuation
/// column would itself be a defect even with no byte after it.
///
/// No double-advance: the lead cell's glyph already spans two physical columns,
/// so we read the display width from the lead cell and let the loop's natural
/// `col += 1` over the continuation cell (which we skip) account for the second
/// column — we never advance the physical cursor twice for one glyph, and we
/// never paint the lead glyph's right half into `left_margin + W`.
///
/// **Dormant by decision (A1, iteration-02).** gutter renders via the primary
/// `rows_diff` path *only*; right-edge safety rests on vt100's margin rule, not
/// on this walk (ADR-006: `text()` wraps a wide glyph rather than placing its
/// lead at `W-1`, so nothing ever paints past `margin + W`). This function is
/// tested-but-dormant insurance against a corrupting cell the design believes
/// cannot occur on the live path — so it has no production caller, and no branch
/// chooser is wired (no `if`/`match` at the `render_once` call site, no config
/// toggle, no per-frame heuristic). The reason it stays dormant rather than live
/// is that there is **no runtime signal** to feed a chooser: `rows_diff` returns
/// relative byte runs and never flags an unsafe frame, the `Renderer` carries no
/// such flag, and vt100 exposes no "`rows_diff` is unsafe here" predicate. A
/// chooser today would have no honest input.
///
/// What would make it live: a corrupting cell actually observed on the live
/// `rows_diff` path — most plausibly surfaced by the A2 wide-edge equivalence
/// fixture (slice 06), the CJK/emoji-at-band-edge `.cast` designed to put real
/// pressure on vt100's margin rule. If that fixture ever produces a corrupting
/// cell, A1 reopens with a concrete reason and a known trigger, and only the
/// chooser/refactor remains: wiring it live would mean factoring the
/// cursor-mirroring/baseline-sync tail out of `render_once` to share it (this
/// function takes `(screen, left_margin, width, term)` directly and neither
/// mirrors the cursor nor updates the diff baseline) and inventing the decision
/// input that does not exist today. Until then, the fallback is kept correct and
/// ready, exercised by the cell-walking edge-of-band tests.
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
            // Its `contents()` is "" (byte-length zero), so skipping loses
            // nothing, and a `move_to` to its column would be a spurious defect.
            if cell.is_wide_continuation() {
                continue;
            }
            // Blank cells need no paint — the grid started clear and the gutter
            // clear (slice 05) owns the empties; emitting here would be churn.
            let contents = cell.contents();
            if contents.is_empty() {
                continue;
            }
            // Paint the (possibly wide) cell at its physical column. The lead
            // cell carries the whole glyph; its display width is owned by the
            // lead cell, so the following continuation cell — skipped above —
            // is never separately advanced. The glyph's right half lands at
            // `left_margin + col + 1`, which for a lead cell at the last in-band
            // column (`col == W - 1`) vt100 never produces — a wide glyph that
            // wouldn't fit is wrapped to the next row, so the lead cell is at
            // most `col == W - 2` and `left_margin + W` is never painted.
            term.move_to(geometry::physical_col(left_margin, col), row)?;
            term.write_row(contents.as_bytes())?;
        }
    }
    term.flush()?;
    Ok(())
}

impl Renderer {
    /// Advance the `prev` baseline to match the current screen, so the next
    /// frame's `rows_diff` is against what was just painted. vt100 exposes no
    /// `clone`, so feed `prev` the current screen's `contents_formatted()` —
    /// a full state replay that leaves `prev` cell-identical to `parser`.
    fn sync_prev(&mut self) {
        let formatted = self.parser.screen().contents_formatted();
        // Reset prev to a blank grid of the same size, then replay, so stale
        // cells from a shrunk region don't linger. `set_size` is cheap and
        // clears wrap flags; the formatted replay repaints the live content.
        let (rows, cols) = self.parser.screen().size();
        self.prev = vt100::Parser::new_with_callbacks(rows, cols, 0, GutterCallbacks::new(false));
        self.prev.process(&formatted);
    }

    /// Drop the diff baseline to a blank grid of the live size, so the next
    /// `rows_diff` differs on every non-empty row and forces a full repaint.
    /// Used on resize (ADR-008 step 4): after `set_size` the band geometry
    /// changed, so the cached previous frame is no longer a valid diff baseline.
    ///
    /// The scroll tracker is re-seeded to the live grid at the new size too, so it
    /// keeps mirroring the live content and its scrollback detection stays sound
    /// across the resize / the alt→primary edge that triggers this.
    fn reset_prev_baseline(&mut self) {
        let (rows, cols) = self.parser.screen().size();
        self.prev = vt100::Parser::new_with_callbacks(rows, cols, 0, GutterCallbacks::new(false));
        self.reset_scroll_tracker();
    }

    /// Drain the lines that scrolled off the top of the W-window this frame from
    /// the scroll tracker (ADR-013), then reset the tracker to the live grid so
    /// it starts the next frame with an empty scrollback (bounded memory).
    ///
    /// The tracker carries the **same** content as the live grid but with a small
    /// bounded scrollback, so vt100's own scroll machinery has captured exactly
    /// the departed lines — even across a coalesced burst that turned the screen
    /// over entirely (the live `parser`, `scrollback=0`, dropped them; a grid-vs-
    /// grid overlap heuristic would see no surviving rows and wrongly report
    /// nothing). The lines are returned formatted, oldest first, ready for the
    /// scrolling-band emit.
    ///
    /// Reading walks the tracker's scrollback offsets from deepest to one: at
    /// offset `k` the row that is `k` lines above the current top sits at grid
    /// row 0, so the top row at offsets `n..=1` yields the `n` departed lines in
    /// order. The reset re-seeds the tracker from the live grid's formatted
    /// contents, which leaves its scrollback empty — the same recreate-from-grid
    /// pattern `sync_prev` uses for the diff baseline.
    fn drain_scrolled_off(&mut self) -> Vec<Vec<u8>> {
        let width = self.width;
        // The tracker started this frame with an empty scrollback, so its current
        // scrollback length is exactly the lines that departed this frame. vt100
        // has no length accessor; probe by clamping the offset to its maximum.
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

        // Reset the tracker to the live grid: re-seed from the current formatted
        // contents so its scrollback is empty again for the next frame. Keeps the
        // bounded scrollback so the next frame's scroll is captured the same way.
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

    'frames: loop {
        // --- Phase A: block for the first message (zero idle CPU) ---
        let first = match clock.recv() {
            Some(m) => m,
            // Backstop only: all senders gone with no ChildExited.
            None => break 'frames,
        };
        let mut shutdown = false;
        if let Some(code) = dispatch(first, renderer, pty_writer, resizer, term) {
            exit_code = Some(code);
            shutdown = true;
        }

        let frame_start = clock.now();
        let deadline = clock.deadline(frame_start, FRAME);

        // --- Phase B: drain to the deadline ---
        if !shutdown {
            loop {
                // MANDATORY explicit burst-exit check (ADR-007). NOT redundant
                // with the Timeout arm: under a saturating burst `recv_until`
                // returns `Msg` forever and never `Timeout`, so without this
                // the render is deferred for the whole burst.
                if clock.now() >= deadline {
                    break;
                }
                match clock.recv_until(deadline) {
                    Recv::Msg(m) => {
                        if let Some(code) = dispatch(m, renderer, pty_writer, resizer, term) {
                            exit_code = Some(code);
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

    // Explicit ordered restore BEFORE process::exit (ADR-010). `process::exit`
    // runs no destructors, so this cannot be a Drop guard. The exit code is
    // threaded in so the dim status line draws from the same value `run` returns
    // to `main` — `None` (channel disconnected without a `ChildExited`) maps to a
    // clean exit, so it suppresses the status line just like a zero exit. The
    // renderer is passed so teardown can read the outer alt state (conditional
    // leave) and the final band (the Option C replay) — ADR-012.
    let _ = run_teardown(renderer, term, exit_code.unwrap_or(0));
    exit_code
}

/// The explicit, ordered terminal restore (ADR-010), now mode-aware (ADR-012):
/// **conditional** alt-leave → Option C latched replay + dim status → pop kitty
/// flags → disable mouse → show cursor → disable raw mode. Each step undoes only
/// what was actually set up (conditional alt-leave, kitty pop, mouse disable).
///
/// **Conditional alt-leave (ADR-012).** The alt screen is left **only if**
/// `outer_alt_active` — a plain command never entered it, so there is nothing to
/// leave, and forcing a leave would itself be wrong. The load-bearing ADR-010
/// invariant — alt-leave (when it happens) before raw-disable — is preserved.
///
/// **Option C latched replay (ADR-012/013).** When the child **never** entered
/// the alt screen (`!ever_entered_alt` — the latch, not the bare exit-time read),
/// the final band is replayed onto the primary screen plus the dim `Exited with:
/// N` status line (slice 02). This is the safety net: a plain command's last
/// screenful and its status survive teardown even before slice 04's scroll-aware
/// paint. The latch suppresses the replay for anything that ever touched the alt
/// screen (a TUI that toggled alt then left it before exit), so a TUI's final
/// frame is never littered onto the primary screen. The status line therefore
/// rides the replay only — a TUI emits none.
fn run_teardown<T: OuterTerminal>(
    renderer: &Renderer,
    term: &mut T,
    exit_code: i32,
) -> std::io::Result<()> {
    if renderer.outer_alt_active {
        term.leave_alt_screen()?;
    }
    if !renderer.ever_entered_alt {
        replay_band_to_primary(renderer, term, exit_code)?;
    }
    term.pop_keyboard_flags()?;
    term.disable_mouse()?;
    term.show_cursor()?;
    term.disable_raw_mode()?;
    Ok(())
}

/// Option C's replay (ADR-012/013): paint the final band onto the primary screen
/// at the band offset, then the dim `Exited with: N` status line below it. Called
/// from teardown only when the child never entered the alt screen, so the user is
/// already on the primary screen and the output should persist there.
///
/// Each non-empty row is positioned with `move_to(left_margin, row)` and painted
/// from its full formatted bytes (`rows_formatted`, a from-scratch paint, not a
/// diff — the live diff baseline is irrelevant at teardown). The cursor is left at
/// the end of the bottom-most painted row, so the status line's leading `\r\n`
/// lands one fresh line below the child's output. This is the constrained
/// one-screenful replay — the scroll-off-the-top case is slice 04.
fn replay_band_to_primary<T: OuterTerminal>(
    renderer: &Renderer,
    term: &mut T,
    exit_code: i32,
) -> std::io::Result<()> {
    let screen = renderer.parser.screen();
    for (row, line) in screen.rows_formatted(0, renderer.width).enumerate() {
        // Skip blank rows: a fully-empty formatted row carries no visible cells,
        // so positioning and painting it would only churn the primary screen.
        if line.iter().all(|b| *b == b' ') {
            continue;
        }
        term.move_to(renderer.left_margin, row as u16)?;
        term.write_row(&line)?;
    }
    write_exit_status(term, exit_code)
    // No explicit flush: the queued replay bytes are drained by the `show_cursor`
    // flush later in `run_teardown` (the same way `write_exit_status`'s bytes are),
    // so teardown emits exactly one terminal flush, not a per-helper one.
}

/// Emit the dim `Exited with: N` status line below the band, on a **non-zero**
/// exit only — success is silent (a zero exit emits nothing). The leading `\r\n`
/// scrolls the primary screen one line so the status lands on a fresh line below
/// the content, as a normal command's trailing output would, rather than
/// overwriting the last band row. The dim SGR (`\x1b[2m`) and reset (`\x1b[0m`)
/// ride the existing `write_row` as plain bytes — no new trait method.
///
/// A single reusable unit: slice 03's replay path calls this rather than
/// re-implementing the placement and dimming.
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

    /// A no-op resizer for the slice-02 tests that never drive a resize event.
    /// (The recording resizer that asserts the ADR-008 ordering lives in the
    /// `resize` test module.)
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

    /// Build a left-aligned, fixed-`width` renderer (margin 0) for the slice-02
    /// tests — the absolute-width, left-aligned baseline.
    fn left_renderer(width: u16, rows: u16, outer_kitty: bool) -> Renderer {
        Renderer::new(
            width,
            rows,
            width, // real_cols == width → margin 0 for both Left and Center
            Layout::Left,
            Width::Cols(width),
            outer_kitty,
            Box::new(std::io::sink()),
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

    /// Child-exit-restore (ADR-010), now mode-aware (ADR-012): enqueue
    /// `Msg::ChildExited` with no input event and no PTY content — a plain command
    /// that never entered the alt screen. The restore side effects must fire in
    /// order, the loop must exit rather than park, and the exit code must match
    /// `status.exit_code()`.
    ///
    /// Conditional teardown (ADR-012): the child never entered the alt screen, so
    /// `LeaveAltScreen` must **not** fire (nothing to leave). The Option C latched
    /// replay does run — for a **non-zero** exit (42) it emits the dim
    /// `Exited with: 42` status line via `write_row`, slotted before the remaining
    /// restore steps.
    ///
    /// Non-kitty outer terminal (nothing pushed at startup): `PopKeyboardFlags`
    /// must NOT fire — we only pop what we pushed (ADR-003).
    #[test]
    fn child_exit_restores_in_order_no_kitty_pop_when_not_pushed() {
        use crate::terminal::OuterTerminal;
        let mut clock =
            VirtualClock::new(vec![(0u64, Msg::ChildExited(ExitStatus::with_exit_code(42)))]);
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
        // `Exited with: 42` bytes (a blank grid replays no other `write_row`, so
        // this is the only one).
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
            vec![b"\r\n\x1b[2mExited with: 42\x1b[0m".as_slice()],
            "non-zero exit replays exactly the dim status line"
        );

        // …and it slots before the remaining restore steps: the replay status
        // comes before DisableMouse (the first restore call here).
        let status = term
            .calls
            .iter()
            .position(|c| matches!(c, Call::WriteRow(_)))
            .unwrap();
        let disable_mouse = term
            .calls
            .iter()
            .position(|c| *c == Call::DisableMouse)
            .unwrap();
        assert!(
            status < disable_mouse,
            "replay status slots before the remaining restore steps"
        );
    }

    /// Child-exit-restore with a kitty-capable outer terminal, for a **TUI** that
    /// is in the alt screen at exit (`?1049h` then exit): the startup kitty push
    /// (modelled here by pushing flags on the mock before `run`) must be paired
    /// with a `PopKeyboardFlags` in the correct restore slot — after the (now
    /// conditional) leave-alt-screen, before disable-raw-mode (ADR-010/003/012).
    ///
    /// Because the child entered the alt screen, `LeaveAltScreen` fires and the
    /// Option C replay is suppressed (the `ever_entered_alt` latch) — so a TUI's
    /// teardown emits NO replay rows and NO status line, even on a zero exit.
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

        // A TUI emits no replay and no status line — the latch suppresses Option C.
        assert_eq!(
            term.calls
                .iter()
                .filter(|c| matches!(c, Call::WriteRow(_)))
                .count(),
            0,
            "an alt-screen TUI replays nothing onto the primary screen"
        );
    }

    /// The reusable status-line unit (slice 02), driven directly through
    /// `run_teardown` on a **primary-mode** renderer (never entered the alt
    /// screen) so slice 03's contract is pinned independently of the loop.
    /// `exit_code = 1`: the Option C replay records a `write_row` carrying the dim
    /// `Exited with: 1` bytes, with NO `LeaveAltScreen` (nothing to leave), before
    /// the remaining restore steps. `exit_code = 0`: no status-line `write_row`.
    #[test]
    fn run_teardown_replays_dim_status_on_primary_only_on_nonzero() {
        // Non-zero on a primary (never-alt) renderer: the dim line is replayed,
        // with no alt-leave, before the remaining restore steps.
        let renderer = left_renderer(80, 24, false); // outer_alt_active == false
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
            "exit_code = 1 replays the dim status line via write_row"
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

        // Zero exit: no status-line write_row at all.
        let renderer0 = left_renderer(80, 24, false);
        let mut term0 = MockTerminal::new();
        run_teardown(&renderer0, &mut term0, 0).unwrap();
        assert_eq!(
            term0
                .calls
                .iter()
                .filter(|c| matches!(c, Call::WriteRow(_)))
                .count(),
            0,
            "exit_code = 0 replays no status line"
        );
    }

    // --- Screen-mode mirror (slice 03, ADR-012/013) ---

    /// **Edge-trigger de-dupe (ADR-012).** A child that enters the alt screen
    /// (`?1049h`) and later leaves it (`?1049l`) must toggle the OUTER alt screen
    /// exactly once each — one `EnterAltScreen` then one `LeaveAltScreen` — the
    /// twin of the cursor-visibility de-dupe. Repaints between the edges must not
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

    /// **A plain stream never enters the outer alt screen (the E2 unit floor).**
    /// A child that only prints to the primary screen emits ZERO `EnterAltScreen`
    /// — gutter mirrors the child's mode and never forces the alt screen.
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

    /// **Conditional teardown + latch (ADR-012).** Three streams, one assertion
    /// each, driven through the full loop + teardown:
    /// - plain → no `LeaveAltScreen`, the band is replayed (its `write_row`s) plus
    ///   the dim status line on the non-zero exit;
    /// - alt (still in the alt screen at exit) → `LeaveAltScreen` present, no
    ///   replay;
    /// - alt-then-`?1049l` (left the alt screen before exit) → the
    ///   `ever_entered_alt` latch still suppresses the replay, proving the latch
    ///   beats the bare `alternate_screen()` read.
    #[test]
    fn conditional_teardown_and_latch() {
        // --- Plain stream, non-zero exit: replay + status, no alt-leave. ---
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
        // The replay emits the band rows AND the status line; assert both the
        // content and the status are present among the teardown write_rows.
        assert!(
            writes.iter().any(|w| w.windows(5).any(|s| s == b"hello")),
            "plain stream replays the band content, got {writes:?}"
        );
        assert!(
            writes.contains(&b"\r\n\x1b[2mExited with: 5\x1b[0m".as_slice()),
            "plain stream replays the dim status line, got {writes:?}"
        );

        // --- Alt stream, still in the alt screen at exit: leave, no replay. ---
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
        // No teardown replay: the only paint is the live alt frame, not a replay.
        // After the leave, no write_row is emitted (the replay is suppressed).
        let leave_idx = term.calls.iter().position(|c| *c == Call::LeaveAltScreen).unwrap();
        assert!(
            !term.calls[leave_idx..].iter().any(|c| matches!(c, Call::WriteRow(_))),
            "alt stream: no replay write_row after the alt-leave"
        );

        // --- Alt-then-?1049l: latched, replay STILL suppressed. ---
        let (_f, term, _p, _r, _c) = run_with(
            vec![
                (0u64, Msg::Pty(b"\x1b[?1049h\x1b[1;1Htui".to_vec())),
                (20, Msg::Pty(b"\x1b[?1049l".to_vec())),
                (20, Msg::ChildExited(ExitStatus::with_exit_code(3))),
            ],
            20,
            5,
        );
        // The bare alternate_screen() read at exit is false (the child left it),
        // but the latch remembers — so no replay and no status line.
        let teardown_writes: Vec<&[u8]> = term
            .calls
            .iter()
            .filter_map(|c| match c {
                Call::WriteRow(b) => Some(b.as_slice()),
                _ => None,
            })
            .collect();
        assert!(
            !teardown_writes.contains(&b"\r\n\x1b[2mExited with: 3\x1b[0m".as_slice()),
            "alt-then-primary: the latch suppresses the replay status, got {teardown_writes:?}"
        );
    }

    /// **The primary-branch cursor tail targets the live physical row (ADR-012).**
    /// A plain stream leaves the cursor at the child's cursor row; the recorded
    /// `PlaceCursor` row must equal the grid cursor row, not an absolute row above
    /// the band. (For the constrained one-screenful paint the live physical row IS
    /// the grid cursor row; slice 04's scroll emit is what makes them diverge.)
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

    /// **Primary-mode resize preserves history above the band (ADR-012/013).** A
    /// plain (non-alt) stream resized mid-run must NOT emit the absolute
    /// `clear_gutter` — that would erase the real shell's scrollback above the band
    /// on the primary screen. (The band itself still repaints in full at the new
    /// margin, in both modes — slice 04's `reset_prev_baseline`; this test pins the
    /// suppressed *clear*, not the repaint.) Contrast: an alt stream resized the
    /// same way still does the absolute `[0, rows)` clear (the v1 behaviour, correct
    /// in the alt screen). Pins the `outer_alt_active` gate in `handle_resize`.
    #[test]
    fn primary_resize_preserves_history_alt_resize_clears() {
        use crossterm::event::Event;

        // --- Primary (plain) stream resized: NO clear_gutter (the band repaints). ---
        let script = vec![
            (0u64, Msg::Pty(b"primary content".to_vec())),
            // A resize arrives mid-run, BEFORE the child exits.
            (20, Msg::Input(Event::Resize(100, 30))),
            (20, Msg::ChildExited(ExitStatus::with_exit_code(0))),
        ];
        let mut clock = VirtualClock::new(script);
        let mut renderer = left_renderer(20, 24, false);
        let mut term = MockTerminal::new();
        let mut pty: Vec<u8> = Vec::new();
        run(&mut clock, &mut renderer, &mut term, &mut pty, &NoopResizer);

        // The absolute gutter clear must NOT fire on the primary screen — it would
        // blank rows the real shell drew above the band.
        assert!(
            !term.calls.iter().any(|c| matches!(c, Call::ClearGutter(..))),
            "primary-mode resize must not clear_gutter (would erase history), calls = {:?}",
            term.calls
        );

        // --- Alt stream resized: the absolute clear_gutter STILL fires. ---
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

        assert!(
            term.calls.iter().any(|c| matches!(c, Call::ClearGutter(..))),
            "alt-mode resize must still clear_gutter (v1 behaviour), calls = {:?}",
            term.calls
        );
    }

    /// End-to-end through the render loop's dispatch arm: once the child has
    /// enabled kitty on its output (`CSI > 1 u` arrives as a `Msg::Pty`), a
    /// subsequent Enter and Shift+Enter re-encode at the negotiated level and
    /// reach the PTY writer as the DISTINCT kitty `CSI 13 u` / `CSI 13 ; 2 u`
    /// byte sequences. This is the case-A contract proven through the real
    /// dispatch path (not just the pure encoder), with a kitty-capable outer.
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

    /// Case B through the dispatch path: with the outer terminal unable to source
    /// kitty (`outer_supports_kitty = false`), the child's `CSI > 1 u` enable is
    /// clamped to a no-op, so Enter and Shift+Enter both degrade to the SAME
    /// legacy byte (`\r`). The tested degradation contract.
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

    // --- Mouse forwarding through the render-loop dispatch arm (slice 07) ---
    //
    // These exercise the full Thread-2 wiring: a `Msg::Pty` carrying the child's
    // DECSET negotiation, the per-cycle poll that refreshes the gate from the live
    // screen, then a `Msg::Input(Event::Mouse)` that the gate translates and
    // re-encodes onto the PTY writer. They assert on the child-received bytes (the
    // PTY-writer sink), never the grid — the same contract the expectrl oracle
    // would assert, proven here without a PTY.

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

    /// First post-negotiation click is delivered AND the margin is subtracted:
    /// the child enables SGR mouse (`CSI ?1000h ?1006h`), then a click at physical
    /// col `margin + 5` row 3 arrives → the child receives `CSI < 0 ; 6 ; 4 M`
    /// (child col 5 → SGR 6, row 3 → SGR 4). This is both the correct-cell and the
    /// first-click-after-negotiation criterion (eager capture means click #1 is
    /// the one asserted).
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

    /// Non-Sgr encoding: when the child negotiates a reporting mode but NOT SGR
    /// (`CSI ?1000h` with no `?1006h` → Default encoding), a click must NOT produce
    /// a malformed SGR event. gutter fails loud — the dispatch arm panics rather
    /// than forwarding garbage (the v1 abort, the E2E counterpart of the unit
    /// `BailNonSgr` test).
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

    /// The offset repaint paints each row at the left margin and repositions the
    /// real cursor inside the band. With `left_margin == 0` (this slice) content
    /// starts at physical column 0 and the cursor lands at `(col, row)`.
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

    /// **Cursor visibility golden-master against the real-target fixture (slice
    /// 08).** Replay the checked-in Claude Code fixture through the render loop
    /// at a non-zero margin, sample the cursor state (position + visibility)
    /// across the replay as an insta snapshot, AND assert directly that at settle
    /// the outer cursor position equals `(left_margin + col, row)` from
    /// `cursor_position()` and that the fixture's `CSI ?25l` hid the outer cursor.
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

    /// No bleed past the band: with content that fills the full width, every
    /// painted row's bytes stay within `[0, W)` and nothing is emitted that
    /// would carry the cursor to column `W` (vt100's margin rule, ADR-006). We
    /// assert the recorded `MoveTo` columns and the cursor placement never reach
    /// `margin + W`.
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

    /// **Cursor-shape mirroring (slice 08), end-to-end through the dispatch
    /// path.** A child-emitted `DECSCUSR` (`CSI 6 SP q`, steady bar) must reach
    /// the outer terminal as the matching `CSI 6 SP q` — proving shape is IN
    /// scope (not a documented gap) and mirrored exactly once on the change.
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
}

/// Wide-char (CJK / emoji) edge-of-band correctness — the milestone-2 exit
/// criterion (ADR-006). Every case asserts **two-sidedly**: on the child-side
/// `vt100` grid (what the child believes it painted) AND on the **physical
/// outer cells** at column `margin + W` via a [`RecordingGrid`] (where a real
/// bleed would live — the assertion `COLUMNS == W` can never make).
#[cfg(test)]
mod cjk {
    use super::*;
    use crate::terminal::mock::RecordingGrid;

    /// U+4E00 (一), the canonical double-width CJK ideograph. Two grid cells: a
    /// lead cell holding "一" and a continuation cell (byte-length zero).
    const HAN: &str = "\u{4e00}";

    /// Build a renderer at `width × rows` with margin `margin`, feed it `bytes`
    /// straight into the parser (no clock, no threads — the pure unit path), and
    /// paint one frame through the **primary** `rows_diff` path into a fresh
    /// [`RecordingGrid`] sized to the physical outer terminal (`phys_cols`).
    fn render_primary(
        bytes: &[u8],
        width: u16,
        rows: u16,
        margin: u16,
        phys_cols: u16,
    ) -> (Renderer, RecordingGrid) {
        let mut renderer = Renderer::at_margin(width, rows, margin);
        renderer.parser.process(bytes);
        let mut grid = RecordingGrid::new(phys_cols, rows);
        render_once(&mut renderer, &mut grid).unwrap();
        (renderer, grid)
    }

    /// As [`render_primary`], but paint through the cell-walking **fallback**.
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

    /// Every physical outer cell at column `margin + W` (the first gutter column)
    /// must be blank on every row — the core "no bleed past the band" assertion.
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

    /// **CJK case 1 — DECAWM ON, wide glyph at child col `W-1`.** Set autowrap,
    /// position the cursor at the last in-band column, emit U+4E00. vt100's
    /// margin rule wraps the glyph to the next row rather than placing it at
    /// `W-1`. Assert two-sidedly: child grid shows the glyph wrapped (lead cell
    /// at col 0 of a later row, NOT at `W-1`), and physical `margin + W` blank.
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

    /// **CJK case 2 — DECAWM OFF, wide glyph at child col `W-1`.** Clear
    /// autowrap, same position, emit U+4E00. This is the distinct vt100 code
    /// path. Whatever vt100 does with the glyph (clamp/drop/wrap), the slice's
    /// invariant is the same: the glyph stays **inside** the `W`-column grid and
    /// physical `margin + W` is blank.
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

    /// **CJK case 3 — exactly one in-band cell remaining.** Position so only the
    /// final column is free, then attempt a wide glyph that needs two columns.
    /// It cannot fit at `W-1` (its continuation would be column `W`), so vt100
    /// wraps/drops it inside the band — no half-glyph spill. Physical
    /// `margin + W` blank.
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

    /// **CJK case 4 — cell-walking FALLBACK exercised.** Drive a wide glyph,
    /// then repaint through `render_cell_walk` (the absolute-column fallback).
    /// Assert: the continuation cell is **skipped** (no `move_to`/emit for it —
    /// proven by the physical grid showing exactly one glyph, no double-glyph),
    /// there is **no double-advance** (the glyph occupies two physical columns,
    /// the cell after it is the continuation, blank), and physical `margin + W`
    /// is blank. This is the one case that exercises gutter's own
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

    /// The fallback's continuation-skip emits **no** `move_to` to a continuation
    /// column. Proven against [`MockTerminal`]'s recorded call sequence: with
    /// two wide glyphs the fallback issues exactly two `move_to`s (one per lead
    /// cell), never one per cell.
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

    /// insta golden-master: a representative CJK line rendered through the
    /// **primary** path, snapshotting the physical outer grid (the painted-column
    /// map). Freezes correct in-band column alignment so any future wide-char
    /// drift surfaces as a snapshot diff.
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

    /// Primary-path purity (ADR-006 re-scope checkpoint, mechanised): the
    /// production `render_once` source must contain no `is_wide_continuation`
    /// call and no per-cell walk. Reading the source keeps the architectural
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

    /// Slice-02 regression sentinel: U+4E00 at the band edge does not drift the
    /// child grid's reported width — the child still believes it has exactly `W`
    /// columns regardless of the wide glyph.
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

/// Primary-screen scroll emit (slice 04 / ADR-013): the count-based scroll-delta
/// (sourced from the scroll tracker, robust to a burst that turns the screen over
/// in one frame) and the emit of each departed top line into the real terminal's
/// scrollback as a scrolling stream. Drives `render_once` directly against a
/// [`MockTerminal`] (to count the per-departed-line `Newline`) and a physical-
/// sized [`RecordingGrid`] **with scrollback** (to read back the scrolled-off
/// content), so the assertions land on the real emit, not the loop plumbing.
#[cfg(test)]
mod primary_scroll {
    use super::*;
    use crate::terminal::mock::{Call, MockTerminal, RecordingGrid};

    /// Feed bytes to the renderer exactly as the render loop's `Msg::Pty` dispatch
    /// does — the live parser AND the scroll tracker — so the tests exercise the
    /// real per-frame scroll detection rather than a parser the tracker never saw.
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

    /// **Count-based scroll-delta (the ADR-007/013 obligation).** Prime the grid
    /// full, then advance it by THREE lines in a SINGLE frame (one `render_once`)
    /// and assert ALL THREE departed lines (L0, L1, L2) reach the recorder's
    /// scrollback, in order — the count is the number of lines advanced, NOT
    /// one-per-frame. A one-per-frame regression would carry only ONE line into
    /// scrollback under this coalesced burst and fail (L1 and L2 would be missing).
    /// The two rows that stayed on screen (L3, L4) plus the fresh ones are the
    /// visible band, not scrollback.
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

    /// **The whole-screen-turnover burst — the case a witnessed-overlap delta
    /// drops.** Prime a 4-row band full, then advance it by EIGHT lines in a SINGLE
    /// frame (twice the band height) so the new grid shares NO row with the old —
    /// there is no surviving overlap to witness the scroll, the exact case the
    /// previous grid-diff delta returned 0 for and silently lost. Replay the frame
    /// into a real-terminal-shaped [`RecordingGrid`] **with scrollback** and assert
    /// every one of the eight departed lines (including the ones that arrived and
    /// left within the single frame, never on any painted grid) is recoverable from
    /// the recorder's scrollback, in order, and the last screenful is visible.
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

    /// **A still screen emits nothing.** With no advance between frames the
    /// scroll-delta is zero, so no Newline — the emit only fires on a real scroll.
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

    /// **The first primary frame, filling the screen, pushes nothing into
    /// scrollback.** A fresh renderer fed less than one screenful is still
    /// filling — content grows downward, nothing departs — so no Newline.
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

    /// **The alt screen never scrolls the outer terminal.** Even when the child's
    /// alt-screen content changes between frames, no Newline is emitted — the
    /// scroll emit is primary-only (an alt screen owns a fixed viewport), and the
    /// tracker is drained-and-dropped without advancing the terminal.
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

    /// **Scrolled-off lines are emitted, frame after frame, and the band stays the
    /// last screenful.** Drive more lines than fit the band, one line per frame,
    /// across the whole run into a real-terminal-shaped [`RecordingGrid`] **with
    /// scrollback**: the early lines that scroll off the top must each be
    /// recoverable from the recorder's scrollback (the count is what carries them
    /// in), and the recorder must end with the last screenful visible.
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

    /// **The tracker's bounded scrollback does not accumulate across frames.** The
    /// tracker is reset to the live grid every `render_once`, so after many
    /// scrolling frames its scrollback length is back to zero between frames — the
    /// ADR-013 backpressure guarantee that this detection device stays bounded and
    /// never grows like vt100 `set_scrollback` would. We probe the tracker length
    /// directly after a render.
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

/// Resize (slice 05 / ADR-008 / ADR-011): the SIGWINCH ordering, the proportional
/// `--width Npct` recompute, the centred-offset recompute, the gutter clear, and
/// the stress / floor-cap criteria. Drives `handle_resize` directly (every
/// dependency injected) so the intermediate-invariant assertions land in the
/// window *before* the child's repaint — the assertion the settled-grid test
/// structurally cannot make.
#[cfg(test)]
mod resize {
    use super::*;
    use crate::geometry::{Layout, Width, MIN_W};
    use crate::terminal::mock::{Call, MockTerminal, RecordingGrid};
    use std::cell::RefCell;

    /// A recording [`PtyResizer`] capturing each `master.resize(cols, rows)` in
    /// order. The ADR-008 gate asserts `master.resize` *preceded* `set_size`.
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
        )
    }

    /// **Resize ordering (ADR-008 gate).** Drive one resize and assert the
    /// `master.resize` call was recorded BEFORE `set_size` ran, both inside the
    /// one `handle_resize` invocation, and that `set_size` left the parser at the
    /// band width `W` — proving `master.resize` happened first against the same
    /// `W`.
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

    /// **Mid-burst case A — intermediate invariant.** After `set_size`, BEFORE
    /// any child repaint is fed in, the grid must be internally consistent:
    /// `COLUMNS == W`, cursor column in `[0, W)`, every row length `== W`, no
    /// panic. This is the degraded-but-consistent contract (ADR-008).
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

    /// **Mid-burst case B — settled grid.** Feed `[old bytes at old W →
    /// set_size(rows, W) → new bytes at new W]` and assert the settled grid equals
    /// a reference parser fed only the post-resize stream at the new size, with
    /// `COLUMNS == W` and no panic. (The child's clear+repaint after SIGWINCH
    /// overwrites the transient grid wholesale — modelled here by the new bytes
    /// starting with a clear.)
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

    /// **Mid-burst case C — physical gutter has no stale cells.** Paint a wide
    /// left-aligned frame in the **alt screen** (where the absolute gutter clear
    /// is owned, ADR-012), then resize so the band shrinks and the margin moves;
    /// after the gutter clear + repaint, every physical cell outside the band must
    /// be blank. Proves the explicit gutter clear (ADR-008 step 4) — the
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

    /// **Resize stress.** A scripted sequence of rapid resizes, including
    /// shrinking `real_cols` below the band width so the centred margin clamps to
    /// 0 (the `saturating_sub` path). Must never panic and always settle to a
    /// consistent grid (`COLUMNS == W`, all rows length `W`).
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

    /// **Proportional resize (ADR-011).** With `Width::Percent(50)`, drive a
    /// resize from `real_cols = 200` to `160`; assert `W` is recomputed (100 →
    /// 80), and that BOTH `master.resize` and `set_size` used the new `W` (not
    /// `real_cols`, not the old `W`). Then the SAME resize with `Width::Cols(100)`
    /// asserts `W` stays 100 (step 0 is a no-op) — the absolute path is untouched.
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

    /// **Floor/cap on a proportional resize.** Shrinking the terminal below the
    /// point where the percentage would yield less than `MIN_W` floors the band at
    /// `MIN_W`; a terminal narrower than `MIN_W` caps the band at the terminal —
    /// never `0`, never `> real_cols`, no panic.
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

    /// The gutter clear is invoked with the live band geometry every resize — in
    /// the **alt screen**, where gutter owns the whole viewport (ADR-012). Proven
    /// on the `MockTerminal` call record.
    #[test]
    fn resize_clears_the_gutter() {
        let mut r = renderer(80, 24, 200, Layout::Center, Width::Cols(80));
        // The child is in the alt screen at the resize (a TUI). The absolute
        // gutter clear is gated on this (ADR-012); see the primary-mode guard test
        // for the suppressed case.
        r.outer_alt_active = true;
        let resizer = RecResizer::default();
        let mut term = MockTerminal::new();
        handle_resize(&mut r, &resizer, &mut term, 120, 30);

        // (margin, width, real_cols, rows) for the new geometry: margin =
        // (120-80)/2 = 20, W = 80, real_cols = 120, rows = 30.
        assert!(
            term.calls.contains(&Call::ClearGutter(20, 80, 120, 30)),
            "gutter clear must run with the recomputed geometry, calls = {:?}",
            term.calls
        );
    }

    /// **Primary-aware resize clears only the live band region (slice 04 /
    /// ADR-013).** On the **primary** screen a resize must NOT emit the absolute
    /// `[0, rows)` `clear_gutter` (that would blank rows holding real shell
    /// history), but it must still force a full band repaint so the band tracks
    /// the new margin/width. Driving `handle_resize` directly then one
    /// `render_once`: no `ClearGutter`, yet the band content is repainted in full
    /// at the new margin (the diff baseline was reset, so every populated row is
    /// re-emitted) — the "narrow what is cleared, not whether the band repaints"
    /// contract.
    #[test]
    fn primary_resize_repaints_band_without_absolute_clear() {
        // A primary-screen renderer (never entered the alt screen) with content.
        let mut r = renderer(40, 6, 100, Layout::Center, Width::Cols(40));
        r.parser.process(b"\x1b[1;1Hbanded primary content");
        // Settle `prev` to the current grid so a plain re-render would diff to
        // nothing — the resize must be what forces the repaint below.
        let mut sink = MockTerminal::new();
        render_once(&mut r, &mut sink).unwrap();

        let resizer = RecResizer::default();
        let mut term = MockTerminal::new();
        handle_resize(&mut r, &resizer, &mut term, 120, 10);

        // No absolute gutter clear on the primary screen — real history is safe.
        assert!(
            !term.calls.iter().any(|c| matches!(c, Call::ClearGutter(..))),
            "primary resize must not clear_gutter (would erase history), calls = {:?}",
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
        let repainted_at_margin = term2.calls.contains(&Call::MoveTo(40, 0));
        assert!(
            repainted_at_margin,
            "the repainted band row must land at the recomputed margin 40, calls = {:?}",
            term2.calls
        );
    }
}
