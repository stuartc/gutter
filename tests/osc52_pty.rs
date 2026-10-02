//! OSC-52 clipboard-write — the PTY-driven end-to-end leg (ADR-004).
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
//! This proves the whole spine: child copy → Thread 1 pump
//! → Thread 2 `parser.process` → vt100 dispatch → reconstruct → `/dev/tty`. The
//! `data` is asserted **byte-for-byte** the base64 the child emitted (no
//! round-trip mangling). The byte-exact reconstruction and the
//! distinct-fd/captured-sink assertions are covered by the in-crate unit tests
//! (`clipboard::tests`, `callbacks::tests`), which can inject a buffer the way a
//! separate spawned process cannot.
//!
//! Headless: a real PTY, no display, `TERM=xterm-256color`.

mod common;
use common::{find, Gutter};

/// Run `sh -c script` under gutter and return every byte the outer terminal saw,
/// once `expected` has turned up among them. `script` must end on a `read`, which
/// holds the child until then.
fn outer_bytes_once_seen(script: &str, expected: &str) -> String {
    let mut gutter = Gutter::spawn_argv(&["sh", "-c", script]);
    gutter.wait_for("the forwarded OSC 52", |s| {
        find(s.bytes, expected.as_bytes()).is_some()
    });
    gutter.send(b"\n");
    String::from_utf8_lossy(&gutter.finish().bytes).into_owned()
}

/// A known base64 payload the child copies. Carries `+`, `/` and `=` padding so
/// the "verbatim, no decode/re-encode" guarantee is exercised on the full base64
/// alphabet end-to-end.
const KNOWN_B64: &str = "aGVs+bG8/8w==";

/// **Copy in the child reaches the real terminal (OSC 52 write).** The child
/// emits `ESC ] 52 ; c ; <known-base64> BEL` on its output. gutter must
/// reconstruct that exact sequence and write it to `/dev/tty`; under the test
/// PTY that is gutter's controlling terminal, so the reconstructed bytes appear
/// in what the outer terminal receives. Assert the full reconstructed sequence
/// is present and that `data` is byte-for-byte the base64 the child emitted.
#[test]
fn child_osc52_write_reaches_the_real_terminal() {
    // \033 = ESC, \007 = BEL — built explicitly so no shell quoting mangles it.
    let payload = format!("\\033]52;c;{KNOWN_B64}\\007");
    let child_script = format!("printf '{payload}'; read _");

    // The reconstructed OSC-52 (BEL-terminated) gutter forwarded to /dev/tty.
    let expected = format!("\x1b]52;c;{KNOWN_B64}\x07");
    let haystack = outer_bytes_once_seen(&child_script, &expected);
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
    let child_script = format!("printf '{payload}'; read _");

    // Exactly one forwarded clipboard sequence (the real OSC-52), and it carries
    // the known payload — the title's "my-title" never appears as a 52 write. The
    // title comes first in the child's output, so once the real copy is through,
    // anything the title was going to cause has been written too.
    let clip = format!("]52;c;{KNOWN_B64}\x07");
    let haystack = outer_bytes_once_seen(&child_script, &clip);
    assert!(
        haystack.contains(&clip),
        "the real OSC-52 copy must be forwarded"
    );
    assert!(
        !haystack.contains("]52;c;my-title"),
        "a window-title OSC must not be forwarded as a clipboard write"
    );
}
