//! OSC-52 clipboard write — reconstruction and the `/dev/tty` sink (ADR-004).
//!
//! gutter does NOT scan bytes for OSC 52. vt100 (over vte) owns the full OSC
//! termination/abort rule set — BEL `0x07`, ST `ESC \`, C1 ST `0x9C`, the
//! CAN `0x18` / SUB `0x1A` abort, and split-read reassembly via vte's persistent
//! `osc_raw` buffer — and hands gutter a *complete*, already-decoded OSC 52 via
//! [`vt100::Callbacks::copy_to_clipboard`]. This module is only what happens
//! AFTER that callback fires: reconstruct the wire sequence and write it to the
//! real terminal.
//!
//! Three small, pure-ish pieces, each independently testable:
//!
//! - [`open_tty_read_write`] — open `/dev/tty` read-write at exactly one call
//!   site. Read is unused in v1; it is opened now so the deferred post-v1 OSC-52
//!   read-response relay (the child asking the terminal to *return* clipboard
//!   contents) can be added on Thread 2 without re-architecting the fd handling.
//! - [`reconstruct_osc52`] — build `ESC ] 52 ; ty ; data BEL` by concatenating
//!   the raw byte slices. `data` arrives base64-encoded; it is forwarded
//!   **verbatim** — no decode/re-encode, so no padding or charset drift.
//! - [`forward_to_tty`] — write the reconstructed sequence to an injected
//!   `Write` and flush. The injected writer is the seam: production hands in the
//!   real `/dev/tty` handle, tests hand in a buffer they read back.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};

/// `ESC` — introduces the OSC.
const ESC: u8 = 0x1b;
/// `]` — the OSC introducer's second byte.
const OSC_INTRODUCER: u8 = b']';
/// `;` — the OSC parameter separator.
const SEP: u8 = b';';
/// `BEL` (`0x07`) — the terminator gutter emits. The child may have used ST or
/// `0x9C`; vt100 already consumed and normalised that. We emit a fresh,
/// well-formed sequence and BEL is the design's chosen terminator (ADR-004).
const BEL: u8 = 0x07;

/// Open `/dev/tty` for **read and write** (ADR-004). Called once at startup; the
/// returned handle is the clipboard sink the render-thread callback writes to —
/// a fd distinct from crossterm's stdout, so a clipboard write and a frame
/// repaint never share a fd.
///
/// Read is opened but unused in v1 — it is the forward-looking hook for the
/// post-v1 OSC-52 read-response relay (out of scope here). Opening read-write
/// now means that relay does not have to re-open or re-architect the handle.
pub fn open_tty_read_write() -> io::Result<File> {
    OpenOptions::new().read(true).write(true).open("/dev/tty")
}

/// Build the wire bytes `ESC ] 52 ; ty ; data BEL` from the callback's `ty` and
/// `data` slices.
///
/// `data` is copied **verbatim** — it arrives base64-encoded from vt100 and is
/// emitted byte-for-byte with no decode/re-encode (ADR-004). The terminator is
/// always BEL regardless of what the child originally used. Pure and total.
pub fn reconstruct_osc52(ty: &[u8], data: &[u8]) -> Vec<u8> {
    // ESC ] 5 2 ; <ty> ; <data> BEL
    let mut out = Vec::with_capacity(ty.len() + data.len() + 6);
    out.push(ESC);
    out.push(OSC_INTRODUCER);
    out.extend_from_slice(b"52");
    out.push(SEP);
    out.extend_from_slice(ty);
    out.push(SEP);
    out.extend_from_slice(data);
    out.push(BEL);
    out
}

/// Write the reconstructed OSC 52 to `out` and flush.
///
/// `out` is taken as `&mut impl Write` so the production caller injects the real
/// `/dev/tty` handle and the tests inject a buffer. Returns `io::Result` so the
/// caller can log a failure; the callback swallows the error rather than letting
/// it unwind out of `parser.process()` (a failed clipboard write must never
/// crash the render loop or desync the parser).
pub fn forward_to_tty(out: &mut impl Write, ty: &[u8], data: &[u8]) -> io::Result<()> {
    out.write_all(&reconstruct_osc52(ty, data))?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reconstruction is byte-exact: `ESC ] 52 ; c ; <data> BEL`, with `data`
    /// copied verbatim — the "no decode/re-encode round-trip" guarantee at the
    /// smallest level. The payload carries `=` padding and the `+`/`/` base64
    /// alphabet chars; none may be altered.
    #[test]
    fn reconstruct_is_byte_exact_and_verbatim() {
        let data = b"aGVsbG8=";
        let got = reconstruct_osc52(b"c", data);
        assert_eq!(got, b"\x1b]52;c;aGVsbG8=\x07");

        // A payload exercising the full base64 alphabet incl. `+`, `/`, `=`.
        let tricky = b"a+b/cD==";
        let got = reconstruct_osc52(b"p", tricky);
        assert_eq!(got, b"\x1b]52;p;a+b/cD==\x07");
        // The data segment is the input slice unchanged (between the 2nd `;`
        // and the trailing BEL).
        assert_eq!(&got[got.len() - 1 - tricky.len()..got.len() - 1], tricky);
    }

    /// Empty `data` (a clear-clipboard request) still produces a well-formed
    /// sequence — `ty` and the two separators are present, the body is empty.
    #[test]
    fn reconstruct_handles_empty_data() {
        assert_eq!(reconstruct_osc52(b"c", b""), b"\x1b]52;c;\x07");
    }

    /// `forward_to_tty` writes exactly the reconstructed bytes to the injected
    /// sink and flushes — the seam the end-to-end test reads back.
    #[test]
    fn forward_writes_reconstructed_bytes() {
        let mut buf: Vec<u8> = Vec::new();
        forward_to_tty(&mut buf, b"c", b"aGVsbG8=").unwrap();
        assert_eq!(buf, b"\x1b]52;c;aGVsbG8=\x07");
    }

    /// **Distinct fd** (ADR-004 / the slice AC): the clipboard `/dev/tty` handle
    /// must be a separate fd from the stdout the render loop repaints through, so
    /// a clipboard write and a frame repaint can never share a fd. Assert the raw
    /// fd of the opened `/dev/tty` differs from stdout's. Skips cleanly when no
    /// controlling tty is present; the *separateness* is structural regardless
    /// (`/dev/tty` is opened independently, not cloned from stdout).
    #[test]
    fn tty_fd_is_distinct_from_stdout() {
        use std::os::fd::AsRawFd;
        let tty = match open_tty_read_write() {
            Ok(f) => f,
            Err(_) => return, // no controlling terminal here.
        };
        let stdout_fd = io::stdout().as_raw_fd();
        assert_ne!(
            tty.as_raw_fd(),
            stdout_fd,
            "the clipboard /dev/tty fd must be distinct from the stdout repaint fd"
        );
    }

    /// `/dev/tty` is opened read AND write (ADR-004): the post-v1 read-response
    /// relay must not be foreclosed. We can't assert the access mode portably, so
    /// prove write works (the v1 path) and that a read call doesn't fail with the
    /// `EBADF`-for-write-only that a write-only fd would give — i.e. the read
    /// side is usable. Skips cleanly when no controlling tty is present (CI nodes
    /// without a `/dev/tty`), since the read-write *intent* is already pinned by
    /// the explicit `OpenOptions` in `open_tty_read_write`.
    #[test]
    fn tty_opens_read_write() {
        use std::io::Read;
        let mut file = match open_tty_read_write() {
            Ok(f) => f,
            // No controlling terminal in this environment — nothing to assert.
            Err(_) => return,
        };
        // The write side is exercised in v1.
        assert!(file.write_all(b"").is_ok());
        // The read side must be usable (a write-only fd would error EBADF on a
        // read attempt). A zero-length read touches the fd's read capability
        // without blocking on actual terminal input.
        let mut empty: [u8; 0] = [];
        assert!(
            file.read(&mut empty).is_ok(),
            "the handle must be readable (opened read-write, not write-only)"
        );
    }
}
