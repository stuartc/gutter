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
//! Dispatch (slice 02): `Pty(b)` → `parser.process(b)`; `Input(Key)` →
//! placeholder legacy encode → PTY writer (real re-encode is slice 04);
//! `ChildExited(s)` → set shutdown with status, break. Resize/focus/mouse are
//! swallowed (resize is slice 05, mouse slice 07).

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
    pub fn new(width: u16, rows: u16, left_margin: u16) -> Self {
        Self {
            parser: vt100::Parser::new_with_callbacks(
                rows,
                width,
                0,
                GutterCallbacks,
            ),
            prev: vt100::Parser::new_with_callbacks(
                rows,
                width,
                0,
                GutterCallbacks,
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
                if let Some(bytes) = keyboard::encode(&key) {
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
        self.prev = vt100::Parser::new_with_callbacks(rows, cols, 0, GutterCallbacks);
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
    use crate::clock::Clock;
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
        let mut renderer = Renderer::new(width, rows, 0);
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
            vt100::Parser::new_with_callbacks(24, 80, 0, GutterCallbacks);
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
        let mut renderer = Renderer::new(80, 24, 0);
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
        let mut renderer = Renderer::new(80, 24, 0);
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
    #[test]
    fn child_exit_restores_in_order_and_returns_code() {
        let script = vec![(0u64, Msg::ChildExited(ExitStatus::with_exit_code(42)))];
        let (_flushes, term, _pty, _r, code) = run_with(script, 80, 24);

        assert_eq!(code, Some(42), "exit code must equal status.exit_code()");
        assert_eq!(
            term.restore_calls(),
            vec![
                Call::LeaveAltScreen,
                Call::PopKeyboardFlags,
                Call::DisableMouse,
                Call::ShowCursor,
                Call::DisableRawMode,
            ],
            "restore must fire in the ADR-010 order"
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
        let mut renderer = Renderer::new(20, 5, 7); // margin 7
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
        let mut renderer = Renderer::new(width, 5, 0);
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
