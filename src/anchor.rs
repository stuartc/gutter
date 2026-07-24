//! The startup cursor-position query behind the inline anchor (ADR-013), and the
//! tty gutter reads input from.
//!
//! Hand-rolled rather than `crossterm::cursor::position()`: that runs through
//! crossterm's event machinery, which pushes every non-reply event it meets into an
//! internal queue that nothing drains once crossterm is out of the input path
//! (ADR-020). The bytes a user typed while gutter was starting have to survive, so
//! the probe hands them back instead.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::time::{Duration, Instant};

use crate::clipboard;

/// How long the probe waits for the terminal's reply. Generous: a real terminal
/// answers in a millisecond or two.
pub const CPR_TIMEOUT: Duration = Duration::from_millis(100);

/// The tty gutter reads input from: `/dev/tty` (a separate open from the
/// clipboard's, ADR-004), falling back to a `dup` of stdin.
pub fn open_input_tty() -> Option<File> {
    if let Ok(tty) = clipboard::open_tty_read_write() {
        return Some(tty);
    }
    // SAFETY: `dup` returns a fresh descriptor this process owns outright, so
    // handing it to `File` transfers a genuinely exclusive ownership.
    let fd = unsafe { libc::dup(0) };
    (fd >= 0).then(|| unsafe { File::from_raw_fd(fd) })
}

/// Ask the terminal where the cursor is (DSR-CPR, `ESC [ 6 n`) and read the
/// `ESC [ row ; col R` answer back off the input tty. Returns the 0-based row and
/// **everything else that was read** — bytes the user typed while gutter was
/// starting, which the caller must not drop.
pub fn probe_cursor_row(tty: &File, timeout: Duration) -> (Option<u16>, Vec<u8>) {
    let mut out = std::io::stdout();
    if out.write_all(b"\x1b[6n").is_err() || out.flush().is_err() {
        return (None, Vec::new());
    }

    let deadline = Instant::now() + timeout;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        if let Some((start, end, row)) = find_cpr(&buf) {
            let mut leftover = buf[..start].to_vec();
            leftover.extend_from_slice(&buf[end..]);
            return (Some(row), leftover);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || !wait_readable(tty, remaining) {
            break;
        }
        let mut chunk = [0u8; 256];
        match (&mut &*tty).read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    // No reply inside the window: the whole buffer is the user's.
    (None, buf)
}

/// Whether `tty` has readable bytes within `timeout`.
fn wait_readable(tty: &File, timeout: Duration) -> bool {
    let mut pfd = libc::pollfd {
        fd: tty.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: one initialised pollfd, described honestly by the count.
    let n = unsafe { libc::poll(&mut pfd, 1, ms) };
    n > 0 && pfd.revents & libc::POLLIN != 0
}

/// Locate a `ESC [ <row> ; <col> R` reply: its byte span and the 0-based row.
fn find_cpr(buf: &[u8]) -> Option<(usize, usize, u16)> {
    for start in 0..buf.len().saturating_sub(1) {
        if buf[start] != 0x1b || buf[start + 1] != b'[' {
            continue;
        }
        let digits_start = start + 2;
        let mut i = digits_start;
        while i < buf.len() && buf[i].is_ascii_digit() {
            i += 1;
        }
        if i == digits_start || i >= buf.len() || buf[i] != b';' {
            continue;
        }
        let row_end = i;
        i += 1;
        let col_start = i;
        while i < buf.len() && buf[i].is_ascii_digit() {
            i += 1;
        }
        if i == col_start || i >= buf.len() || buf[i] != b'R' {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&buf[digits_start..row_end]) else {
            continue;
        };
        let Ok(row) = text.parse::<u32>() else {
            continue;
        };
        // The reply is 1-based; the grid is 0-based.
        let row0 = row.saturating_sub(1).min(u16::MAX as u32) as u16;
        return Some((start, i + 1, row0));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::find_cpr;

    #[test]
    fn cpr_reply_is_located_and_the_row_is_zero_based() {
        assert_eq!(find_cpr(b"\x1b[24;1R"), Some((0, 7, 23)));
        assert_eq!(find_cpr(b"\x1b[1;1R"), Some((0, 6, 0)));
    }

    #[test]
    fn keystrokes_around_the_reply_are_leftover() {
        let buf = b"ab\x1b[7;3Rcd";
        let (start, end, row) = find_cpr(buf).unwrap();
        assert_eq!(row, 6);
        let mut leftover = buf[..start].to_vec();
        leftover.extend_from_slice(&buf[end..]);
        assert_eq!(leftover, b"abcd".to_vec());
    }

    #[test]
    fn a_partial_or_absent_reply_is_not_matched() {
        assert_eq!(find_cpr(b""), None);
        assert_eq!(find_cpr(b"\x1b[24;1"), None, "no final byte yet");
        assert_eq!(find_cpr(b"\x1b[24R"), None, "no column parameter");
        assert_eq!(find_cpr(b"hello"), None);
        // A different CSI must not be mistaken for the reply.
        assert_eq!(find_cpr(b"\x1b[15~"), None);
    }
}
