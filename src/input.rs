//! Thread 3 — the input reader. Owns crossterm's event source exclusively and
//! is the only thread that ever calls `event::read()` (crossterm 0.29's
//! same-thread rule). Forwards each decoded `Event` as `Msg::Input`. Never
//! writes the PTY, never calls `poll()`. Detached at spawn (its `read()` is
//! un-interruptible) and reaped by `process::exit`.
