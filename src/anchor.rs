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
///
/// `select`, not `poll`: on macOS the `/dev/tty` cloning device answers `poll()`
/// with `POLLNVAL` straight away instead of waiting, so the probe would give up
/// before the terminal had a chance to reply. `select()` waits correctly on both
/// macOS and Linux.
fn wait_readable(tty: &File, timeout: Duration) -> bool {
    let fd = tty.as_raw_fd();
    if fd < 0 || fd >= libc::FD_SETSIZE as i32 {
        return false;
    }
    // SAFETY: a zeroed `fd_set` is a valid empty set, `fd` is owned by `tty` and
    // checked to be in range, and `select` is given the matching nfds.
    unsafe {
        let mut set: libc::fd_set = std::mem::zeroed();
        libc::FD_SET(fd, &mut set);
        let mut tv = libc::timeval {
            tv_sec: timeout.as_secs().min(i32::MAX as u64) as libc::time_t,
            tv_usec: timeout.subsec_micros() as libc::suseconds_t,
        };
        let n = libc::select(
            fd + 1,
            &mut set,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut tv,
        );
        n > 0 && libc::FD_ISSET(fd, &set)
    }
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
    use super::{find_cpr, open_input_tty, wait_readable};
    use std::time::{Duration, Instant};

    /// The macOS `/dev/tty` guard: `poll()` there returns `POLLNVAL` at once, so a
    /// probe built on it gives up before the terminal can answer.
    #[test]
    fn waiting_on_an_idle_tty_uses_the_whole_timeout() {
        let Some(tty) = open_input_tty() else {
            return; // No tty at all (CI): nothing to wait on.
        };
        if wait_readable(&tty, Duration::from_millis(0)) {
            return; // Something is already pending; the wait would prove nothing.
        }
        let start = Instant::now();
        assert!(!wait_readable(&tty, Duration::from_millis(100)));
        assert!(
            start.elapsed() >= Duration::from_millis(90),
            "returned after {:?}, so it never waited",
            start.elapsed()
        );
    }

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
