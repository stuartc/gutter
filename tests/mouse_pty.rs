//! SGR mouse forwarding PTY round-trips (slice 07 acceptance criteria).
//!
//! Mouse reports are the one keystroke-shaped thing gutter does NOT pass through:
//! their coordinates carry the band's left margin, so gutter's own scanner
//! extracts them and re-encodes (ADR-005/020). These tests assert on what the
//! **child actually receives** — never the grid. The child is a tiny shell that
//! negotiates SGR mouse (`CSI ?1000h ?1006h`) and then idles in line-discipline
//! cooked mode, where the tty driver **echoes** the bytes it receives back in
//! caret notation (`ESC` → `^[`). So the SGR report gutter forwards shows up as
//! visible `^[[<…M` text on the outer frame — an unambiguous child-side oracle of
//! exactly which bytes reached the child (the raw `ESC[<…M` are an input report,
//! not renderable output, so the child must surface them some printable way; the
//! tty echo does that for free).
//!
//! The outer test writes a real SGR 1006 mouse sequence into the session — the
//! very bytes gutter's scanner now parses — gutter subtracts the live
//! `left_margin` and re-encodes, and the child's echoed bytes are the oracle.
//! Eager capture (ADR-005) means the FIRST click after the child's negotiation is
//! the one asserted — no dropped-first-click warm-up.
//!
//! Byte-exactness of the translation + encode + down-filter is proven
//! deterministically by the in-crate unit/proptest seams and the through-dispatch
//! tests in `src/render.rs` (which assert on the bytes written to the PTY-master
//! writer — the child-received bytes — along the real `process` → poll → gate
//! path). These PTY tests layer the end-to-end leg on top: real binary, real PTY,
//! headless.
//!
//! CI runs these headlessly: a real PTY, no display, `TERM=xterm-256color`.

use std::io::Write;
use std::time::{Duration, Instant};

use expectrl::session::OsSession;

fn gutter_bin() -> String {
    env!("CARGO_BIN_EXE_gutter").to_string()
}

/// Run gutter inside an outer terminal of a fixed size.
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

/// Drain a bounded window with non-blocking reads, so the window is a real
/// wall-clock cap even while the child keeps the PTY open.
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

fn outer_grid(bytes: &[u8], cols: u16, rows: u16) -> vt100::Parser {
    let mut p = vt100::Parser::new(rows, cols, 0);
    p.process(bytes);
    p
}

fn grid_text(screen: &vt100::Screen, cols: u16, rows: u16) -> String {
    (0..rows)
        .map(|r| screen.rows(0, cols).nth(r as usize).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A child that negotiates SGR press/release mouse, then idles. The tty driver is
/// in cooked mode, so any bytes gutter forwards to the child's stdin are echoed
/// back in caret notation (`ESC` → `^[`) and rendered onto gutter's band — the
/// child-side oracle. The long `sleep` keeps the child alive so the echo stays on
/// the live alt-screen frame the drain captures.
fn sgr_mouse_child() -> String {
    "/bin/sh -c 'printf \"\\033[?1000h\\033[?1006h\"; sleep 5'".to_string()
}

/// **First post-negotiation click delivered AND margin subtracted.** The child
/// negotiates SGR mouse; gutter runs centred in a 120-col outer with `--width 40`
/// (margin `(120 - 40) / 2 = 40`). A physical click at wire col 46 (0-based phys
/// 45) row 5 → child 0-based col `45 - 40 = 5` → SGR wire `CSI < 0 ; 6 ; 5 M`,
/// echoed as `^[[<0;6;5M`. Eager capture means this is the very first click after
/// negotiation — the dropped-first-click race is gone by construction.
#[test]
fn first_click_delivered_with_margin_subtracted() {
    let child = sgr_mouse_child();
    let cmd = gutter_in_terminal(120, 40, &format!("--width 40 --center {child}"));
    let mut session = spawn(cmd);
    // Let gutter come up and the child negotiate mouse (vt100 must see the DECSET
    // so the gate forwards — eager capture is already on the outer terminal).
    std::thread::sleep(Duration::from_millis(600));

    // A real SGR 1006 mouse press at wire col 46, row 5. The scanner extracts it
    // at 0-based physical (45, 4); margin 40 → child 0-based col `45 - 40 = 5` →
    // SGR wire col 6.
    session.write_all(b"\x1b[<0;46;5M").unwrap();
    session.flush().unwrap();

    let bytes = drain_window(&mut session, Duration::from_millis(900));
    let parser = outer_grid(&bytes, 120, 40);
    let text = grid_text(parser.screen(), 120, 40);
    // The child received `CSI < 0 ; 6 ; 5 M`, echoed in caret notation.
    assert!(
        text.contains("^[[<0;6;5M"),
        "child must receive the margin-subtracted SGR click \
         (CSI < 0 ; 6 ; 5 M, echoed ^[[<0;6;5M); grid was {text:?}"
    );

    drop(session);
}

/// **Gutter click ignored.** A click left of the band (wire col 6, while the
/// margin is 40) is in the left gutter → discarded, the child receives nothing,
/// so no echoed SGR report appears on the band.
#[test]
fn gutter_click_delivers_nothing() {
    let child = sgr_mouse_child();
    let cmd = gutter_in_terminal(120, 40, &format!("--width 40 --center {child}"));
    let mut session = spawn(cmd);
    std::thread::sleep(Duration::from_millis(600));

    // Click at wire col 6 (0-based phys 5) — left of the margin-40 band.
    session.write_all(b"\x1b[<0;6;5M").unwrap();
    session.flush().unwrap();

    let bytes = drain_window(&mut session, Duration::from_millis(700));
    let parser = outer_grid(&bytes, 120, 40);
    let text = grid_text(parser.screen(), 120, 40);
    // No echoed SGR report (`^[[<` is the start of an SGR report) — the gutter
    // click was discarded, so nothing reached the child to echo.
    assert!(
        !text.contains("^[[<"),
        "a gutter click must deliver nothing to the child; grid was {text:?}"
    );

    drop(session);
}

/// **A Shift-click keeps its modifier bit.** The SGR button byte carries the
/// modifiers in bits 2–4, and gutter forwards the byte verbatim rather than
/// rebuilding it from a decoded button — so a Shift-click (button `0 | 4`) must
/// reach the child as button 4, not as a plain click. Rebuilding the byte from a
/// decoded button is what loses it.
#[test]
fn shift_click_keeps_the_modifier_bit() {
    let child = sgr_mouse_child();
    let cmd = gutter_in_terminal(120, 40, &format!("--width 40 --center {child}"));
    let mut session = spawn(cmd);
    std::thread::sleep(Duration::from_millis(600));

    session.write_all(b"\x1b[<4;46;5M").unwrap();
    session.flush().unwrap();

    let bytes = drain_window(&mut session, Duration::from_millis(900));
    let parser = outer_grid(&bytes, 120, 40);
    let text = grid_text(parser.screen(), 120, 40);
    assert!(
        text.contains("^[[<4;6;5M"),
        "the shift bit must survive the margin translation; grid was {text:?}"
    );

    drop(session);
}
