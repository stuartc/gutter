//! gutter — a one-pane terminal multiplexer that runs a command inside a
//! narrower column band, transparent to keyboard/mouse/clipboard/resize.
//!
//! Architecture: three live threads + a waiter, one merged unbounded channel,
//! no async runtime. See `.context/design/CONTEXT.md` for the full design brief.
//!
//! - Thread 1 (`pty::reader`)   — dumb PTY byte pump, self-throttled.
//! - Thread 2 (`render::run`)    — owns the vt100 parser + outer terminal; the
//!   only writer of the PTY master and the only thread touching outer output.
//! - Thread 3 (`input::reader`)  — owns crossterm's event source exclusively.
//! - Thread 4 (`waiter`)         — blocks on `child.wait()`, the authoritative
//!   child-death signal.

mod callbacks;
mod cli;
mod clipboard;
mod clock;
mod input;
mod keyboard;
mod mouse;
mod msg;
mod pty;
mod render;
mod resize;
mod terminal;
mod waiter;
mod width;

fn main() {
    // Wiring (arg parse → terminal setup → spawn PTY + threads → run render
    // loop → restore + exit code) lands in milestone 1.
}
