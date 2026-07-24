//! Shared harness for the PTY integration suites: spawn gutter under a real outer
//! PTY, drain what it wrote, and read the result back as a grid.
//!
//! Each test binary compiles its own copy of this file and uses a subset of it, so
//! unused helpers are expected rather than dead.
#![allow(dead_code)]

use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use expectrl::session::OsSession;

/// Serialize every PTY test in a binary — run in parallel they flake under
/// PTY/process contention (issue #1).
pub fn pty_guard() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Spawn gutter under a real `cols × rows` outer PTY wrapping `gutter_args` (a
/// single string, so the inner `sh` parses any nested quoting).
pub fn spawn_gutter(cols: u16, rows: u16, gutter_args: &str) -> OsSession {
    let script = format!(
        "stty cols {cols} rows {rows}; exec env GUTTER_FORCE_ANCHOR_ROW=0 TERM=xterm-256color {} {gutter_args}",
        env!("CARGO_BIN_EXE_gutter")
    );
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.arg("-c").arg(script);
    OsSession::spawn(cmd).expect("spawn gutter under PTY")
}

/// Drain a bounded wall-clock window with non-blocking reads, returning every byte
/// the outer terminal saw. The non-blocking reads are what make the window a real
/// cap even while the child holds the PTY open. Stops early on EOF.
pub fn drain_window(session: &mut OsSession, window: Duration) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    let start = Instant::now();
    while start.elapsed() < window {
        match session.try_read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
        std::thread::sleep(Duration::from_millis(3));
    }
    out
}

/// Where `needle` first appears in `hay`.
pub fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The outer terminal's bytes parsed at its physical size and joined to a single
/// string, so a test can search for echoed content regardless of which row it
/// landed on.
pub fn grid_text(bytes: &[u8], cols: u16, rows: u16) -> String {
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(bytes);
    parser.screen().rows(0, cols).collect::<Vec<_>>().join("\n")
}

/// Run a child that emits `emit` and lingers, and return everything the outer
/// terminal saw.
pub fn outer_bytes_for(emit: &str) -> Vec<u8> {
    let mut session = spawn_gutter(80, 24, &format!("--width 40 sh -c 'printf \"{emit}\"; sleep 0.5'"));
    let out = drain_window(&mut session, Duration::from_secs(2));
    drop(session);
    out
}
