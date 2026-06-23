//! PTY-driven integration tests (the slice-02 acceptance criteria).
//!
//! These drive the real `gutter` binary through a real PTY (via `expectrl` /
//! `ptyprocess`) and assert on what the child and the outer terminal actually
//! see — never on gutter internals. The outer terminal is parsed back through a
//! second `vt100` so physical-cell assertions can read which column each glyph
//! landed in.
//!
//! Two harness facts shape the tests:
//!
//! - **Outer size.** The PTY's default is 80x24. To run gutter inside a wider
//!   terminal (so a `--width 100` band leaves real gutters) the command is
//!   `sh -c 'stty cols C rows R; exec gutter ...'`: `stty` resizes gutter's own
//!   controlling terminal BEFORE it reads its size at startup — deterministic,
//!   no resize race.
//! - **Capture the LIVE frame, not the post-exit screen.** gutter renders into
//!   the alternate screen and, on child exit, leaves it (`?1049l`) — which
//!   discards that screen's content. So the wrapped children here stay alive
//!   (a long `sleep`) and the harness drains a bounded window WHILE the child
//!   is still running, parsing the alt-screen frame gutter actually painted.
//!   The child-exit-restore test is the one case that deliberately reads the
//!   teardown sequence instead.
//!
//! CI runs these headlessly: a real PTY, no display, `TERM=xterm-256color`.

use std::io::Write;
use std::time::{Duration, Instant};

use expectrl::session::OsSession;

/// Path to the freshly-built `gutter` binary (cargo sets this for the test).
fn gutter_bin() -> String {
    env!("CARGO_BIN_EXE_gutter").to_string()
}

/// Build a `std::process::Command` that runs gutter inside an outer terminal of
/// the given size: `sh -c 'stty cols C rows R; exec gutter <args>'`. The whole
/// script is one argv element, so the inner shell parses any nested quoting in
/// `gutter_args` — avoiding expectrl's own word-splitting.
fn gutter_in_terminal(
    outer_cols: u16,
    outer_rows: u16,
    gutter_args: &str,
) -> std::process::Command {
    // `GUTTER_FORCE_KITTY=0`: a dumb test PTY can't answer the kitty probe, so
    // the real `supports_keyboard_enhancement()` would stall ~2s before
    // returning false. This suite is not about the keyboard; inject the known
    // result to skip the stall (slice 04's injectable-capability seam).
    let script = format!(
        "stty cols {outer_cols} rows {outer_rows}; exec env GUTTER_FORCE_KITTY=0 {} {gutter_args}",
        gutter_bin()
    );
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.arg("-c").arg(script);
    cmd
}

/// Spawn the prepared gutter command under a PTY.
fn spawn(cmd: std::process::Command) -> OsSession {
    OsSession::spawn(cmd).expect("spawn gutter under PTY")
}

/// Drain a bounded window of output using NON-BLOCKING reads, so the window is a
/// real wall-clock cap even while the child keeps the PTY open: a still-running
/// child's live alt-screen frame is captured (not the post-exit primary screen,
/// which leaving the alt screen would discard). `try_read` returns `WouldBlock`
/// when no data is pending — the loop just keeps ticking until `window` elapses.
fn drain_window(session: &mut OsSession, window: Duration) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    let start = Instant::now();
    while start.elapsed() < window {
        match session.try_read(&mut buf) {
            Ok(0) => break, // EOF (child gone)
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
        std::thread::sleep(Duration::from_millis(3));
    }
    out
}

/// Parse outer-terminal bytes through a vt100 at the given physical size, so
/// tests can inspect which physical column each glyph landed in.
fn outer_grid(bytes: &[u8], cols: u16, rows: u16) -> vt100::Parser {
    let mut p = vt100::Parser::new(rows, cols, 0);
    p.process(bytes);
    p
}

/// The first non-empty row's trimmed text, or "" if the screen is blank.
fn first_content_row(screen: &vt100::Screen, cols: u16) -> String {
    screen
        .rows(0, cols)
        .map(|r| r.trim_end().to_string())
        .find(|r| !r.is_empty())
        .unwrap_or_default()
}

/// Assert the gutter columns `[band, outer)` hold no painted glyph on any row.
fn assert_gutters_empty(screen: &vt100::Screen, band: u16, outer_cols: u16, rows: u16) {
    for r in 0..rows {
        for c in band..outer_cols {
            if let Some(cell) = screen.cell(r, c) {
                let s = cell.contents();
                assert!(
                    s.is_empty() || s == " ",
                    "gutter cell ({r},{c}) must be empty, found {s:?}"
                );
            }
        }
    }
}

/// **Child sees `COLUMNS == W`** — asserted from inside the child (`stty size`
/// prints `rows W`), not from gutter internals. gutter sizes the child PTY to
/// `W × real_rows` regardless of the outer width, so this holds independently of
/// the outer terminal size.
#[test]
fn child_sees_band_width() {
    let cmd = gutter_in_terminal(120, 40, "--width 100 /bin/sh -c 'stty size; sleep 3'");
    let mut session = spawn(cmd);
    let bytes = drain_window(&mut session, Duration::from_millis(700));

    let parser = outer_grid(&bytes, 120, 40);
    let row = first_content_row(parser.screen(), 120);
    assert!(
        row.contains("100"),
        "child must see W=100 columns (stty size row was {row:?})"
    );
}

/// **Band render + gutter empties.** `gutter --width 100 <cmd>` renders the
/// child inside a 100-column band; content starts at left margin 0 and the
/// gutter columns to the right (>= 100) hold no stale cells.
#[test]
fn content_in_band_gutters_empty() {
    let child = "/bin/sh -c 'printf HELLO_FROM_THE_BAND; sleep 3'";
    let cmd = gutter_in_terminal(120, 40, &format!("--width 100 --left {child}"));
    let mut session = spawn(cmd);
    let bytes = drain_window(&mut session, Duration::from_millis(700));

    let parser = outer_grid(&bytes, 120, 40);
    let screen = parser.screen();

    let row0: String = screen.rows(0, 120).next().unwrap_or_default();
    assert!(
        row0.starts_with("HELLO_FROM_THE_BAND"),
        "content must start at the left margin (row 0 = {row0:?})"
    );
    assert_gutters_empty(screen, 100, 120, 40);
}

/// **Cursor tracking.** After the repaint the real cursor lands inside the band
/// at `(left_margin + col, row)` following the child's cursor (margin 0 in this
/// slice, so physical col == child col).
#[test]
fn cursor_tracks_child_inside_band() {
    // Move to row 3, col 10 (1-based CSI), then idle alive.
    let child = "/bin/sh -c 'printf \"\\033[3;10H\"; sleep 3'";
    let cmd = gutter_in_terminal(120, 40, &format!("--width 100 --left {child}"));
    let mut session = spawn(cmd);
    let bytes = drain_window(&mut session, Duration::from_millis(700));

    let parser = outer_grid(&bytes, 120, 40);
    let (crow, ccol) = parser.screen().cursor_position();
    // CSI 3;10H is row 2, col 9 zero-based; margin 0 so physical == child.
    assert_eq!(
        (crow, ccol),
        (2, 9),
        "outer cursor must track the child into the band"
    );
}

/// **Cursor hide/show (DECTCEM) mirrored.** The child hides its cursor; gutter
/// must mirror that on the outer terminal (the outer vt100 reports hidden).
#[test]
fn cursor_visibility_mirrored_on_outer() {
    let child = "/bin/sh -c 'printf \"\\033[?25lX\"; sleep 3'";
    let cmd = gutter_in_terminal(120, 40, &format!("--width 100 --left {child}"));
    let mut session = spawn(cmd);
    let bytes = drain_window(&mut session, Duration::from_millis(700));

    let parser = outer_grid(&bytes, 120, 40);
    assert!(
        parser.screen().hide_cursor(),
        "outer terminal must mirror the child hiding its cursor"
    );
}

/// **Full-screen TUI usable.** Drive vim inside the band: open it, type text,
/// and assert the edited text renders at the band's left edge with the gutter
/// still empty — proving a real full-screen app (alt screen, absolute
/// positioning, SGR) works through gutter, not just `echo`.
#[test]
fn vim_renders_inside_band() {
    let child = "/usr/bin/vim -u NONE -N -i NONE";
    let cmd = gutter_in_terminal(120, 40, &format!("--width 100 --left {child}"));
    let mut session = spawn(cmd);

    // Let vim enter the alt screen and lay out.
    std::thread::sleep(Duration::from_millis(700));
    // Insert mode, type a marker at the top-left, then leave insert mode.
    session.write_all(b"ggIGUTTERVIMOK\x1b").unwrap();
    session.flush().unwrap();
    std::thread::sleep(Duration::from_millis(500));

    let bytes = drain_window(&mut session, Duration::from_millis(600));
    let parser = outer_grid(&bytes, 120, 40);
    let screen = parser.screen();

    let row0: String = screen.rows(0, 120).next().unwrap_or_default();
    assert!(
        row0.starts_with("GUTTERVIMOK"),
        "vim edit must render at the band's left margin (row 0 = {row0:?})"
    );
    // Content stays inside the band — gutter columns still empty.
    for c in 100..120u16 {
        if let Some(cell) = screen.cell(0, c) {
            let s = cell.contents();
            assert!(
                s.is_empty() || s == " ",
                "vim content must not bleed into the gutter at col {c}"
            );
        }
    }

    // Quit vim without saving so the process exits cleanly.
    session.write_all(b"\x1b:q!\r").unwrap();
    let _ = session.flush();
}

/// **Child-exit restore + exit code (real-PTY smoke).** A child that enters the
/// alt screen then exits immediately: gutter must leave the alt screen and show
/// the cursor with NO keypress, and propagate the child's exit code.
#[test]
fn child_exit_restores_terminal_and_propagates_code() {
    let child = "/bin/sh -c 'printf \"\\033[?1049h\"; exit 7'";
    let cmd = gutter_in_terminal(80, 24, &format!("--width 60 {child}"));
    let mut session = spawn(cmd);

    // Read to EOF this time — we WANT the teardown sequence.
    let bytes = drain_window(&mut session, Duration::from_secs(3));
    let s = String::from_utf8_lossy(&bytes);
    assert!(
        s.contains("\u{1b}[?1049l") || s.contains("\u{1b}[?47l"),
        "restore must leave the alternate screen, got {s:?}"
    );
    assert!(
        s.contains("\u{1b}[?25h"),
        "restore must show the cursor, got {s:?}"
    );

    assert_eq!(
        wait_status(session),
        Some(7),
        "gutter must propagate the child's exit code"
    );
}

/// Block on the wrapped process and return its exit code, if any.
fn wait_status(session: OsSession) -> Option<i32> {
    use expectrl::process::unix::WaitStatus;
    use expectrl::process::Healthcheck;
    let proc = session.get_process();
    let start = Instant::now();
    loop {
        match proc.get_status() {
            Ok(WaitStatus::Exited(_, code)) => return Some(code),
            Ok(WaitStatus::Signaled(_, _, _)) => return None,
            _ => {}
        }
        if start.elapsed() > Duration::from_secs(5) {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// **Real-PTY smoke (cap).** Feed a few MB of scrolling output through a real
/// PTY: gutter must keep up, stay alive, drain in bounded time, and the settled
/// frame must show late flood output (not a frozen early frame). The exact
/// frame-count proof is the virtual-clock unit test; this is the real-PTY
/// sanity check that the coalescing loop neither hangs nor tears.
#[test]
fn multi_mb_scroll_stays_bounded() {
    // A burst of scrolling output, then a long idle so the child stays alive
    // PAST our capture window — we want the live alt-screen frame, not the
    // post-exit primary screen (leaving the alt screen discards its content).
    let child = "/bin/sh -c 'i=0; while [ $i -lt 20000 ]; do printf \"line %d of the flood test\\n\" $i; i=$((i+1)); done; sleep 6'";
    let cmd = gutter_in_terminal(120, 40, &format!("--width 100 --left {child}"));
    let mut session = spawn(cmd);

    let start = Instant::now();
    let bytes = drain_window(&mut session, Duration::from_secs(3));
    let elapsed = start.elapsed();

    // The capture window is a hard wall-clock cap (non-blocking reads), proving
    // the coalescing loop kept draining and never hung the reader.
    assert!(
        elapsed < Duration::from_secs(5),
        "flood capture window must stay bounded (took {elapsed:?})"
    );
    assert!(!bytes.is_empty(), "gutter must paint frames of the flood");

    let parser = outer_grid(&bytes, 120, 40);
    let screen = parser.screen();
    let shows_flood = screen
        .rows(0, 120)
        .any(|row| row.contains("of the flood test"));
    assert!(shows_flood, "the settled frame must show flood output");
    // Even under a flood the band edge holds — no bleed into the gutter.
    assert_gutters_empty(screen, 100, 120, 40);

    // Drop the session explicitly so the child (and its `sleep`) is reaped now.
    drop(session);
}

/// **Non-zero exit shows the dim `Exited with: N` status line (slice 02).** A
/// child that exits non-zero: gutter must, after leaving the alt screen (so the
/// line lands on the primary screen the user returns to), emit the dim
/// `\r\n\x1b[2mExited with: N\x1b[0m`. Asserted on the raw teardown bytes —
/// reading to the teardown, like the child-exit-restore test, and confirming the
/// status line lands *after* the alt-leave (`?1049l`).
#[test]
fn non_zero_exit_shows_dim_status_line() {
    let child = "/bin/sh -c 'exit 3'";
    let cmd = gutter_in_terminal(80, 24, &format!("--width 60 {child}"));
    let mut session = spawn(cmd);

    // Read to the teardown — we WANT the post-exit restore + status sequence.
    let bytes = drain_window(&mut session, Duration::from_secs(3));
    let s = String::from_utf8_lossy(&bytes);

    assert!(
        s.contains("\u{1b}[2mExited with: 3\u{1b}[0m"),
        "non-zero exit must emit the dim status line, got {s:?}"
    );
    // The status line lands AFTER the alt-leave (on the primary screen).
    let leave = s
        .find("\u{1b}[?1049l")
        .or_else(|| s.find("\u{1b}[?47l"))
        .expect("teardown must leave the alternate screen");
    let status = s
        .find("Exited with: 3")
        .expect("status line present");
    assert!(
        leave < status,
        "the status line must land after the alt-leave (on the primary screen)"
    );

    assert_eq!(
        wait_status(session),
        Some(3),
        "gutter must propagate the non-zero exit code"
    );
}

/// **Zero exit is silent (slice 02).** A child that exits cleanly: gutter must
/// emit NO status line — a clean run leaves a clean screen. Asserted on the raw
/// teardown bytes, which must still carry the alt-leave but no `Exited with:`.
#[test]
fn zero_exit_shows_no_status_line() {
    let child = "/bin/sh -c 'exit 0'";
    let cmd = gutter_in_terminal(80, 24, &format!("--width 60 {child}"));
    let mut session = spawn(cmd);

    let bytes = drain_window(&mut session, Duration::from_secs(3));
    let s = String::from_utf8_lossy(&bytes);

    // The teardown still ran (alt-leave present), but no status line at all.
    assert!(
        s.contains("\u{1b}[?1049l") || s.contains("\u{1b}[?47l"),
        "teardown must leave the alternate screen, got {s:?}"
    );
    assert!(
        !s.contains("Exited with:"),
        "a zero exit must emit no status line, got {s:?}"
    );

    assert_eq!(
        wait_status(session),
        Some(0),
        "gutter must propagate the zero exit code"
    );
}
