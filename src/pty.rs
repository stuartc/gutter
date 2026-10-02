//! PTY setup and Thread 1, the byte pump.
//!
//! [`spawn`] opens a PTY sized `W × real_rows` — the band width, never the real
//! terminal width — so the child lays out as if it owns a `W`-wide terminal.
//! [`reader`] pumps the master in bounded chunks into the staging channel; it
//! never scans bytes, never detects child death, and never writes the PTY.
//! [`forward_to_merge`] relays those chunks onto the merge as `Msg::Pty`.

use std::io::Read;
use std::sync::mpsc::{Sender, SyncSender};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};

use crate::msg::Msg;

/// Resizes the child's PTY via `TIOCSWINSZ`, which makes the kernel send
/// SIGWINCH to the child. See ADR-008. Behind a trait so the resize handler can
/// run against a recording mock that checks the call order against `set_size`.
pub trait PtyResizer {
    /// Resize the PTY to `cols × rows`. `cols` is always the band width `W`
    /// (recomputed for a proportional width — ADR-011), never the real width.
    fn resize(&self, cols: u16, rows: u16) -> Result<(), String>;
}

/// Production resizer wrapping the `portable-pty` master.
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

/// Depth of the bounded staging channel between the byte pump and the merge.
/// Thread 1 blocks here when it outruns Thread 2 — the backpressure seam of
/// ADR-009. Holds at most ≈ `N × READ_CHUNK` (≈ 4MB at N=64), which the kernel
/// PTY buffer absorbs.
pub const STAGING_DEPTH: usize = 64;

/// The handles a spawned PTY hands back to the rest of the program.
pub struct Pty {
    /// The PTY master. Held so [`take_writer`](MasterPty::take_writer) can hand
    /// the render thread its single write handle.
    pub master: Box<dyn MasterPty + Send>,
    /// Read handle for Thread 1, the byte pump.
    pub reader: Box<dyn Read + Send>,
    /// The child, handed to Thread 4 (the waiter), which loops on raw
    /// `waitpid(WUNTRACED | WCONTINUED)` to observe stops/continues as well as exit
    /// (ADR-018), rather than a plain `child.wait()`.
    pub child: Box<dyn Child + Send + Sync>,
}

/// Spawn `cmd` (with `args`) into a fresh PTY sized `cols × rows`.
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
    // A gutter started inside this one would truncate the same recording.
    builder.env_remove("GUTTER_RECORD");
    for arg in args {
        builder.arg(arg);
    }
    // portable-pty defaults the child's cwd to $HOME when none is set, so set
    // it to where gutter was launched. If the cwd can't be read we leave it
    // unset and accept that $HOME fallback rather than aborting.
    if let Ok(cwd) = std::env::current_dir() {
        builder.cwd(cwd);
    }

    let child = pair
        .slave
        .spawn_command(builder)
        .map_err(|e| format!("spawn_command: {e}"))?;
    // Drop the slave so the master sees EOF once the child and any
    // grandchildren close their handles.
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

/// Thread 1 — the byte pump. Reads `reader` in `READ_CHUNK` chunks, staging
/// each through the bounded `staging` channel. On EOF or read error it stops
/// without signalling shutdown: that is the waiter's job, because PTY EOF is
/// unreliable as a death signal.
pub fn reader(mut reader: Box<dyn Read + Send>, staging: SyncSender<Vec<u8>>) {
    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break, // EOF — stop reading, do NOT shut down.
            Ok(n) => {
                if staging.send(buf[..n].to_vec()).is_err() {
                    break; // merge side gone (render thread exited).
                }
            }
            Err(_) => break, // EIO after child exit, etc. — just stop.
        }
    }
}

/// Drains the bounded staging channel into the unbounded merge as `Msg::Pty`.
/// This is the seam that keeps PTY backpressure (the bounded `sync_channel`)
/// off the merge, which must stay always-admissible for input — ADR-009.
pub fn forward_to_merge(
    staging: std::sync::mpsc::Receiver<Vec<u8>>,
    merged: Sender<Msg>,
) {
    for chunk in staging {
        if merged.send(Msg::Pty(chunk)).is_err() {
            return;
        }
    }
    // EOF closed `staging`, so the loop has drained every chunk. Chunks and this
    // sentinel share one sender, so the merge delivers all `Msg::Pty` before
    // `Msg::PtyEof` — the FIFO ordering the render loop's shutdown drain relies
    // on. PtyEof only ends that drain; it never triggers shutdown (the waiter
    // does).
    let _ = merged.send(Msg::PtyEof);
}
