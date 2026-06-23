//! Thread 4 — the waiter. Blocks on `child.wait()` and, the instant it
//! returns, sends `Msg::ChildExited(status)` then exits. This is the
//! authoritative child-death signal — NOT PTY EOF (unreliable: EIO on Linux,
//! can block while a grandchild holds the slave fd).

use std::sync::mpsc::Sender;

use portable_pty::{Child, ExitStatus};

use crate::msg::Msg;

/// Thread 4 body. Blocks on `child.wait()`; on return, sends
/// `Msg::ChildExited(status)` into the merged channel and exits.
///
/// If `wait()` errors (it generally does not on a healthy child), synthesise a
/// non-zero status so the render loop still tears down rather than hanging.
pub fn run(mut child: Box<dyn Child + Send + Sync>, merged: Sender<Msg>) {
    let status = child
        .wait()
        .unwrap_or_else(|_| ExitStatus::with_exit_code(1));
    // If the render thread has already gone, the send fails harmlessly.
    let _ = merged.send(Msg::ChildExited(status));
}
