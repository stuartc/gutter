//! PTY setup and Thread 1 (the dumb byte pump).
//!
//! Opens a `portable-pty` `PtyPair` sized `W × real_rows` — the band width `W`,
//! NOT the real terminal width. This is the whole mechanism: the child believes
//! it owns a `W`-wide terminal and lays out absolutely within `W` (ADR-008's
//! `(rows, cols)` order is easy to transpose — keep it `cols: W`). The width
//! flows from a single place — [`spawn`]'s `cols` argument.
//!
//! [`reader`] reads the master in bounded chunks and forwards `Msg::Pty(bytes)`
//! through a bounded `sync_channel(N)` staging step (the backpressure seam from
//! ADR-007/009). It never scans for OSC, never detects child death, never
//! writes the PTY.

use std::io::Read;
use std::sync::mpsc::{Sender, SyncSender};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};

use crate::msg::Msg;

/// The one PTY-master call the resize handler makes: `master.resize(PtySize)`,
/// which issues `TIOCSWINSZ` and lets the **kernel** send SIGWINCH to the
/// child's foreground process group (ADR-008). Abstracted behind a trait so the
/// resize handler can be driven against a recording mock that captures the call
/// order alongside `set_size` — the seam the ADR-008 ordering test asserts on.
///
/// `cols` passed here is always the band width `W` (recomputed for a proportional
/// width — ADR-011), **never** `real_cols`.
pub trait PtyResizer {
    /// Resize the PTY to `cols × rows`. `cols` is the band width `W`.
    fn resize(&self, cols: u16, rows: u16) -> Result<(), String>;
}

/// The production resizer: wraps the `portable-pty` master. `resize` takes
/// `&self` on the master, so a shared handle is enough — no `&mut`.
pub struct MasterResizer {
    master: Box<dyn MasterPty + Send>,
}

impl MasterResizer {
    pub fn new(master: Box<dyn MasterPty + Send>) -> Self {
        Self { master }
    }
}

impl PtyResizer for MasterResizer {
    fn resize(&self, cols: u16, rows: u16) -> Result<(), String> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("pty resize: {e}"))
    }
}

/// Chunk size for a single PTY read. The kernel PTY buffer absorbs slack.
const READ_CHUNK: usize = 64 * 1024;

/// Depth of the bounded staging channel between the read loop and the merge
/// point. The backpressure seam from ADR-007 — Thread 1 blocks here when it
/// gets too far ahead of Thread 2. Ceiling ≈ `N × READ_CHUNK` (≈ 4MB at N=64),
/// which the kernel PTY buffer absorbs.
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
    // Without a cwd, portable-pty actively `current_dir($HOME)`s the child
    // (`cmdbuilder.rs` `dir = self.cwd.unwrap_or(home)`), so the child runs in
    // $HOME rather than where gutter was launched. Spawn it where the user is
    // standing. The error is swallowed deliberately: if the cwd cannot be read,
    // portable-pty's home fallback is a reasonable last resort over aborting.
    if let Ok(cwd) = std::env::current_dir() {
        builder.cwd(cwd);
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
            return;
        }
    }
    // The reader hit EOF, so `staging` closed and the loop drained every chunk.
    // Both the chunks and this sentinel come from this one thread, so the merged
    // channel delivers all `Msg::Pty` before `Msg::PtyEof` — the FIFO-per-sender
    // ordering the render loop's bounded shutdown drain relies on. PtyEof only
    // terminates that drain; it never triggers shutdown (the waiter is authoritative).
    let _ = merged.send(Msg::PtyEof);
}
