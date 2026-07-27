//! Keyboard passthrough PTY round-trips (ADR-020).
//!
//! The claim these prove is the whole point of the raw-input design:
//!
//! > **The bytes written to the outer tty arrive at the child's PTY unchanged.**
//!
//! The child is `sh -c 'stty raw -echo; exec cat -v'`. `stty raw` on the child's
//! own tty stops the line discipline mangling `\r`, `0x03` and friends before
//! `cat` sees them; `-echo` stops the discipline echoing, so the only thing on
//! screen is `cat`'s output; and `cat -v` renders control and escape bytes in
//! caret notation (`ESC` → `^[`), so the transcript is visible glyphs on gutter's
//! band. `tests/mouse_pty.rs` already relies on the same caret-notation trick.
//!
//! **What these tests cannot prove**, stated plainly so nobody reads more into a
//! green run than is there:
//!
//! - **Not that Shift+Enter works in iTerm2.** `expectrl` writes bytes; it does
//!   not press keys. These prove that *if* the terminal sends `ESC[13;2u`, the
//!   child receives it. Whether the terminal sends that depends on the terminal
//!   honouring a relayed mode request, which only a real terminal can do. That
//!   leg stays on the manual checklist.
//! - **Nothing about timing.** The ESC-hold's added latency is invisible here by
//!   design — the bytes arrive, just later. The virtual-clock tests in
//!   `src/render.rs` cover the hold itself.
//!
//! CI runs these headlessly: a real PTY, no display, `TERM=xterm-256color`.

use std::io::Write;
use std::time::Duration;

mod common;
use common::{drain_window, pty_guard, read_until, spawn_gutter};

/// The outer terminal these tests run gutter in.
const OUTER_COLS: u16 = 120;
const OUTER_ROWS: u16 = 40;

/// The band as the outer terminal rendered it.
fn band_text(bytes: &[u8]) -> String {
    common::grid_text(bytes, OUTER_COLS, OUTER_ROWS)
}

/// A `cat -v` child in raw mode: what it prints is a caret-notation transcript of
/// exactly the bytes that reached it. It announces `READY` once its own `stty raw`
/// has landed — a `0x1A` written before that is SUSP and stops the child instead of
/// reaching `cat`, so waiting on a fixed sleep is a race rather than a delay.
const CARET_ECHO_CHILD: &str = "/bin/sh -c 'stty raw -echo; printf READY; exec cat -v'";

/// The same child without `-v`, for the cases where caret notation would obscure
/// what is being asserted (`cat -v` renders every non-ASCII byte as `M-…`).
const RAW_ECHO_CHILD: &str = "/bin/sh -c 'stty raw -echo; printf READY; exec cat'";

/// Write `payload` into a fresh `cat -v` session and return the band transcript.
fn transcript(payload: &[u8]) -> String {
    echo_transcript(CARET_ECHO_CHILD, payload)
}

/// Write `payload` once the child has announced itself, and return the band
/// transcript.
fn echo_transcript(child: &str, payload: &[u8]) -> String {
    let mut session = spawn_gutter(OUTER_COLS, OUTER_ROWS, &format!("--width 100 {child}"));
    let (_, seen) = read_until(&mut session, "READY", Duration::from_secs(5));
    assert!(seen.contains("READY"), "the child never came up");

    session.write_all(payload).unwrap();
    session.flush().unwrap();

    let bytes = drain_window(&mut session, Duration::from_millis(700));
    let text = band_text(&bytes);
    drop(session);
    text
}

/// Write every case into one session, separated by a marker so a failure can be
/// read off the transcript, and assert each arrived in caret notation.
fn assert_all_arrive(cases: &[(&str, &[u8])], why: &str) {
    let mut payload: Vec<u8> = Vec::new();
    for (_, bytes) in cases {
        payload.extend_from_slice(bytes);
        payload.push(b'.');
    }
    let text = transcript(&payload);

    for (name, bytes) in cases {
        let caret = caret_notation(bytes);
        assert!(
            text.contains(&caret),
            "{name} ({caret}) {why}; transcript was {text:?}"
        );
    }
}

/// **The keys that go silently dead when this breaks.** Delete, Home, End, PageUp,
/// PageDown, Insert, Shift+Tab and F1–F12 each have to arrive at the child
/// verbatim; a key that reaches it as nothing at all is a key that does literally
/// nothing in a TUI, with no error anywhere to say so.
#[test]
fn keys_that_were_silently_dead_reach_the_child() {
    let _g = pty_guard();
    let cases: &[(&str, &[u8])] = &[
        ("Delete", b"\x1b[3~"),
        ("Home", b"\x1b[H"),
        ("End", b"\x1b[F"),
        ("Home (numbered)", b"\x1b[1~"),
        ("End (numbered)", b"\x1b[4~"),
        ("PageUp", b"\x1b[5~"),
        ("PageDown", b"\x1b[6~"),
        ("Insert", b"\x1b[2~"),
        ("Shift+Tab", b"\x1b[Z"),
        ("F1", b"\x1bOP"),
        ("F5", b"\x1b[15~"),
        ("F12", b"\x1b[24~"),
    ];
    assert_all_arrive(cases, "must reach the child");
}

/// The keys that arrive as *something*, where the risk is the wrong thing: a
/// modifier dropped from a special, the ESC prefix dropped from Alt+&lt;char&gt;, an
/// arrow flattened into CSI form under DECCKM. gutter chooses no form at all, so
/// whatever the terminal sent is what arrives.
#[test]
fn modified_and_alt_keys_arrive_byte_identical() {
    let _g = pty_guard();
    let cases: &[(&str, &[u8])] = &[
        ("Ctrl+Right", b"\x1b[1;5C"),
        ("Shift+Left", b"\x1b[1;2D"),
        ("Alt+r", b"\x1br"),
        ("SS3 Up (DECCKM)", b"\x1bOA"),
        ("CSI Up", b"\x1b[A"),
        ("modifyOtherKeys Shift+Enter", b"\x1b[13;2u"),
        ("xterm-form Shift+Enter", b"\x1b[27;2;13~"),
        ("kitty Ctrl+a", b"\x1b[97;5u"),
    ];
    assert_all_arrive(cases, "must arrive byte-identical");
}

/// The bare control bytes, which have no escape-sequence shape for the scanner to
/// recognise and so must simply fall through. `0x7f` is what Backspace sends on
/// nearly every terminal; `0x1a` is the byte a cooked-mode Ctrl-Z relies on
/// reaching the child's own line discipline (ADR-0018).
#[test]
fn bare_control_bytes_reach_the_child() {
    let _g = pty_guard();
    let text = transcript(b"A\x7fB\x1aC");
    assert!(
        text.contains("A^?B^ZC"),
        "DEL and SUB must arrive verbatim; transcript was {text:?}"
    );
}

/// A lone Escape is the one key the scanner has to withhold, because it cannot be
/// told from the start of a mouse report until either more bytes arrive or the
/// hold expires. It must still arrive, exactly once, and nothing else with it.
#[test]
fn a_lone_escape_reaches_the_child_after_the_hold() {
    let _g = pty_guard();
    let text = transcript(b"\x1b");
    assert!(
        text.contains("^["),
        "a bare Escape must reach the child once the hold resolves it; \
         transcript was {text:?}"
    );
}

/// The reserved chord is the one byte gutter withholds. It must never appear in
/// the child's transcript. Sent twice — enter then exit — so the bytes after it
/// are not swallowed by the mode it opened.
#[test]
fn the_reserved_chord_byte_never_reaches_the_child() {
    let _g = pty_guard();
    // `Ctrl-\` (0x1C) is the default chord; `a` and `b` bracket it so the test
    // fails loudly if the surrounding bytes went missing too.
    let text = transcript(b"a\x1c\x1cb");
    assert!(text.contains("ab"), "the bytes either side must arrive: {text:?}");
    assert!(
        !text.contains("^\\"),
        "the chord byte must be consumed by gutter, not forwarded: {text:?}"
    );
}

/// A `cat -v` child that first asks its terminal to bracket pastes, so gutter
/// mirrors `?2004h` outward and the scanner's paste gate is live (ADR-022).
const PASTE_ECHO_CHILD: &str =
    "/bin/sh -c 'stty raw -echo; printf \"\\033[?2004hREADY\"; exec cat -v'";

/// **A pasted chord byte is text.** Under the guards nothing is interpreted, so the
/// `0x1C` that would otherwise open resize mode mid-paste reaches the child like any
/// other pasted byte.
#[test]
fn a_pasted_chord_byte_reaches_the_child() {
    let _g = pty_guard();
    let text = echo_transcript(PASTE_ECHO_CHILD, b"\x1b[200~abc\x1cdef\x1b[201~");
    assert!(
        text.contains("^[[200~abc^\\def^[[201~"),
        "the whole paste, guards and chord byte included, must arrive verbatim; \
         transcript was {text:?}"
    );
}

/// **And so is a pasted mouse report.** Sharper than the chord case: inside the
/// guards nothing is *extracted* either, so a mouse-report-shaped run arrives
/// verbatim rather than margin-translated, and a malformed one is not swallowed.
#[test]
fn a_pasted_mouse_report_is_not_extracted() {
    let _g = pty_guard();
    let text = echo_transcript(PASTE_ECHO_CHILD, b"\x1b[200~\x1b[<0;10;5M\x1b[<99M\x1b[201~");
    assert!(
        text.contains("^[[200~^[[<0;10;5M^[[<99M^[[201~"),
        "pasted text is data, not input protocol; transcript was {text:?}"
    );
}

/// UTF-8 must never be split across the scanner's buffering.
#[test]
fn multi_byte_utf8_survives_intact() {
    let _g = pty_guard();
    let text = echo_transcript(RAW_ECHO_CHILD, "MARK-é🎉-END".as_bytes());
    assert!(
        text.contains("MARK-é🎉-END"),
        "multi-byte characters must arrive whole; transcript was {text:?}"
    );
}

/// **Ordinary typing reaches the child.** Type ASCII into a `cat` that echoes its
/// stdin; the echoed text must appear in gutter's band.
#[test]
fn ordinary_typing_reaches_child() {
    let _g = pty_guard();
    let mut session = spawn_gutter(OUTER_COLS, OUTER_ROWS, "--width 100 /bin/cat");
    std::thread::sleep(Duration::from_millis(400));

    session.write_all(b"HELLO_KBD").unwrap();
    session.flush().unwrap();

    let bytes = drain_window(&mut session, Duration::from_millis(600));
    let text = band_text(&bytes);
    assert!(
        text.contains("HELLO_KBD"),
        "ordinary typing must reach the child; grid was {text:?}"
    );

    drop(session);
}

/// **Enter reaches the child.** A shell `read` loop completes one read per Enter
/// and prints a per-line marker, so each Enter that arrives becomes a `GOT:`
/// token — unambiguous, unlike `cat`, where a bare `\r` just rewinds the cursor.
/// Two Enters → two markers. The child stays alive (a long sleep after the loop)
/// so the live frame is captured.
#[test]
fn enter_reaches_child() {
    let _g = pty_guard();
    let child = "/bin/sh -c 'i=0; while [ $i -lt 2 ] && read x; do printf \"GOT:%s \" \"$x\"; i=$((i+1)); done; sleep 3'";
    let mut session = spawn_gutter(OUTER_COLS, OUTER_ROWS, &format!("--width 100 {child}"));
    std::thread::sleep(Duration::from_millis(400));

    // Two lines, each terminated by Enter (\r): the read loop fires twice.
    session.write_all(b"ALPHA\r").unwrap();
    session.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    session.write_all(b"BETA\r").unwrap();
    session.flush().unwrap();

    let bytes = drain_window(&mut session, Duration::from_millis(700));
    let text = band_text(&bytes);
    assert!(
        text.contains("GOT:ALPHA") && text.contains("GOT:BETA"),
        "each Enter must complete a child read (two markers); grid was {text:?}"
    );

    drop(session);
}

/// **Ctrl-C reaches the child as the raw `0x03`.** Wrap a shell that reports when
/// it receives SIGINT (which the tty driver raises from the `0x03` byte), then
/// send Ctrl-C. Seeing the trap's marker proves the control byte transited the
/// scanner untouched.
///
/// The trap prints its marker then keeps the child ALIVE (a long sleep), so the
/// marker stays on the live frame the drain window captures.
#[test]
fn ctrl_c_delivers_interrupt_to_child() {
    let _g = pty_guard();
    let child = "/bin/sh -c 'trap \"printf GOTSIGINT; sleep 3\" INT; while true; do sleep 0.1; done'";
    let mut session = spawn_gutter(OUTER_COLS, OUTER_ROWS, &format!("--width 100 {child}"));
    std::thread::sleep(Duration::from_millis(500));

    session.write_all(&[0x03]).unwrap();
    session.flush().unwrap();

    let bytes = drain_window(&mut session, Duration::from_millis(900));
    let text = band_text(&bytes);
    assert!(
        text.contains("GOTSIGINT"),
        "Ctrl-C must reach the child as 0x03 and raise SIGINT; grid was {text:?}"
    );

    drop(session);
}

/// Render bytes the way `cat -v` does, so a test can search the band for them.
fn caret_notation(bytes: &[u8]) -> String {
    let mut out = String::new();
    for &b in bytes {
        match b {
            0x00..=0x1f => {
                out.push('^');
                out.push((b + 0x40) as char);
            }
            0x7f => out.push_str("^?"),
            _ => out.push(b as char),
        }
    }
    out
}
