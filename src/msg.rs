//! The merged message enum carried into the render thread on one unbounded
//! channel. See ADR-009.

use portable_pty::ExitStatus;

/// Everything the render thread selects over, merged onto one unbounded
/// `std::sync::mpsc` channel.
///
/// - `Pty` — a chunk of raw child output from the PTY reader (Thread 1).
/// - `Input` — a chunk of raw outer-terminal input from the input reader
///   (Thread 3), uninterpreted.
/// - `Resize` — a `SIGWINCH` from the signal thread (Thread 5).
/// - `ChildExited` — the child-death signal from the waiter (Thread 4). The
///   authoritative shutdown trigger: PTY EOF never drives it, and a channel
///   `Disconnected` is only a backstop for when every sender drops without one.
/// - `ChildStopped` — the child stopped (`WIFSTOPPED`) from the waiter (Thread 4):
///   a self-suspend (nvim `:suspend`), a cooked-mode Ctrl-Z, or an external
///   SIGSTOP/SIGTTIN/SIGTTOU. Drives the suspend/resume cycle (ADR-0018/0019).
/// - `ChildContinued` — the child was continued (`WIFCONTINUED`). A repaint hint
///   only; never triggers suspend. Fires on gutter's own `continue_child` SIGCONT
///   too, which just costs one harmless redundant baseline reset.
/// - `PtyEof` — the PTY reader hit EOF after sending its final `Pty` chunk
///   (same-thread FIFO ordering). A drain terminator only: it tells the shutdown
///   path the child's last bytes have landed, so teardown can read
///   `outer_alt_active` without racing the final frame. It never triggers
///   shutdown.
#[derive(Debug)]
pub enum Msg {
    Pty(Vec<u8>),
    /// Raw bytes read from the outer tty. Uninterpreted — the render thread's
    /// scanner turns them into forwarded bytes, mouse reports and gutter-consumed
    /// keys (ADR-020).
    Input(Vec<u8>),
    /// The outer terminal resized. No payload: the render thread reads the real
    /// size when it handles this, so coalesced signals cannot leave it acting on
    /// a stale one.
    Resize,
    ChildExited(ExitStatus),
    /// The child stopped (`WIFSTOPPED`). `sig` is `WSTOPSIG`, carried for
    /// tests/logging; v1 reacts to every stop signal identically, so the binary
    /// itself never reads it.
    ChildStopped {
        #[allow(dead_code)]
        sig: i32,
    },
    /// The child was continued (`WIFCONTINUED`). A repaint hint only.
    ChildContinued,
    PtyEof,
}
