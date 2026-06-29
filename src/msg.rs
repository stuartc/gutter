//! The merged message enum carried into the render thread on one unbounded
//! channel. See ADR-009.

use portable_pty::ExitStatus;

/// Everything the render thread selects over, merged onto one unbounded
/// `std::sync::mpsc` channel.
///
/// - `Pty` — a chunk of raw child output from the PTY reader (Thread 1).
/// - `Input` — a decoded crossterm event from the input reader (Thread 3).
/// - `ChildExited` — the child-death signal from the waiter (Thread 4). The
///   authoritative shutdown trigger: PTY EOF never drives it, and a channel
///   `Disconnected` is only a backstop for when every sender drops without one.
/// - `PtyEof` — the PTY reader hit EOF after sending its final `Pty` chunk
///   (same-thread FIFO ordering). A drain terminator only: it tells the shutdown
///   path the child's last bytes have landed, so teardown can read
///   `outer_alt_active` without racing the final frame. It never triggers
///   shutdown.
#[derive(Debug)]
pub enum Msg {
    Pty(Vec<u8>),
    Input(crossterm::event::Event),
    ChildExited(ExitStatus),
    PtyEof,
}
