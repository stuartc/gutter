//! SGR mouse forwarding PTY round-trips.
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
//! very bytes gutter's scanner extracts — gutter subtracts the live
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
use std::time::Duration;

mod common;
use common::{drain_window, grid_text, pty_guard, screen_text, spawn_gutter, Gutter};

/// A child that negotiates SGR press/release mouse, then idles. The tty driver is
/// in cooked mode, so any bytes gutter forwards to the child's stdin are echoed
/// back in caret notation (`ESC` → `^[`) and rendered onto gutter's band — the
/// child-side oracle. The long `sleep` keeps the child alive so the echo stays on
/// the live alt-screen frame the drain captures.
const SGR_MOUSE_CHILD: &str = "/bin/sh -c 'printf \"\\033[?1000h\\033[?1006h\"; sleep 5'";

/// The same child, announcing itself and then blocking on a line of input instead of
/// idling. `READY` follows the mode requests in the one `printf`, and gutter paints
/// only what it has already parsed, so `READY` on screen means the gate will forward.
/// Enter ends the child; its echo is on the primary screen and survives the exit.
const GATED_MOUSE_CHILD: &str =
    "/bin/sh -c 'printf \"\\033[?1000h\\033[?1006hREADY\"; read _'";

/// **First post-negotiation click delivered AND margin subtracted.** The child
/// negotiates SGR mouse; gutter runs centred in a 120-col outer with `--width 40`
/// (margin `(120 - 40) / 2 = 40`). A physical click at wire col 46 (0-based phys
/// 45) row 5 → child 0-based col `45 - 40 = 5` → SGR wire `CSI < 0 ; 6 ; 5 M`,
/// echoed as `^[[<0;6;5M`. Eager capture means this is the very first click after
/// negotiation — the dropped-first-click race is gone by construction.
#[test]
fn first_click_delivered_with_margin_subtracted() {
    let mut gutter = Gutter::spawn(120, 40, &format!("--width 40 --center {GATED_MOUSE_CHILD}"));
    gutter.wait_for("the child's READY", |s| s.contents().contains("READY"));

    // A real SGR 1006 mouse press at wire col 46, row 5. The scanner extracts it
    // at 0-based physical (45, 4); margin 40 → child 0-based col `45 - 40 = 5` →
    // SGR wire col 6.
    gutter.send(b"\x1b[<0;46;5M");
    // A report ends in `M`, whatever its coordinates. Seen before the child is let
    // go: an echo still in flight when the child exits is only painted if it reaches
    // gutter within its teardown grace.
    gutter.wait_for("the echo of a mouse report", |s| s.contents().ends_with('M'));
    gutter.send(b"\n");

    let done = gutter.finish();
    let text = screen_text(&done.screen, 120);
    // The child received `CSI < 0 ; 6 ; 5 M`, echoed in caret notation.
    assert!(
        text.contains("^[[<0;6;5M"),
        "child must receive the margin-subtracted SGR click \
         (CSI < 0 ; 6 ; 5 M, echoed ^[[<0;6;5M); grid was {text:?}"
    );
}

/// **Gutter click ignored.** A click left of the band (wire col 6, while the
/// margin is 40) is in the left gutter → discarded, the child receives nothing,
/// so no echoed SGR report appears on the band. The text typed after the click is
/// echoed, and input reaches the child in order, so the click was handled by then.
#[test]
fn gutter_click_delivers_nothing() {
    let mut gutter = Gutter::spawn(120, 40, &format!("--width 40 --center {GATED_MOUSE_CHILD}"));
    gutter.wait_for("the child's READY", |s| s.contents().contains("READY"));

    // Click at wire col 6 (0-based phys 5) — left of the margin-40 band.
    gutter.send(b"\x1b[<0;6;5M");
    gutter.send(b"after");
    gutter.wait_for("the echo of the text typed after the click", |s| {
        s.contents().contains("after")
    });
    gutter.send(b"\n");

    let done = gutter.finish();
    let text = screen_text(&done.screen, 120);
    assert!(
        text.contains("after"),
        "the text typed after the click must reach the child; grid was {text:?}"
    );
    // No echoed SGR report (`^[[<` is the start of an SGR report) — the gutter
    // click was discarded, so nothing reached the child to echo.
    assert!(
        !text.contains("^[[<"),
        "a gutter click must deliver nothing to the child; grid was {text:?}"
    );
}

/// **A Shift-click keeps its modifier bit.** The SGR button byte carries the
/// modifiers in bits 2–4, and gutter forwards the byte verbatim rather than
/// rebuilding it from a decoded button — so a Shift-click (button `0 | 4`) must
/// reach the child as button 4, not as a plain click. Rebuilding the byte from a
/// decoded button is what loses it.
#[test]
fn shift_click_keeps_the_modifier_bit() {
    let _g = pty_guard();
    let mut session = spawn_gutter(120, 40, &format!("--width 40 --center {SGR_MOUSE_CHILD}"));
    std::thread::sleep(Duration::from_millis(600));

    session.write_all(b"\x1b[<4;46;5M").unwrap();
    session.flush().unwrap();

    let bytes = drain_window(&mut session, Duration::from_millis(900));
    let text = grid_text(&bytes, 120, 40);
    assert!(
        text.contains("^[[<4;6;5M"),
        "the shift bit must survive the margin translation; grid was {text:?}"
    );

    drop(session);
}
