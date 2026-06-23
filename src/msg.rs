//! The merged message enum carried by the single unbounded channel into the
//! render thread. The PTY path is throttled upstream (bounded `sync_channel`),
//! so the merged channel is unbounded and an input `send()` never blocks —
//! a keystroke stays admissible under a multi-MB PTY flood.

use portable_pty::ExitStatus;

/// Everything the render thread (Thread 2) selects over, merged onto one
/// unbounded `std::sync::mpsc` channel.
///
/// - `Pty` — a chunk of raw child output from Thread 1 (the dumb byte pump).
/// - `Input` — a decoded crossterm event from Thread 3 (the input reader).
/// - `ChildExited` — the authoritative child-death signal from Thread 4 (the
///   waiter). This, **not** PTY EOF and **not** channel `Disconnected`, drives
///   shutdown. See ADR-009 / ADR-010.
#[derive(Debug)]
pub enum Msg {
    Pty(Vec<u8>),
    Input(crossterm::event::Event),
    ChildExited(ExitStatus),
}
