//! Thread 5 — terminal resize. `SIGWINCH` is delivered to gutter's process
//! group; with crossterm out of the input path (ADR-020) nothing else installs a
//! handler, so gutter registers its own through `signal-hook`.
//!
//! The message carries no payload. The render thread queries the real size
//! itself when it handles it, so two rapid resizes that coalesce into one signal
//! cannot leave it acting on a stale geometry.

use std::sync::mpsc::Sender;

use signal_hook::consts::SIGWINCH;
use signal_hook::iterator::Signals;

use crate::msg::Msg;

/// Forwards every `SIGWINCH` as [`Msg::Resize`] until the merged channel closes.
/// Detached like Thread 3 and reaped by `process::exit` (ADR-010).
pub fn run(merged: Sender<Msg>) {
    let Ok(mut signals) = Signals::new([SIGWINCH]) else {
        eprintln!("gutter: failed to register SIGWINCH; resize is disabled");
        return;
    };
    for _ in signals.forever() {
        if merged.send(Msg::Resize).is_err() {
            break;
        }
    }
}
