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
use std::time::{Duration, Instant};

use expectrl::session::OsSession;

fn gutter_bin() -> String {
    env!("CARGO_BIN_EXE_gutter").to_string()
}

/// Run gutter inside an outer terminal of a fixed size:
/// `sh -c 'stty cols C rows R; exec env gutter <args>'`.
fn gutter_in_terminal(
    outer_cols: u16,
    outer_rows: u16,
    gutter_args: &str,
) -> std::process::Command {
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

/// Parse outer-terminal bytes through a vt100 at the physical size.
fn outer_grid(bytes: &[u8], cols: u16, rows: u16) -> vt100::Parser {
    let mut p = vt100::Parser::new(rows, cols, 0);
    p.process(bytes);
    p
}

/// The whole grid joined to a single string, so a test can search for echoed
/// content regardless of which row it landed on.
fn grid_text(screen: &vt100::Screen, cols: u16, rows: u16) -> String {
    (0..rows)
        .map(|r| screen.rows(0, cols).nth(r as usize).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A `cat -v` child in raw mode: what it prints is a caret-notation transcript of
/// exactly the bytes that reached it.
const CARET_ECHO_CHILD: &str = "/bin/sh -c 'stty raw -echo; exec cat -v'";

/// The same child without `-v`, for the cases where caret notation would obscure
/// what is being asserted (`cat -v` renders every non-ASCII byte as `M-…`).
const RAW_ECHO_CHILD: &str = "/bin/sh -c 'stty raw -echo; exec cat'";

/// Write `payload` into a fresh `cat -v` session and return the band transcript.
fn transcript(payload: &[u8]) -> String {
    echo_transcript(CARET_ECHO_CHILD, payload)
}

fn echo_transcript(child: &str, payload: &[u8]) -> String {
    let cmd = gutter_in_terminal(120, 40, &format!("--width 100 {child}"));
    let mut session = spawn(cmd);
    std::thread::sleep(Duration::from_millis(400));

    session.write_all(payload).unwrap();
    session.flush().unwrap();

    let bytes = drain_window(&mut session, Duration::from_millis(700));
    let parser = outer_grid(&bytes, 120, 40);
    let text = grid_text(parser.screen(), 120, 40);
    drop(session);
    text
}

/// **The keys that were silently dead.** Delete, Home, End, PageUp, PageDown,
/// Insert, Shift+Tab and F1–F12 produced no bytes at all under the old
/// re-encoder: pressing F5 in a TUI under gutter did literally nothing. Every one
/// of them must now arrive at the child verbatim.
///
/// Named after the bug rather than the mechanism, so a future regression says
/// what it broke.
#[test]
fn keys_that_were_silently_dead_reach_the_child() {
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
    // One session for the whole battery, separated by a marker so a failure can be
    // read off the transcript.
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
            "{name} ({caret}) must reach the child; transcript was {text:?}"
        );
    }
}

/// The faults the old encoder had on keys that *did* produce something: modifiers
/// dropped on specials, the ESC prefix dropped on Alt+&lt;char&gt;, and arrows
/// always emitted in CSI form even under DECCKM. Under passthrough gutter never
/// chooses a form, so whatever the terminal sent is what arrives.
#[test]
fn modified_and_alt_keys_arrive_byte_identical() {
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
            "{name} ({caret}) must arrive byte-identical; transcript was {text:?}"
        );
    }
}

/// A lone Escape is the one key the scanner has to withhold, because it cannot be
/// told from the start of a mouse report until either more bytes arrive or the
/// hold expires. It must still arrive, exactly once, and nothing else with it.
#[test]
fn a_lone_escape_reaches_the_child_after_the_hold() {
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
    // `Ctrl-\` (0x1C) is the default chord; `a` and `b` bracket it so the test
    // fails loudly if the surrounding bytes went missing too.
    let text = transcript(b"a\x1c\x1cb");
    assert!(text.contains("ab"), "the bytes either side must arrive: {text:?}");
    assert!(
        !text.contains("^\\"),
        "the chord byte must be consumed by gutter, not forwarded: {text:?}"
    );
}

/// UTF-8 must never be split across the scanner's buffering.
#[test]
fn multi_byte_utf8_survives_intact() {
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
    let cmd = gutter_in_terminal(120, 40, "--width 100 /bin/cat");
    let mut session = spawn(cmd);
    std::thread::sleep(Duration::from_millis(400));

    session.write_all(b"HELLO_KBD").unwrap();
    session.flush().unwrap();

    let bytes = drain_window(&mut session, Duration::from_millis(600));
    let parser = outer_grid(&bytes, 120, 40);
    let text = grid_text(parser.screen(), 120, 40);
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
    let child = "/bin/sh -c 'i=0; while [ $i -lt 2 ] && read x; do printf \"GOT:%s \" \"$x\"; i=$((i+1)); done; sleep 3'";
    let cmd = gutter_in_terminal(120, 40, &format!("--width 100 {child}"));
    let mut session = spawn(cmd);
    std::thread::sleep(Duration::from_millis(400));

    // Two lines, each terminated by Enter (\r): the read loop fires twice.
    session.write_all(b"ALPHA\r").unwrap();
    session.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    session.write_all(b"BETA\r").unwrap();
    session.flush().unwrap();

    let bytes = drain_window(&mut session, Duration::from_millis(700));
    let parser = outer_grid(&bytes, 120, 40);
    let text = grid_text(parser.screen(), 120, 40);
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
    let child = "/bin/sh -c 'trap \"printf GOTSIGINT; sleep 3\" INT; while true; do sleep 0.1; done'";
    let cmd = gutter_in_terminal(120, 40, &format!("--width 100 {child}"));
    let mut session = spawn(cmd);
    std::thread::sleep(Duration::from_millis(500));

    session.write_all(&[0x03]).unwrap();
    session.flush().unwrap();

    let bytes = drain_window(&mut session, Duration::from_millis(900));
    let parser = outer_grid(&bytes, 120, 40);
    let text = grid_text(parser.screen(), 120, 40);
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
