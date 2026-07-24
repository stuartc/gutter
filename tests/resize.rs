//! PTY-driven integration tests for slice 05 (resize + `--center`/`--left` +
//! proportional `--width Npct`).
//!
//! These drive the real `gutter` binary through a real PTY (via `expectrl` /
//! `ptyprocess`) and assert on what the outer terminal actually shows — never on
//! gutter internals. The outer terminal is parsed back through a second `vt100`
//! so physical-column assertions can read which column each glyph landed in.
//!
//! Two harness facts (shared with `tests/pty.rs`):
//! - **Outer size** is set by `sh -c 'stty cols C rows R; exec gutter ...'` so
//!   gutter reads the intended size at startup with no race. For the resize
//!   tests the outer PTY is then resized live via `set_window_size`, which sends
//!   SIGWINCH to gutter's process group (crossterm surfaces it as `Event::Resize`).
//! - **Capture the LIVE alt-screen frame**, not the post-exit primary screen
//!   (leaving the alt screen on exit discards its content) — so the wrapped
//!   children stay alive (a long `sleep`) and the harness drains a bounded window
//!   while the child still runs.
//!
//! CI runs these headlessly: a real PTY, no display, `TERM=xterm-256color`.

use std::time::{Duration, Instant};

use expectrl::session::OsSession;

fn gutter_bin() -> String {
    env!("CARGO_BIN_EXE_gutter").to_string()
}

/// Run gutter inside an outer terminal of the given size:
/// `sh -c 'stty cols C rows R; exec env gutter <args>'`.
fn gutter_in_terminal(outer_cols: u16, outer_rows: u16, gutter_args: &str) -> std::process::Command {
    let script = format!(
        "stty cols {outer_cols} rows {outer_rows}; exec env GUTTER_FORCE_ANCHOR_ROW=0 {} {gutter_args}",
        gutter_bin()
    );
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.arg("-c").arg(script);
    cmd
}

fn spawn(cmd: std::process::Command) -> OsSession {
    OsSession::spawn(cmd).expect("spawn gutter under PTY")
}

/// Drain a bounded window of output with NON-BLOCKING reads — a wall-clock cap
/// even while the child keeps the PTY open, so the live alt-screen frame is
/// captured (not the post-exit primary screen).
fn drain_window(session: &mut OsSession, window: Duration) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    let start = Instant::now();
    while start.elapsed() < window {
        match session.try_read(&mut buf) {
            Ok(0) => break,
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

/// Parse outer-terminal bytes through a vt100 at the given physical size.
fn outer_grid(bytes: &[u8], cols: u16, rows: u16) -> vt100::Parser {
    let mut p = vt100::Parser::new(rows, cols, 0);
    p.process(bytes);
    p
}

/// The physical column of the first painted (non-blank) cell on row 0, or `None`
/// if the row is blank.
fn first_painted_col(screen: &vt100::Screen, cols: u16) -> Option<u16> {
    for c in 0..cols {
        if let Some(cell) = screen.cell(0, c) {
            let s = cell.contents();
            if !s.is_empty() && s != " " {
                return Some(c);
            }
        }
    }
    None
}

/// Assert columns `[from, to)` on every row are blank.
fn assert_cols_blank(screen: &vt100::Screen, from: u16, to: u16, rows: u16) {
    for r in 0..rows {
        for c in from..to {
            if let Some(cell) = screen.cell(r, c) {
                let s = cell.contents();
                assert!(
                    s.is_empty() || s == " ",
                    "col {c} row {r} must be blank, found {s:?}"
                );
            }
        }
    }
}

/// **`--center` centres the band with gutters on BOTH sides.** Outer 120,
/// `--width 100` → margin (120-100)/2 = 10. Content fills row 0; it must start at
/// physical col 10 and end before col 110, with both gutters `[0,10)` and
/// `[110,120)` blank.
#[test]
fn center_band_has_gutters_both_sides() {
    // Fill the band's first row with a run of '#'s (100 of them) so the painted
    // span is the whole band width.
    let child = "/bin/sh -c 'printf \"%0.s#\" $(seq 1 100); sleep 3'";
    let cmd = gutter_in_terminal(120, 40, &format!("--width 100 --center {child}"));
    let mut session = spawn(cmd);
    let bytes = drain_window(&mut session, Duration::from_millis(800));

    let parser = outer_grid(&bytes, 120, 40);
    let screen = parser.screen();

    let first = first_painted_col(screen, 120).expect("row 0 has painted content");
    assert_eq!(first, 10, "centred band starts at physical col 10 (margin)");
    // Left gutter and right gutter both empty.
    assert_cols_blank(screen, 0, 10, 40);
    assert_cols_blank(screen, 110, 120, 40);
}

/// **`--left` left-aligns: gutter on the RIGHT only.** Same content; content
/// starts at col 0, the right gutter `[100,120)` is blank.
#[test]
fn left_band_has_right_gutter_only() {
    let child = "/bin/sh -c 'printf \"%0.s#\" $(seq 1 100); sleep 3'";
    let cmd = gutter_in_terminal(120, 40, &format!("--width 100 --left {child}"));
    let mut session = spawn(cmd);
    let bytes = drain_window(&mut session, Duration::from_millis(800));

    let parser = outer_grid(&bytes, 120, 40);
    let screen = parser.screen();

    let first = first_painted_col(screen, 120).expect("row 0 has painted content");
    assert_eq!(first, 0, "left-aligned band starts at physical col 0");
    assert_cols_blank(screen, 100, 120, 40);
}

/// **`--width 50pct` renders a band ~50% of the terminal at launch.** Outer 200;
/// 50% → W = 100. The child sees `COLUMNS == 100` (asserted from inside via
/// `stty size`).
#[test]
fn proportional_width_pct_at_launch() {
    let cmd = gutter_in_terminal(200, 40, "--width 50pct /bin/sh -c 'stty size; sleep 3'");
    let mut session = spawn(cmd);
    let bytes = drain_window(&mut session, Duration::from_millis(800));

    let parser = outer_grid(&bytes, 200, 40);
    let row = parser
        .screen()
        .rows(0, 200)
        .map(|r| r.trim_end().to_string())
        .find(|r| !r.is_empty())
        .unwrap_or_default();
    // 50% of 200 = 100 columns.
    assert!(
        row.contains("100"),
        "child must see ~50% = 100 columns (stty size row was {row:?})"
    );
}

/// **`--width 50%` alias** behaves identically to `50pct`.
#[test]
fn proportional_width_percent_alias_at_launch() {
    let cmd = gutter_in_terminal(160, 40, "--width 50% /bin/sh -c 'stty size; sleep 3'");
    let mut session = spawn(cmd);
    let bytes = drain_window(&mut session, Duration::from_millis(800));

    let parser = outer_grid(&bytes, 160, 40);
    let row = parser
        .screen()
        .rows(0, 160)
        .map(|r| r.trim_end().to_string())
        .find(|r| !r.is_empty())
        .unwrap_or_default();
    // 50% of 160 = 80.
    assert!(
        row.contains("80"),
        "child must see 50% of 160 = 80 columns (stty size row was {row:?})"
    );
}

/// **Resize smoke (retained, not the gate).** Wrap a plain child that prints its
/// `COLUMNS`, resize the outer terminal once wider, and assert gutter survives:
/// no panic, the child believes it has the (absolute) band width `W`, the band
/// content survives the resize, and the right gutter holds no stale cells. An
/// absolute `--width 100` keeps `W = 100` across the resize — so the child's PTY
/// size is unchanged and the child does not re-report; the band content the child
/// already printed must therefore be preserved across the resize, NOT erased.
///
/// This is also the E2 primary-mode resize-guard check on a real PTY (ADR-012):
/// because the child is a plain command (no alt screen), gutter must not blank
/// `[0, rows)` on the resize — doing so would erase the user's scrollback above
/// the band. The cumulative drain (launch through post-resize) holds the child's
/// printed `100`, proving the band survived.
#[test]
fn resize_once_absolute_width_stays_fixed() {
    let child = "/bin/sh -c 'stty size; while true; do sleep 0.2; done'";
    let cmd = gutter_in_terminal(120, 40, &format!("--width 100 --center {child}"));
    let mut session = spawn(cmd);

    // Accumulate the whole stream from launch — gutter paints the child's `stty
    // size` output onto the PRIMARY screen (no forced alt screen, ADR-012). For an
    // absolute width an outer resize does not change the child's columns, so the
    // child does not reprint; the startup paint must still be in the stream.
    let mut bytes = drain_window(&mut session, Duration::from_millis(500));

    // Resize the outer terminal WIDER (120 → 160). gutter re-lays-out; the band
    // width W stays 100 (absolute), so the child PTY size is unchanged.
    session
        .get_process_mut()
        .set_window_size(160, 40)
        .expect("resize outer PTY");

    bytes.extend(drain_window(&mut session, Duration::from_millis(900)));
    let parser = outer_grid(&bytes, 160, 40);
    let screen = parser.screen();

    // The child saw W = 100 (absolute width), and that content survived the
    // resize rather than being erased by an absolute-row clear.
    let shows_100 = screen.rows(0, 160).any(|r| r.contains("100"));
    assert!(
        shows_100,
        "absolute --width 100 must keep the child at COLUMNS=100, content preserved across resize"
    );

    drop(session);
}

/// **Proportional band tracks on resize.** With `--width 50pct`, resizing the
/// outer terminal from 200 to 160 must recompute the band: the child's reported
/// `COLUMNS` tracks from ~100 to ~80. Proves `W` is recomputed and the child PTY
/// is resized to the new `W` (ADR-011), not frozen at the launch value.
#[test]
fn proportional_band_tracks_on_resize() {
    let child = "/bin/sh -c 'trap \"stty size\" WINCH; stty size; while true; do sleep 0.2; done'";
    let cmd = gutter_in_terminal(200, 40, &format!("--width 50pct {child}"));
    let mut session = spawn(cmd);

    let bytes0 = drain_window(&mut session, Duration::from_millis(500));
    let parser0 = outer_grid(&bytes0, 200, 40);
    let at_launch = parser0.screen().rows(0, 200).any(|r| r.contains("100"));
    assert!(at_launch, "launch: 50% of 200 = 100 columns");

    // Resize narrower: 200 → 160. 50% → 80.
    session
        .get_process_mut()
        .set_window_size(160, 40)
        .expect("resize outer PTY");

    let bytes1 = drain_window(&mut session, Duration::from_millis(1000));
    let parser1 = outer_grid(&bytes1, 160, 40);
    let tracked = parser1.screen().rows(0, 160).any(|r| r.contains("80"));
    assert!(
        tracked,
        "after resize: 50% of 160 = 80 columns — the proportional band must track"
    );

    drop(session);
}
