//! Thread 4 — the waiter. Blocks on `child.wait()` and, the instant it
//! returns, sends `Msg::ChildExited(status)` then exits. This is the
//! authoritative child-death signal — NOT PTY EOF (unreliable: EIO on Linux,
//! can block while a grandchild holds the slave fd).
