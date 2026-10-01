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

mod common;
use common::{find, outer_bytes_for, screen_text, Gutter};

/// **A kitty push reaches the real terminal.** The child asks its terminal to start
/// reporting keys in the richer form; those exact bytes must appear on the outer
/// stream, because gutter is not the terminal that can honour them.
#[test]
fn kitty_push_reaches_the_outer_terminal() {
    let out = outer_bytes_for("\\033[>1u");
    assert!(
        find(&out, b"\x1b[>1u").is_some(),
        "the child's kitty push must be relayed verbatim"
    );
}

/// **modifyOtherKeys reaches the real terminal.** The request that makes a terminal
/// report Shift+Enter as something other than a plain `\r`. A terminal that is never
/// told carries on sending the plain `\r`, and the child cannot tell the two Enters
/// apart.
#[test]
fn modify_other_keys_reaches_the_outer_terminal() {
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
/// undefined. And gutter must invent no answer: the child reads up to the first `u`
/// and reports `NOREPLY` only if what it read is the `X` the test typed, which is
/// what a terminal that does not speak the protocol looks like, and the only honest
/// thing gutter can say on its behalf.
///
/// gutter writes a reply as it parses the query and paints `READY` afterwards, so an
/// invented answer would be in the child's input ahead of anything typed once
/// `READY` is on screen.
#[test]
fn the_kitty_query_is_forwarded_canonically_and_unanswered() {
    let child = "bash -c 'printf \"\\033[?uREADY\"; IFS= read -rs -d u got; if [ \"$got\" = X ]; then printf NOREPLY; else printf REPLY; fi; read _'";
    let mut gutter = Gutter::spawn(80, 24, &format!("--width 40 {child}"));
    gutter.wait_for("the child's READY", |s| s.contents().contains("READY"));

    gutter.send(b"Xu");
    gutter.wait_for("the child's verdict", |s| s.contents().contains("REPLY"));
    gutter.send(b"\r");
    let done = gutter.finish();

    assert!(
        find(&done.bytes, b"\x1b[?u").is_some(),
        "the query must reach the terminal as the spec's CSI ? u"
    );
    assert!(
        find(&done.bytes, b"\x1b[?0u").is_none(),
        "CSI ? 0 u is not the query — re-serialising the parameters would send it"
    );
    let text = screen_text(&done.screen, 80);
    assert!(
        text.contains("NOREPLY"),
        "gutter must answer the capability query with silence; band was {text:?}"
    );
}

/// **A terminal's answer transits back to the child.** The test writes the reply a
/// kitty-capable terminal would send into gutter's input fd; the child echoes what
/// reaches it in caret notation. This is the inward half of the proxy, and it needs
/// no relay code at all — the reply is just bytes on the input fd, and the raw
/// passthrough (ADR-020) carries them.
///
/// `READY` follows the child's `stty raw`. A raw-mode `cat` has no byte that ends
/// it, so the echo is checked on the live screen and the session dropped.
#[test]
fn a_terminal_reply_transits_back_to_the_child() {
    let mut gutter = Gutter::spawn(
        80,
        24,
        "--width 40 sh -c 'stty raw -echo; printf READY; exec cat -v'",
    );
    gutter.wait_for("the child's READY", |s| s.contents().contains("READY"));

    // What a terminal that speaks the protocol answers `CSI ? u` with.
    gutter.send(b"\x1b[?1u");
    gutter.wait_for("the terminal's reply reaching the child unchanged", |s| {
        s.contents().contains("^[[?1u")
    });
}

/// **Teardown pops what the child pushed.** A child that pushes a kitty level and
/// exits must not leave the user's shell with key reporting live — Escape would
/// start arriving as `CSI 27 u`. The pop lands before the rest of the restore.
#[test]
fn teardown_pops_the_childs_kitty_level() {
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
    let out = outer_bytes_for("hello");

    assert!(find(&out, b"hello").is_some(), "the child ran");
    for stray in [b"\x1b[<1u".as_slice(), b"\x1b[>4;0m", b"\x1b[=0;1u"] {
        assert!(
            find(&out, stray).is_none(),
            "nothing to undo, so nothing is written: {stray:?}"
        );
    }
}
