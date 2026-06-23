//! PTY setup and Thread 1 (the dumb byte pump).
//!
//! Opens a `portable-pty` `PtyPair` sized `W × real_rows` (the child must
//! believe it owns a `W`-column terminal, never the real width). `reader`
//! reads the master in bounded chunks, self-throttled by a bounded
//! `sync_channel(N)` staging step, and forwards `Msg::Pty(bytes)`. It never
//! scans for OSC, never detects child death, never writes the PTY.
