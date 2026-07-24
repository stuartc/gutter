//! Keyboard-mode relay, end to end through a real PTY (ADR-021).
//!
//! The outer side here is a kernel PTY the test owns, so the test can do both
//! halves of a terminal's job: read what gutter wrote outward, and write bytes back
//! as if it had answered. That is enough to prove everything on gutter's side of
//! the boundary — the right bytes go out, no bytes are invented, incoming bytes
//! transit unchanged, and the modes are undone at exit.
//!
//! **What it cannot prove**, because the test terminal is a kernel PTY and not
//! iTerm2: that Shift+Enter arrives as `ESC[13;2u`. A kernel PTY has no
//! key-reporting behaviour, implements neither protocol, and answers nothing unless
//! the test writes the answer itself. Whether a relayed mode request actually
//! changes what a terminal sends is a property of the terminal, and stays on the
//! manual checklist.
//!
//! Headless: a real PTY, no display, `TERM=xterm-256color`.

use std::io::Write;
use std::time::Duration;

mod common;
use common::{drain_window, find, outer_bytes_for, pty_guard, spawn_gutter};

/// The band as the outer terminal rendered it, for the cases that assert on what a
/// child printed rather than on raw escape bytes.
fn band_text(bytes: &[u8]) -> String {
    common::grid_text(bytes, 80, 24)
}

/// **A kitty push reaches the real terminal.** The child asks its terminal to start
/// reporting keys in the richer form; those exact bytes must appear on the outer
/// stream, because gutter is not the terminal that can honour them.
#[test]
fn kitty_push_reaches_the_outer_terminal() {
    let _g = pty_guard();
    let out = outer_bytes_for("\\033[>1u");
    assert!(
        find(&out, b"\x1b[>1u").is_some(),
        "the child's kitty push must be relayed verbatim"
    );
}

/// **modifyOtherKeys reaches the real terminal.** This is the sequence gutter
/// dropped on the floor entirely, and the direct cause of the reported Shift+Enter
/// bug: iTerm2 was never told, so it carried on sending a plain `\r`.
#[test]
fn modify_other_keys_reaches_the_outer_terminal() {
    let _g = pty_guard();
    let out = outer_bytes_for("\\033[>4;2m");
    assert!(
        find(&out, b"\x1b[>4;2m").is_some(),
        "the child's modifyOtherKeys request must be relayed verbatim"
    );
}

/// **The capability query is forwarded, canonically, and not answered.** The
/// automated stand-in for phase 2 of the diagnostic probe.
///
/// Two assertions, and both matter. The query must go out as the spec's paramless
/// `CSI ? u` — vte hands the callback a zero for the parameter the child never
/// wrote, so an emitter that re-serialised would send `CSI ? 0 u`, whose meaning is
/// undefined. And gutter must invent no answer: the child reads with a timeout and
/// reports `NOREPLY`, which is what a terminal that does not speak the protocol
/// looks like, and the only honest thing gutter can say on its behalf.
#[test]
fn the_kitty_query_is_forwarded_canonically_and_unanswered() {
    let _g = pty_guard();
    let child = "bash -c 'printf \"\\033[?u\"; if IFS= read -rs -d u -t 1 _; then printf REPLY; else printf NOREPLY; fi; sleep 0.4'";
    let mut session = spawn_gutter(80, 24, &format!("--width 40 {child}"));

    let out = drain_window(&mut session, Duration::from_secs(3));

    assert!(
        find(&out, b"\x1b[?u").is_some(),
        "the query must reach the terminal as the spec's CSI ? u"
    );
    assert!(
        find(&out, b"\x1b[?0u").is_none(),
        "CSI ? 0 u is not the query — re-serialising the parameters would send it"
    );
    let text = band_text(&out);
    assert!(
        text.contains("NOREPLY"),
        "gutter must answer the capability query with silence; band was {text:?}"
    );

    drop(session);
}

/// **A terminal's answer transits back to the child.** The test writes the reply a
/// kitty-capable terminal would send into gutter's input fd; the child echoes what
/// reaches it in caret notation. This closes the loop the whole slice rests on, and
/// it needs no relay code at all — the reply is just bytes on the input fd, and the
/// raw passthrough (ADR-020) carries them.
#[test]
fn a_terminal_reply_transits_back_to_the_child() {
    let _g = pty_guard();
    let mut session = spawn_gutter(80, 24, "--width 40 sh -c 'stty raw -echo; exec cat -v'");
    std::thread::sleep(Duration::from_millis(400));

    // What a terminal that speaks the protocol answers `CSI ? u` with.
    session.write_all(b"\x1b[?1u").expect("write the terminal's reply");
    session.flush().unwrap();

    let out = drain_window(&mut session, Duration::from_millis(700));
    let text = band_text(&out);
    assert!(
        text.contains("^[[?1u"),
        "the terminal's reply must reach the child unchanged; band was {text:?}"
    );

    drop(session);
}

/// **Teardown pops what the child pushed.** A child that pushes a kitty level and
/// exits must not leave the user's shell with key reporting live — Escape would
/// start arriving as `CSI 27 u`. The pop lands before the rest of the restore.
#[test]
fn teardown_pops_the_childs_kitty_level() {
    let _g = pty_guard();
    let out = outer_bytes_for("\\033[>1u");

    let pop = find(&out, b"\x1b[<1u").expect("teardown must pop the child's level");
    let mouse_off = find(&out, b"\x1b[?1000l").expect("teardown must disable mouse");
    assert!(
        pop < mouse_off,
        "the mode reset comes before the mouse disable (ADR-010 order)"
    );
}

/// A child that never negotiates leaves the outer terminal's own keyboard settings
/// alone — gutter resets no mode it did not set.
#[test]
fn a_child_that_negotiates_nothing_relays_nothing() {
    let _g = pty_guard();
    let out = outer_bytes_for("hello");

    assert!(find(&out, b"hello").is_some(), "the child ran");
    for stray in [b"\x1b[<1u".as_slice(), b"\x1b[>4;0m", b"\x1b[=0;1u"] {
        assert!(
            find(&out, stray).is_none(),
            "nothing to undo, so nothing is written: {stray:?}"
        );
    }
}
