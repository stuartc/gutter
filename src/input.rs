//! Thread 3 — the input reader. A dumb `read()` pump on the outer tty, the same
//! shape Thread 1 has for the PTY: it forwards raw chunks and interprets
//! nothing. The scanner that makes sense of those bytes lives on the render
//! thread (ADR-020). Spawned detached because `read()` can't be interrupted, and
//! reaped by `process::exit`.

use std::io::{ErrorKind, Read};
use std::sync::mpsc::Sender;

use crate::msg::Msg;

/// Buffer size for one `read()`. Comfortably larger than any single key
/// sequence or paste burst a terminal delivers in one write.
const READ_BUF: usize = 4096;

/// Pumps the outer tty until EOF, a read error, or the merged channel closing.
///
/// `Interrupted` is retried rather than treated as EOF: gutter installs a
/// `SIGWINCH` handler, so every terminal resize can interrupt this read. Getting
/// that wrong is unmistakable — the keyboard works until you resize the window,
/// then stops completely.
pub fn run<R: Read>(mut tty: R, merged: Sender<Msg>) {
    let mut buf = [0u8; READ_BUF];
    loop {
        match tty.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if merged.send(Msg::Input(buf[..n].to_vec())).is_err() {
                    break;
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    /// A reader that yields `Interrupted` before its payload, modelling the
    /// `EINTR` a SIGWINCH delivers into the blocking read.
    struct InterruptThenRead {
        interrupted: bool,
        payload: Vec<u8>,
    }

    impl Read for InterruptThenRead {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(std::io::Error::from(ErrorKind::Interrupted));
            }
            let n = self.payload.len().min(buf.len());
            buf[..n].copy_from_slice(&self.payload[..n]);
            self.payload.drain(..n);
            Ok(n)
        }
    }

    #[test]
    fn eintr_is_retried_not_treated_as_eof() {
        let (tx, rx) = channel();
        run(
            InterruptThenRead {
                interrupted: false,
                payload: b"abc".to_vec(),
            },
            tx,
        );
        let got: Vec<Vec<u8>> = rx
            .iter()
            .map(|m| match m {
                Msg::Input(b) => b,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(got, vec![b"abc".to_vec()]);
    }

    #[test]
    fn chunks_are_forwarded_verbatim() {
        let (tx, rx) = channel();
        run(&b"\x1b[15~hello"[..], tx);
        let mut all = Vec::new();
        for m in rx.iter() {
            match m {
                Msg::Input(b) => all.extend_from_slice(&b),
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(all, b"\x1b[15~hello".to_vec());
    }
}
