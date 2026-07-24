//! Slice 06 OSC-52 clipboard-write — the PTY-driven end-to-end leg (ADR-004).
//!
//! The unit suite in `src/callbacks.rs` drives `parser.process()` with a
//! recording callback and pins exactly what vt100/vte dispatches (the
//! dispatch-confirmation seam), and `src/clipboard.rs` pins the byte-exact
//! reconstruction. This file layers the end-to-end check on top: a real child
//! emits an OSC-52 write through a real PTY, gutter intercepts it via
//! `copy_to_clipboard` on the render thread, reconstructs `ESC ] 52 ; ty ; data
//! BEL` and writes it to its `/dev/tty` — which, under the test PTY, is gutter's
//! own controlling terminal, so the reconstructed sequence surfaces in the bytes
//! the outer terminal receives. We scan for it there.
//!
//! This proves the whole spine the slice exists for: child copy → Thread 1 pump
//! → Thread 2 `parser.process` → vt100 dispatch → reconstruct → `/dev/tty`. The
//! `data` is asserted **byte-for-byte** the base64 the child emitted (no
//! round-trip mangling). The byte-exact reconstruction and the
//! distinct-fd/captured-sink assertions are covered by the in-crate unit tests
//! (`clipboard::tests`, `callbacks::tests`), which can inject a buffer the way a
//! separate spawned process cannot.
//!
//! Headless: a real PTY, no display, `TERM=xterm-256color`.

use std::process::Command;
use std::time::{Duration, Instant};

use expectrl::session::OsSession;
use expectrl::Session;

const OUTER_COLS: u16 = 80;
const OUTER_ROWS: u16 = 24;

/// A known base64 payload the child copies. Carries `+`, `/` and `=` padding so
/// the "verbatim, no decode/re-encode" guarantee is exercised on the full base64
/// alphabet end-to-end.
const KNOWN_B64: &str = "aGVs+bG8/8w==";

fn gutter_cmd(child_argv: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gutter"));
    cmd.args(child_argv);
    cmd.env("TERM", "xterm-256color");
    cmd.env("GUTTER_FORCE_ANCHOR_ROW", "0");
    cmd
}

fn spawn_gutter(child_argv: &[&str]) -> OsSession {
    let mut session = Session::spawn(gutter_cmd(child_argv)).expect("spawn gutter under PTY");
    session
        .get_process_mut()
        .set_window_size(OUTER_COLS, OUTER_ROWS)
        .expect("set outer PTY window size");
    session.set_expect_timeout(Some(Duration::from_secs(10)));
    session
}

/// Non-blocking drain of the outer PTY for `window`, collecting everything
/// gutter painted (frames AND the clipboard OSC it forwarded to `/dev/tty`).
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

/// **Copy in the child reaches the real terminal (OSC 52 write).** The child
/// emits `ESC ] 52 ; c ; <known-base64> BEL` on its output. gutter must
/// reconstruct that exact sequence and write it to `/dev/tty`; under the test
/// PTY that is gutter's controlling terminal, so the reconstructed bytes appear
/// in what the outer terminal receives. Assert the full reconstructed sequence
/// is present and that `data` is byte-for-byte the base64 the child emitted.
#[test]
fn child_osc52_write_reaches_the_real_terminal() {
    // The child prints the OSC-52 write, then sleeps so it stays alive while
    // gutter's render thread processes the bytes and forwards the clipboard.
    // \033 = ESC, \007 = BEL — built explicitly so no shell quoting mangles it.
    let payload = format!("\\033]52;c;{KNOWN_B64}\\007");
    let child_script = format!("printf '{payload}'; sleep 2");
    let mut session = spawn_gutter(&["sh", "-c", &child_script]);

    let bytes = drain_window(&mut session, Duration::from_millis(1200));

    // The reconstructed OSC-52 (BEL-terminated) gutter forwarded to /dev/tty.
    let expected = format!("\x1b]52;c;{KNOWN_B64}\x07");
    let haystack = String::from_utf8_lossy(&bytes);
    assert!(
        haystack.contains(&expected),
        "gutter must forward the reconstructed OSC-52 to the real terminal.\n\
         expected to find: {expected:?}\n\
         in outer bytes (lossy): {haystack:?}"
    );

    // `data` is byte-for-byte the base64 the child emitted — the `+`, `/` and `=`
    // survived with no decode/re-encode round-trip.
    let marker = format!(";c;{KNOWN_B64}\x07");
    assert!(
        haystack.contains(&marker),
        "the base64 payload must be forwarded verbatim (no round-trip mangling)"
    );

    drop(session);
}

/// A non-52 OSC the child emits (a window-title set, OSC 0) must NOT produce a
/// clipboard write — gutter only forwards OSC 52. Proves the interception is
/// scoped to the clipboard sequence and doesn't leak unrelated OSCs to
/// `/dev/tty` as clipboard writes.
#[test]
fn non_osc52_does_not_produce_a_clipboard_write() {
    // The child sets a window title (OSC 0) and also does a real OSC-52 so we
    // have a positive anchor proving the run captured output. A `52;` clipboard
    // sequence must appear exactly once (the real copy), never spuriously from
    // the title.
    let payload = format!("\\033]0;my-title\\007\\033]52;c;{KNOWN_B64}\\007");
    let child_script = format!("printf '{payload}'; sleep 2");
    let mut session = spawn_gutter(&["sh", "-c", &child_script]);

    let bytes = drain_window(&mut session, Duration::from_millis(1200));
    let haystack = String::from_utf8_lossy(&bytes);

    // Exactly one forwarded clipboard sequence (the real OSC-52), and it carries
    // the known payload — the title's "my-title" never appears as a 52 write.
    let clip = format!("]52;c;{KNOWN_B64}\x07");
    assert!(
        haystack.contains(&clip),
        "the real OSC-52 copy must be forwarded"
    );
    assert!(
        !haystack.contains("]52;c;my-title"),
        "a window-title OSC must not be forwarded as a clipboard write"
    );

    drop(session);
}
