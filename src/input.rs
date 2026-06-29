//! Thread 3 — the input reader. Owns crossterm's event source exclusively;
//! crossterm 0.29 requires every `event::read()` to come from the same thread.
//! Forwards each decoded `Event` as `Msg::Input`. Never writes the PTY. Spawned
//! detached because `read()` can't be interrupted, and reaped by `process::exit`.

use std::sync::mpsc::Sender;

use crate::msg::Msg;

/// Reads events until the merged channel closes. A send error is the only exit:
/// it means the render thread is gone, since `event::read()` itself blocks until
/// the process exits.
pub fn run(merged: Sender<Msg>) {
    while let Ok(event) = crossterm::event::read() {
        if merged.send(Msg::Input(event)).is_err() {
            break;
        }
    }
}
