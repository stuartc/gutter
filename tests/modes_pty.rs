//! Absorbed-mode mirroring, end to end through a real PTY (ADR-022).
//!
//! The child asks its terminal for application cursor keys, an application keypad or
//! bracketed paste; those requests terminate in gutter's vt100 parser, so gutter has
//! to re-emit them outward or the real terminal never hears them. These tests read
//! gutter's outer stream and assert exactly that — the request went out, nothing else
//! did, and what gutter set is turned back off at exit.
//!
//! **What they cannot prove.** The outer side here is a kernel PTY, which is a byte
//! pipe and not an emulator: it does not implement DECCKM, so it will never turn an
//! arrow key into `ESC O A` no matter what gutter sends it. Whether a mirrored mode
//! changes what a terminal produces is a property of the terminal. Closing that loop
//! would need a second emulator on the outer side of the harness, which is out of all
//! proportion to what it would buy.
//!
//! Headless: a real PTY, no display, `TERM=xterm-256color`.

mod common;
use common::{find, outer_bytes_for};

/// **The three mirrored modes reach the real terminal.** All gutter is responsible
/// for; what the terminal does with them is the terminal's business.
#[test]
fn each_mirrored_mode_reaches_the_outer_terminal() {
    for (emit, want) in [
        ("\\033[?1h", b"\x1b[?1h".as_slice()),
        ("\\033=", b"\x1b="),
        ("\\033[?2004h", b"\x1b[?2004h"),
    ] {
        let out = outer_bytes_for(emit);
        assert!(
            find(&out, want).is_some(),
            "the child's {want:?} must reach the terminal"
        );
    }
}

/// **What gutter turned on, gutter turns off.** A child that leaves bracketed paste
/// set must not hand the user's shell a terminal that brackets its pastes for a
/// program that never asked.
#[test]
fn teardown_turns_off_what_was_mirrored_on() {
    let out = outer_bytes_for("\\033[?2004h");

    let on = find(&out, b"\x1b[?2004h").expect("the mode must be mirrored on");
    let off = find(&out[on..], b"\x1b[?2004l").expect("teardown must turn it off");
    let mouse_off = find(&out[on..], b"\x1b[?1000l").expect("teardown must disable mouse");
    assert!(
        off < mouse_off,
        "the mode reset comes before the mouse disable (ADR-010 order)"
    );
}

/// **A mode gutter never mirrored on is never touched.** `?1` and `?2004` off forms
/// must not appear for a child that set neither.
#[test]
fn a_child_that_sets_no_mode_leaves_the_terminal_alone() {
    let out = outer_bytes_for("hello");

    assert!(find(&out, b"hello").is_some(), "the child ran");
    for stray in [b"\x1b[?1l".as_slice(), b"\x1b[?2004l", b"\x1b>"] {
        assert!(
            find(&out, stray).is_none(),
            "nothing mirrored, nothing reset: {stray:?}"
        );
    }
}

/// **The mouse negative, end to end.** `?9` is the right probe: vt100 implements it,
/// so it is absorbed like the mirrored three, and gutter's own eager capture never
/// emits it — so there is no startup noise to filter out the way `?1000h` would have.
#[test]
fn a_mouse_mode_never_reaches_the_outer_terminal() {
    let out = outer_bytes_for("\\033[?9h");
    assert!(
        find(&out, b"\x1b[?9h").is_none(),
        "mouse modes stay absorbed (ADR-005); gutter owns the outer terminal's"
    );
}

/// **The no-relay negative, end to end.** There is no forwarding path for private
/// modes, so a DECSET vt100 does not implement is dropped rather than forwarded —
/// and `?1047` in particular would take the outer alt screen away from the mirror
/// that owns it (ADR-012).
#[test]
fn an_unimplemented_decset_is_not_relayed() {
    let out = outer_bytes_for("\\033[?1047h");
    assert!(
        find(&out, b"\x1b[?1047h").is_none(),
        "no DECSET is ever relayed"
    );
    assert!(
        find(&out, b"\x1b[?1049h").is_none(),
        "and the outer terminal does not enter the alt screen"
    );
}
