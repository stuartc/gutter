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
//! child's current kitty level (ADR-002/003) → PTY writer; `ChildExited(s)` →
//! set shutdown with status, break. Resize/focus/mouse are swallowed (resize is
//! slice 05, mouse slice 07).

use std::io::Write;
use std::time::Duration;

use crate::callbacks::GutterCallbacks;
use crate::clock::{Clock, Recv};
use crate::keyboard;
use crate::msg::Msg;
use crate::terminal::OuterTerminal;

/// The 60fps frame budget. One render per `FRAME` of wall (or virtual) time.
pub const FRAME: Duration = Duration::from_millis(16);

/// The render thread's state: the parser, the cached previous screen for the
/// `rows_diff`, the band width, the left margin, and the last cursor-visibility
/// we mirrored to the outer terminal.
pub struct Renderer {
    parser: vt100::Parser<GutterCallbacks>,
    /// The previous-frame screen the `rows_diff` is computed against.
    prev: vt100::Parser<GutterCallbacks>,
    /// The band width `W`. Fixed for the session in this slice.
    width: u16,
    /// The band's left margin (physical column the band starts at). Fixed `0`
    /// (left-aligned) in this slice; centred margin arrives with the flag in
    /// slice 05.
    left_margin: u16,
    /// The cursor visibility last mirrored to the outer terminal, so we only
    /// emit a show/hide when it actually changes.
    cursor_visible: bool,
}

impl Renderer {
    /// Build a renderer for a `width × rows` virtual grid at `left_margin`.
    ///
    /// `outer_supports_kitty` is the startup `supports_keyboard_enhancement()`
    /// probe — it clamps the child's kitty negotiation (ADR-003). The live
    /// `parser` carries it; `prev` is a diff-baseline that only replays formatted
    /// content and never tracks kitty, so its clamp is irrelevant (`false`).
    pub fn new(width: u16, rows: u16, left_margin: u16, outer_supports_kitty: bool) -> Self {
        Self {
            parser: vt100::Parser::new_with_callbacks(
                rows,
                width,
                0,
                GutterCallbacks::new(outer_supports_kitty),
            ),
            prev: vt100::Parser::new_with_callbacks(
                rows,
                width,
                0,
                GutterCallbacks::new(false),
            ),
            width,
            left_margin,
            // vt100 starts with the cursor visible; mirror that initial state.
            cursor_visible: true,
        }
    }

    /// Read-only view of the virtual screen — for the insta snapshot and the
    /// slice-08 equivalence gate (no production caller in this slice).
    #[allow(dead_code)]
    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }
}

/// Apply one message to the render state. Returns the child's exit code when the
/// message is `ChildExited` (the loop then tears down and stops), else `None`.
///
/// The PTY writer is injected as a `Write` sink so the input-liveness test can
/// assert "key bytes reached the PTY-master mock within one frame" against a
/// recording `Vec<u8>` with no real child.
fn dispatch<P: Write>(msg: Msg, renderer: &mut Renderer, pty_writer: &mut P) -> Option<i32> {
    match msg {
        Msg::Pty(bytes) => {
            renderer.parser.process(&bytes);
            None
        }
        Msg::Input(event) => {
            if let crossterm::event::Event::Key(key) = event {
                // Re-encode at the child's CURRENT kitty level (ADR-002/003).
                // The level lives on the parser's callbacks — read lock-free
                // because the parser and the encoder both run on this thread.
                let level: keyboard::KittyLevel =
                    renderer.parser.callbacks().kitty_state.current();
                let bytes = keyboard::encode_key(&key, level);
                if !bytes.is_empty() {
                    let _ = pty_writer.write_all(&bytes);
                    let _ = pty_writer.flush();
                }
            }
            // Resize/focus/mouse are swallowed in slice 02 (resize = slice 05,
            // mouse = slice 07).
            None
        }
        Msg::ChildExited(status) => Some(status.exit_code() as i32),
    }
}

/// Paint the current virtual grid to the outer terminal at the band's offset
/// (ADR-006). The ONLY place `rows_diff` and `move_to`/`place_cursor` are
/// called — cursor repositioning does NOT live in the channel code.
///
/// For each visible row, emit our own `move_to(left_margin, row)` then that
/// row's `rows_diff` byte run (which carries its own intra-row SGR and relative
/// cursor moves, scoped to `[0, W)`, so it paints into physical columns
/// `[margin, margin + W)` and never past `margin + W` — vt100's margin rule
/// guarantees the grid is exactly `W` columns). Empty diffs (unchanged rows)
/// are skipped. After the repaint, mirror the child's cursor visibility and
/// reposition the real cursor to `(left_margin + col, row)`.
fn render_once<T: OuterTerminal>(renderer: &mut Renderer, term: &mut T) -> std::io::Result<()> {
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

    // Mirror DECTCEM: only emit a show/hide when the state actually changed.
    let visible = !screen.hide_cursor();
    if visible != renderer.cursor_visible {
        term.set_cursor_visible(visible)?;
        renderer.cursor_visible = visible;
    }

    // Reposition the real cursor inside the band.
    let (crow, ccol) = screen.cursor_position();
    term.place_cursor(crate::geometry::physical_col(renderer.left_margin, ccol), crow)?;

    term.flush()?;

    // The current screen becomes the baseline for the next frame's diff. vt100
    // has no clone, so re-process the formatted state into `prev`. Cheap: it is
    // a single in-memory grid replay, not per-frame allocation churn at scale.
    renderer.sync_prev();
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
/// No production call site wires the fallback yet — slice 02 stood up the
/// primary `rows_diff` path and did not build the absolute-column branch
/// chooser, and this slice (per ADR-006) finishes *what the fallback does*, not
/// *when it fires*. The branch chooser lands with the path that needs it; this
/// function is exercised by the cell-walking edge-of-band tests now.
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
            term.move_to(crate::geometry::physical_col(left_margin, col), row)?;
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
pub fn run<C, T, P>(
    clock: &mut C,
    renderer: &mut Renderer,
    term: &mut T,
    pty_writer: &mut P,
) -> Option<i32>
where
    C: Clock<Msg = Msg>,
    T: OuterTerminal,
    P: Write,
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
        if let Some(code) = dispatch(first, renderer, pty_writer) {
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
                        if let Some(code) = dispatch(m, renderer, pty_writer) {
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
    // runs no destructors, so this cannot be a Drop guard.
    let _ = run_teardown(term);
    exit_code
}

/// The explicit, ordered terminal restore (ADR-010): leave alt screen → pop
/// kitty flags → disable mouse → show cursor → disable raw mode. Pop/disable are
/// no-ops in slice 02 but stay in the sequence so slices 04/07 drop in without
/// re-sequencing.
fn run_teardown<T: OuterTerminal>(term: &mut T) -> std::io::Result<()> {
    term.leave_alt_screen()?;
    term.pop_keyboard_flags()?;
    term.disable_mouse()?;
    term.show_cursor()?;
    term.disable_raw_mode()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::mock::{Call, MockTerminal};
    use portable_pty::ExitStatus;

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

    fn run_with(script: Vec<(u64, Msg)>, width: u16, rows: u16) -> (usize, MockTerminal, Vec<u8>, Renderer, Option<i32>) {
        let mut clock = VirtualClock::new(script);
        let mut renderer = Renderer::new(width, rows, 0, false);
        let mut term = MockTerminal::new();
        let mut pty: Vec<u8> = Vec::new();
        let code = run(&mut clock, &mut renderer, &mut term, &mut pty);
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
        let mut renderer = Renderer::new(80, 24, 0, false);
        let mut term = MockTerminal::new();
        let mut pty: Vec<u8> = Vec::new();

        let code = run(&mut clock, &mut renderer, &mut term, &mut pty);

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
        let mut renderer = Renderer::new(80, 24, 0, false);
        let mut term = MockTerminal::new();
        let mut pty: Vec<u8> = Vec::new();
        let code = run(&mut clock, &mut renderer, &mut term, &mut pty);

        // Enter → \r reaches the PTY writer.
        assert_eq!(pty, b"\r", "the mid-burst keystroke must reach the PTY master");

        let flushes = term.calls.iter().filter(|c| **c == Call::Flush).count();
        let cap = ((60.0 * total_ms as f64 / 1000.0).ceil() as usize) + 1;
        assert!(flushes <= cap, "render count {flushes} must be <= cap {cap}");
        assert_eq!(code, Some(0));
    }

    /// Child-exit-restore (ADR-010): enqueue `Msg::ChildExited` with no input
    /// event; the restore side effects must fire in order, the loop must exit
    /// rather than park, and the exit code must match `status.exit_code()`.
    ///
    /// Non-kitty outer terminal (nothing pushed at startup): `PopKeyboardFlags`
    /// must NOT fire — we only pop what we pushed (ADR-003).
    #[test]
    fn child_exit_restores_in_order_no_kitty_pop_when_not_pushed() {
        let script = vec![(0u64, Msg::ChildExited(ExitStatus::with_exit_code(42)))];
        let (_flushes, term, _pty, _r, code) = run_with(script, 80, 24);

        assert_eq!(code, Some(42), "exit code must equal status.exit_code()");
        assert_eq!(
            term.restore_calls(),
            vec![
                Call::LeaveAltScreen,
                Call::DisableMouse,
                Call::ShowCursor,
                Call::DisableRawMode,
            ],
            "restore order with NO kitty pop (nothing was pushed)"
        );
    }

    /// Child-exit-restore with a kitty-capable outer terminal: the startup push
    /// (modelled here by pushing flags on the mock before `run`) must be paired
    /// with a `PopKeyboardFlags` in the correct restore slot — after
    /// leave-alt-screen, before disable-raw-mode (ADR-010/003).
    #[test]
    fn child_exit_pops_kitty_flags_when_pushed_at_startup() {
        use crate::terminal::OuterTerminal;
        let mut clock = VirtualClock::new(vec![(
            0u64,
            Msg::ChildExited(ExitStatus::with_exit_code(0)),
        )]);
        let mut renderer = Renderer::new(80, 24, 0, true);
        let mut term = MockTerminal::kitty_capable();
        // Simulate the startup probe + push that main.rs performs.
        assert!(term.supports_keyboard_enhancement().unwrap());
        term.push_keyboard_flags().unwrap();
        let mut pty: Vec<u8> = Vec::new();

        let code = run(&mut clock, &mut renderer, &mut term, &mut pty);

        assert_eq!(code, Some(0));
        assert_eq!(
            term.restore_calls(),
            vec![
                Call::LeaveAltScreen,
                Call::PopKeyboardFlags,
                Call::DisableMouse,
                Call::ShowCursor,
                Call::DisableRawMode,
            ],
            "kitty flags pushed at startup must be popped in the ADR-010 slot"
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
        let mut renderer = Renderer::new(80, 24, 0, true);
        let mut term = MockTerminal::kitty_capable();
        let mut pty: Vec<u8> = Vec::new();
        run(&mut clock, &mut renderer, &mut term, &mut pty);

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
        let mut renderer = Renderer::new(80, 24, 0, false);
        let mut term = MockTerminal::new();
        let mut pty: Vec<u8> = Vec::new();
        run(&mut clock, &mut renderer, &mut term, &mut pty);

        assert_eq!(
            pty, b"\r\r",
            "both Enters degrade to the same legacy byte under the clamp"
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
        let mut renderer = Renderer::new(20, 5, 7, false); // margin 7
        let mut term = MockTerminal::new();
        let mut pty: Vec<u8> = Vec::new();
        run(&mut clock, &mut renderer, &mut term, &mut pty);

        let first_move = term.calls.iter().find_map(|c| {
            if let Call::MoveTo(col, row) = c { Some((*col, *row)) } else { None }
        });
        assert_eq!(first_move, Some((7, 0)), "row painted at margin 7");

        let place = term.calls.iter().rev().find_map(|c| {
            if let Call::PlaceCursor(col, row) = c { Some((*col, *row)) } else { None }
        });
        assert_eq!(place, Some((7 + 2, 0)), "cursor at margin + col");
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
        let mut renderer = Renderer::new(width, 5, 0, false);
        let mut term = MockTerminal::new();
        let mut pty: Vec<u8> = Vec::new();
        run(&mut clock, &mut renderer, &mut term, &mut pty);

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
        let mut renderer = Renderer::new(width, rows, margin, false);
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
        let mut renderer = Renderer::new(width, rows, margin, false);
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
        let mut renderer = Renderer::new(w, rows, margin, false);
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
        let mut renderer = Renderer::new(w, rows, 0, false);
        renderer
            .parser
            .process(format!("\x1b[1;{w}H{HAN}").as_bytes());
        assert_eq!(renderer.parser.screen().size(), (rows, w));
    }
}
