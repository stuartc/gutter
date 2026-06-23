//! PTY setup and Thread 1 (the dumb byte pump).
//!
//! Opens a `portable-pty` `PtyPair` sized to the band width × `real_rows`. In
//! slice 01 the band width *is* the real terminal width (passthrough, no
//! offset yet); from slice 02 it becomes `W`. The width flows from a single
//! place — [`spawn`]'s `cols` argument — so slice 02 changes one call site.
//!
//! [`reader`] reads the master in bounded chunks and forwards `Msg::Pty(bytes)`
//! through a bounded `sync_channel(N)` staging step (the backpressure seam from
//! ADR-007; its N is slice 02's to tune). It never scans for OSC, never detects
//! child death, never writes the PTY.

use std::io::Read;
use std::sync::mpsc::{Sender, SyncSender};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};

use crate::msg::Msg;

/// Chunk size for a single PTY read. The kernel PTY buffer absorbs slack.
const READ_CHUNK: usize = 64 * 1024;

/// Depth of the bounded staging channel between the read loop and the merge
/// point. The backpressure seam from ADR-007 — Thread 1 blocks here when it
/// gets too far ahead of Thread 2. Real tuning of `N` is slice 02's concern.
pub const STAGING_DEPTH: usize = 64;

/// The handles a spawned PTY hands back to the rest of the program.
pub struct Pty {
    /// The PTY master. Held so [`take_writer`](MasterPty::take_writer) can
    /// produce the single write handle owned by the render thread.
    pub master: Box<dyn MasterPty + Send>,
    /// A cloned read handle for Thread 1 (the byte pump).
    pub reader: Box<dyn Read + Send>,
    /// The spawned child, handed to Thread 4 (the waiter) for `child.wait()`.
    pub child: Box<dyn Child + Send + Sync>,
}

/// Spawn `cmd` (with `args`) into a fresh PTY sized `cols × rows`.
///
/// Keeps spawn-only concerns here: size, `CommandBuilder` assembly, pair
/// creation, and handing out the master/reader/child handles. The waiter loop
/// and teardown deliberately live elsewhere (`waiter.rs` / `render.rs`).
pub fn spawn(
    cmd: &str,
    args: &[String],
    cols: u16,
    rows: u16,
) -> Result<Pty, String> {
    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("openpty: {e}"))?;

    let mut builder = CommandBuilder::new(cmd);
    for arg in args {
        builder.arg(arg);
    }

    let child = pair
        .slave
        .spawn_command(builder)
        .map_err(|e| format!("spawn_command: {e}"))?;
    // The slave handle is no longer needed once the child is spawned; dropping
    // it lets the master see EOF when the child (and any grandchildren) close.
    drop(pair.slave);

    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("try_clone_reader: {e}"))?;

    Ok(Pty {
        master: pair.master,
        reader,
        child,
    })
}

/// Thread 1 body — the dumb byte pump.
///
/// Reads `reader` in `READ_CHUNK`-sized chunks. Each chunk is staged through
/// the bounded `staging` channel (backpressure) and then merged into Thread 2.
/// On EOF or read error it simply stops — it does **not** signal shutdown
/// (that is the waiter's job; PTY EOF is unreliable per ADR-010).
pub fn reader(mut reader: Box<dyn Read + Send>, staging: SyncSender<Vec<u8>>) {
    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,            // EOF — stop reading, do NOT shut down.
            Ok(n) => {
                if staging.send(buf[..n].to_vec()).is_err() {
                    // Merge side gone (render thread exited) — nothing to do.
                    break;
                }
            }
            Err(_) => break, // EIO after child exit etc. — just stop.
        }
    }
}

/// Drain the bounded staging channel into the unbounded merged channel as
/// `Msg::Pty`. This thin forwarder is the merge point: it keeps the PTY path's
/// backpressure (the bounded `sync_channel`) separate from the unbounded merge
/// that keeps input always-admissible (ADR-009).
pub fn forward_to_merge(
    staging: std::sync::mpsc::Receiver<Vec<u8>>,
    merged: Sender<Msg>,
) {
    for chunk in staging {
        if merged.send(Msg::Pty(chunk)).is_err() {
            break;
        }
    }
}
