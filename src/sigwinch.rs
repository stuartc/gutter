//! Thread 5 — terminal resize. `SIGWINCH` is delivered to gutter's process
//! group and nothing else in the binary installs a handler for it, so gutter
//! registers its own through `signal-hook` (ADR-020).
//!
//! The message carries no payload. The render thread queries the real size
//! itself when it handles it, so two rapid resizes that coalesce into one signal
//! cannot leave it acting on a stale geometry.

use std::sync::mpsc::Sender;

use signal_hook::consts::SIGWINCH;
use signal_hook::iterator::Signals;

use crate::msg::Msg;

/// Installs the handler. `main` calls this before it reads the terminal's size:
/// until the handler is in place `SIGWINCH` is ignored, and a resize that lands
/// after the size was read would never be seen.
pub fn register() -> std::io::Result<Signals> {
    Signals::new([SIGWINCH])
}

/// Forwards every `SIGWINCH` as [`Msg::Resize`] until the merged channel closes.
/// Detached like Thread 3 and reaped by `process::exit` (ADR-010).
pub fn run(mut signals: Signals, merged: Sender<Msg>) {
    for _ in signals.forever() {
        if merged.send(Msg::Resize).is_err() {
            break;
        }
    }
}
