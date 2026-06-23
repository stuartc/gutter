//! Thread 3 — the input reader. Owns crossterm's event source exclusively and
//! is the only thread that ever calls `event::read()` (crossterm 0.29's
//! same-thread rule). Forwards each decoded `Event` as `Msg::Input`. Never
//! writes the PTY, never calls `poll()`. Detached at spawn (its `read()` is
//! un-interruptible) and reaped by `process::exit`.

use std::sync::mpsc::Sender;

use crate::msg::Msg;

/// Thread 3 body. Loops on the un-interruptible `event::read()`, forwarding
/// each decoded event as `Msg::Input`. It never returns under normal operation
/// — it is reaped by `process::exit` once the render thread tears down, which
/// is exactly why it is spawned **detached** (it cannot be joined). If the
/// merged channel closes (render thread gone), it stops.
pub fn run(merged: Sender<Msg>) {
    while let Ok(event) = crossterm::event::read() {
        if merged.send(Msg::Input(event)).is_err() {
            break;
        }
    }
}
