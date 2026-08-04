//! OSC-52 clipboard write: reconstruct the wire sequence and send it through a
//! separate open of the terminal gutter resolved. See ADR-004.
//!
//! gutter does NOT scan bytes for OSC 52. vt100 owns the full OSC
//! termination/abort rule set and hands us the already-reassembled `ty` and
//! base64 `data` via [`vt100::Callbacks::copy_to_clipboard`]; this module is
//! only what happens after that callback fires.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

const ESC: u8 = 0x1b;
const OSC_INTRODUCER: u8 = b']';
const SEP: u8 = b';';
/// The terminator gutter emits. The child may have ended its OSC with BEL or
/// ST (`ESC \`); vt100 hands us just the payload either way, so we re-terminate
/// with BEL.
const BEL: u8 = 0x07;

/// Opens the terminal read-write for the clipboard sink. See ADR-004.
///
/// `tty` is the device the render sink was opened from ([`crate::terminal::open_tty_write`]),
/// so gutter keeps one answer to which terminal it is talking to. A distinct open from
/// the render sink's, so a clipboard write and a frame repaint never share fd state.
/// Opened read-write rather than write-only to leave room for a later read-response
/// relay; the clipboard's own read half is unused, but
/// [`crate::anchor::open_input_tty`] opens the input tty through here and reads it.
///
/// `O_NOCTTY` for the same reason the sink's open carries it: naming the terminal must
/// not make it gutter's controlling terminal.
pub fn open_tty_read_write(tty: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(tty)
}

/// Builds the wire bytes `ESC ] 52 ; ty ; data BEL` from the callback's slices.
///
/// `data` is copied verbatim: it arrives base64-encoded and is emitted
/// byte-for-byte with no decode/re-encode. See ADR-004.
pub fn reconstruct_osc52(ty: &[u8], data: &[u8]) -> Vec<u8> {
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

/// Writes the reconstructed OSC 52 to `out` and flushes.
///
/// `out` is `&mut impl Write` so production injects the real terminal handle and
/// tests inject a buffer. Returns the `io::Result` so the caller can log it: a
/// failed clipboard write must not unwind out of `parser.process()` and desync
/// the parser.
pub fn forward_osc52(out: &mut impl Write, ty: &[u8], data: &[u8]) -> io::Result<()> {
    out.write_all(&reconstruct_osc52(ty, data))?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reconstruction is byte-exact and copies `data` verbatim: the `=` padding
    /// and the `+`/`/` base64 chars must survive unchanged.
    #[test]
    fn reconstruct_is_byte_exact_and_verbatim() {
        let data = b"aGVsbG8=";
        let got = reconstruct_osc52(b"c", data);
        assert_eq!(got, b"\x1b]52;c;aGVsbG8=\x07");

        // Full base64 alphabet, including `+`, `/`, `=`.
        let tricky = b"a+b/cD==";
        let got = reconstruct_osc52(b"p", tricky);
        assert_eq!(got, b"\x1b]52;p;a+b/cD==\x07");
        // The data segment (between the 2nd `;` and the BEL) is unchanged.
        assert_eq!(&got[got.len() - 1 - tricky.len()..got.len() - 1], tricky);
    }

    /// Empty `data` (clear-clipboard) still produces a well-formed sequence.
    #[test]
    fn reconstruct_handles_empty_data() {
        assert_eq!(reconstruct_osc52(b"c", b""), b"\x1b]52;c;\x07");
    }

    /// `forward_osc52` writes exactly the reconstructed bytes and flushes.
    #[test]
    fn forward_writes_reconstructed_bytes() {
        let mut buf: Vec<u8> = Vec::new();
        forward_osc52(&mut buf, b"c", b"aGVsbG8=").unwrap();
        assert_eq!(buf, b"\x1b]52;c;aGVsbG8=\x07");
    }

    /// `/dev/tty` is opened read AND write (ADR-004), so a later read-response
    /// relay isn't foreclosed. The access mode isn't portably queryable, so
    /// prove write works and that a zero-length read doesn't hit the EBADF a
    /// write-only fd would give. Skips when no controlling tty is present.
    #[test]
    fn tty_opens_read_write() {
        use std::io::Read;
        let mut file = match open_tty_read_write(Path::new("/dev/tty")) {
            Ok(f) => f,
            Err(_) => return, // no controlling terminal here.
        };
        assert!(file.write_all(b"").is_ok());
        // A zero-length read touches the fd's read capability without blocking
        // on terminal input; a write-only fd would error EBADF here.
        let mut empty: [u8; 0] = [];
        assert!(
            file.read(&mut empty).is_ok(),
            "the handle must be readable (opened read-write, not write-only)"
        );
    }
}
