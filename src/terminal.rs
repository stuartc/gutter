//! Outer-terminal handle, owned by Thread 2: terminal lifecycle plus the render
//! output sink, behind one injectable trait so the render path can be tested
//! against a recording mock.
//!
//! The sink is the process's terminal, not stdout: gutter paints the band on the
//! screen the keyboard, the size and the clipboard already come from, and
//! `gutter cmd > log` leaves the log empty.
//!
//! Lifecycle is raw mode, the mirrored alt screen, and an explicit ordered
//! restore (ADR-010). Teardown runs before `process::exit`, which skips
//! destructors — so it cannot be a `Drop` guard. Setup and teardown are
//! symmetric: each restore step undoes only what was actually set up. The alt
//! screen is not forced at setup; it mirrors the child's mode (ADR-012).

use std::ffi::{CStr, OsStr};
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::queue;
use crossterm::terminal::{Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};

use crate::geometry::Rails;
use crate::mouse::{MOUSE_DISABLE, MOUSE_ENABLE};

/// The outer-terminal side effects the setup, render and teardown paths perform.
///
/// [`CrosstermTerminal`] wraps crossterm against `/dev/tty`; [`mock::MockTerminal`]
/// records the ordered calls so the restore-order and offset-repaint tests can
/// assert against them.
pub trait OuterTerminal {
    // --- Setup ---
    /// Enter raw mode. The first setup step once the PTY is up.
    fn enable_raw_mode(&mut self) -> io::Result<()>;
    /// Enable mouse capture eagerly (ADR-005), emitting the fixed any-motion SGR
    /// bundle. Called once at startup; never re-issued to track the child's mode
    /// (the forwarding gate narrows in software). Paired with [`disable_mouse`].
    ///
    /// [`disable_mouse`]: OuterTerminal::disable_mouse
    fn enable_mouse(&mut self) -> io::Result<()>;
    /// Enter the alternate screen, mirroring the child's `?1049h` edge (ADR-012).
    /// gutter never forces the alt screen at setup; called mid-run only when the
    /// child enters it.
    fn enter_alt_screen(&mut self) -> io::Result<()>;
    /// The outer terminal's current size as `(cols, rows)`. Read on resume to catch
    /// a resize that happened while gutter was suspended (ADR-0019): a pending
    /// SIGWINCH can coalesce a resize-and-back to a stale event, so the cycle
    /// queries the real size explicitly.
    fn terminal_size(&mut self) -> io::Result<(u16, u16)>;

    // --- Render output (per frame) ---
    /// Move the cursor to physical `(col, row)`, emitted before each repainted
    /// row so the row's bytes land at the band's left margin.
    fn move_to(&mut self, col: u16, row: u16) -> io::Result<()>;
    /// Write a row's byte run verbatim. `prepare_row_into` has already clipped it for
    /// one [`Placement`] (ADR-014): it carries its own intra-row SGR, its erases stop at
    /// the band's right edge, and every position inside it is an absolute `CUP` naming a
    /// physical cell on that one row at that one margin.
    ///
    /// So the payload is good at the placement it was built for and nowhere else.
    /// Buffering one and replaying it at another row or margin paints outside the band,
    /// onto cells the diff baseline never repaints.
    ///
    /// [`Placement`]: crate::rowclip::Placement
    fn write_row(&mut self, bytes: &[u8]) -> io::Result<()>;
    /// Advance the real terminal one line (`\r\n`), scrolling when on the bottom
    /// row. The scroll-aware primary paint emits a departed top line then this
    /// newline so the line enters the real terminal's own scrollback — gutter
    /// keeps vt100 at `scrollback=0` and lets the real terminal be the store
    /// (ADR-013).
    ///
    /// An implementation MUST reset the SGR state before it scrolls. A terminal fills
    /// the line that scrolls in with the active background, across the full physical
    /// width — so a newline under a painted row's trailing attribute colours both
    /// gutters on a row the diff baseline never repaints, the same escape from the band
    /// ADR-014's `ESC[K` bounding exists to prevent. Callers scroll from wherever the
    /// last row left the cursor's attributes, so the reset belongs here.
    fn newline(&mut self) -> io::Result<()>;
    /// Clear the gutter columns outside the band `[margin, margin + width)` across rows
    /// `[row_start, row_end)`. The row span is what makes this safe on the primary
    /// screen: the caller passes `base_row..rows`, so shell history above the inline
    /// band is never touched. On the alt screen the caller passes `0..rows`.
    ///
    /// Clears to blank: while a child owns the band, the gutter columns on the band's
    /// rows are gutter-owned and reliably empty, so there is nothing to preserve.
    fn clear_gutter(
        &mut self,
        margin: u16,
        width: u16,
        real_cols: u16,
        row_start: u16,
        row_end: u16,
    ) -> io::Result<()>;
    /// Clear whole physical rows `[row_start, row_end)`, band interior included —
    /// unlike [`clear_gutter`], which spares the band columns. Used after a resize
    /// widens the band: the old, narrower band's glyphs now sit inside the new band's
    /// columns, where neither the gutter clear nor the diff repaint (baseline is blank)
    /// reaches them. The caller supplies the ADR-017 span (`0..rows` on the alt screen,
    /// `base_row..rows` on the primary screen) so shell history above the band survives.
    ///
    /// [`clear_gutter`]: OuterTerminal::clear_gutter
    fn clear_row_span(&mut self, row_start: u16, row_end: u16) -> io::Result<()>;
    /// Paint the resize rails + width readout (faint, monochrome). Emitted while in
    /// resize mode, into gutter columns only (plus the in-band readout fallback). The
    /// per-frame band repaint never touches these columns, so the rails persist between
    /// resizes until a mode-exit clear erases them.
    fn draw_rails(&mut self, rails: &Rails) -> io::Result<()>;
    /// Reposition the real cursor inside the band after the repaint, from the
    /// child's cursor position.
    fn place_cursor(&mut self, col: u16, row: u16) -> io::Result<()>;
    /// Mirror the child's cursor visibility (DECTCEM).
    fn set_cursor_visible(&mut self, visible: bool) -> io::Result<()>;
    /// Mirror the child's cursor shape (DECSCUSR). `bytes` is the ready-made
    /// `CSI Ps SP q` the watcher produced, forwarded verbatim. Emitted only on a
    /// real shape change (the watcher de-dupes).
    fn set_cursor_shape(&mut self, bytes: &[u8]) -> io::Result<()>;
    /// Flush the queued frame to the real terminal. Exactly once per frame.
    fn flush(&mut self) -> io::Result<()>;

    // --- Child-driven keyboard modes (ADR-021) ---
    /// Write child-originated keyboard-mode bytes to the real terminal verbatim, and
    /// flush them: the child may be blocked waiting on the round trip, and the sink is
    /// buffered. The bytes are the relay's canonical forms, so this method
    /// neither builds nor inspects them.
    ///
    /// Distinct from [`write_row`] so the recorded call log keeps relay bytes apart
    /// from row paints — the ADR-010 and ADR-019 ordering assertions read that log.
    ///
    /// [`write_row`]: OuterTerminal::write_row
    fn relay(&mut self, bytes: &[u8]) -> io::Result<()>;

    // --- Teardown (ADR-010 order) ---
    /// Leave the alternate screen — conditional on the outer terminal actually
    /// being in it (ADR-012): a plain command never entered, so teardown skips the
    /// leave. Also called mid-run on the child's alt→primary edge.
    fn leave_alt_screen(&mut self) -> io::Result<()>;
    /// Disable mouse capture (teardown). Pairs with [`enable_mouse`]. Runs via the
    /// explicit restore, not a `Drop` guard — `process::exit` skips destructors,
    /// which would leave the shell emitting mouse escapes after gutter dies.
    ///
    /// [`enable_mouse`]: OuterTerminal::enable_mouse
    fn disable_mouse(&mut self) -> io::Result<()>;
    /// Show the cursor (teardown).
    fn show_cursor(&mut self) -> io::Result<()>;
    /// Disable raw mode (teardown). Must run after leaving the alt screen, or
    /// control sequences leak to the user's shell.
    fn disable_raw_mode(&mut self) -> io::Result<()>;
}

/// Opens the terminal for writing — the band's sink — and names the device it came
/// from, so every other handle gutter opens is opened from that same device.
///
/// `/dev/tty`, the controlling terminal, first. A process handed a terminal on its
/// stdio without that terminal being made its controlling terminal has no `/dev/tty`
/// to open — `setsid gutter bash`, or any launcher that attaches a PTY but omits
/// `TIOCSCTTY` — so the terminal gutter's own stdio names is the second route, reopened
/// **by name**: `dup`ing a descriptor would hand back the caller's file description,
/// where the clipboard (ADR-004) and the probe/Thread-3 split both need independent ones.
///
/// The open doubles as gutter's terminal guard: both routes failing means there is no
/// screen to render a band on. Which route failed is in the error — the fallback's
/// names the device it could not open, so a refusal there does not read as `/dev/tty`'s.
pub fn open_tty_write() -> io::Result<(File, PathBuf)> {
    let dev_tty = PathBuf::from("/dev/tty");
    let no_ctty = match open_write(&dev_tty) {
        Ok(f) => return Ok((f, dev_tty)),
        Err(e) => e,
    };
    let Some(path) = stdio_tty_path() else {
        return Err(no_ctty);
    };
    match open_write(&path) {
        Ok(f) => Ok((f, path)),
        // Not `no_ctty`: gutter did find a screen and was refused when it reopened it,
        // and reporting `/dev/tty`'s reason instead sends the reader at the wrong device.
        Err(e) => Err(io::Error::new(
            e.kind(),
            format!("{}: {e}", path.display()),
        )),
    }
}

/// The terminal's `(cols, rows)`, asked of the device gutter resolved.
///
/// Not crossterm's `terminal::size`, which runs its own resolution — its own `/dev/tty`
/// open, then an ioctl on stdout, then `tput`, then 80×24. On the fallback route that
/// answers for a different device than the band is painted on, or for no terminal at
/// all with stdout redirected, and the size is the one number that positions the band
/// and sizes the child's PTY (ADR-023).
pub fn tty_size(tty: &File) -> io::Result<(u16, u16)> {
    fd_size(tty.as_raw_fd())
}

fn fd_size(fd: RawFd) -> io::Result<(u16, u16)> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: `TIOCGWINSZ` writes one `winsize` through the pointer it is given, and
    // the descriptor is borrowed from a live `File` for the length of the call.
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ as _, &mut ws) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // A terminal that reports a zero dimension has no usable geometry; treat it as a
    // failed query so the caller takes its own fallback rather than laying out a
    // zero-column band.
    if ws.ws_col == 0 || ws.ws_row == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the terminal reports a zero dimension",
        ));
    }
    Ok((ws.ws_col, ws.ws_row))
}

/// `O_NOCTTY`: naming a terminal must not make it gutter's controlling terminal.
/// Where the fallback fires gutter is a session leader with none, and on Linux a
/// plain `open` of a free terminal would silently adopt it — the fallback widens how
/// gutter finds a screen, not what session it is in.
fn open_write(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(path)
}

/// The path of the terminal on gutter's stdio, if any of the three descriptors names
/// one.
///
/// All three, not stdin alone: a launcher that attaches a PTY usually puts it on 0, 1
/// and 2, but a supervisor that pipes stdin — `setsid sh -c 'true | gutter bash'` — still
/// leaves a usable screen on 1 or 2, and refusing to run there would be refusing a
/// terminal gutter can see.
fn stdio_tty_path() -> Option<PathBuf> {
    [
        libc::STDIN_FILENO,
        libc::STDOUT_FILENO,
        libc::STDERR_FILENO,
    ]
    .into_iter()
    .find_map(tty_path)
}

/// The path of the terminal on `fd`, if it is one.
fn tty_path(fd: i32) -> Option<PathBuf> {
    let mut buf = [0u8; libc::PATH_MAX as usize];
    // SAFETY: `ttyname_r` writes at most `buf.len()` bytes into the buffer it is
    // given, and the three standard descriptor numbers are always valid to ask about.
    let rc = unsafe { libc::ttyname_r(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return None;
    }
    let name = CStr::from_bytes_until_nul(&buf).ok()?.to_bytes();
    (!name.is_empty()).then(|| PathBuf::from(OsStr::from_bytes(name)))
}

/// The outer terminal's file, optionally teed to a trace file.
///
/// Where a glyph landed on the real screen is only recoverable from the bytes
/// gutter wrote, and once the terminal has drawn them they are gone. Setting
/// `GUTTER_TRACE_OUT=<path>` appends every byte to that path as well, so a
/// corrupting frame seen on a real terminal can be replayed offline.
pub struct TracedTty {
    tty: File,
    trace: Option<File>,
}

impl TracedTty {
    fn new(tty: File) -> Self {
        let trace = std::env::var_os("GUTTER_TRACE_OUT").and_then(|p| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .ok()
        });
        Self { tty, trace }
    }
}

impl Write for TracedTty {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.tty.write(buf)?;
        if let Some(t) = self.trace.as_mut() {
            // Best-effort: a failed trace write must never disturb the render path.
            let _ = t.write_all(&buf[..n]);
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(t) = self.trace.as_mut() {
            let _ = t.flush();
        }
        self.tty.flush()
    }
}

/// The real outer terminal, backed by crossterm against the terminal
/// [`open_tty_write`] resolved.
pub struct CrosstermTerminal<W: Write = TracedTty> {
    /// Buffered: one `move_to` is several small writes, and unbuffered they would
    /// be several syscalls. Every write here is landed by an explicit flush — no
    /// destructor runs before `process::exit` (ADR-010).
    out: BufWriter<W>,
    /// The sink's descriptor, captured at construction — the size and termios
    /// ioctls need one, and a writer that is not a terminal has none.
    fd: Option<RawFd>,
    /// What `terminal_size` answers when the sink is not a terminal.
    size_override: Option<(u16, u16)>,
    /// Whether mouse capture was enabled at startup, so teardown disables only
    /// what it set.
    mouse_enabled: bool,
    /// The terminal's line settings as gutter found them, saved on the first raw-mode
    /// enable and never overwritten — the suspend cycle's park/unpark re-enables raw
    /// mode over a state park itself restored, and saving again there would make
    /// teardown hand the shell back what park left rather than what the user had.
    /// `None` means raw mode was never taken, so there is nothing to put back.
    saved_termios: Option<libc::termios>,
}

impl CrosstermTerminal<TracedTty> {
    pub fn new(tty: File) -> Self {
        let fd = tty.as_raw_fd();
        Self {
            out: BufWriter::new(TracedTty::new(tty)),
            fd: Some(fd),
            size_override: None,
            mouse_enabled: false,
            saved_termios: None,
        }
    }
}

impl<W: Write> CrosstermTerminal<W> {
    /// The production emitters over an arbitrary sink, for the oracle's tape: no
    /// descriptor, so the size is declared and the raw-mode calls are no-ops.
    // Used by the oracle's Tape, which is only constructed from tests.
    #[cfg(feature = "oracle")]
    #[allow(dead_code)]
    pub fn from_writer(w: W, size: (u16, u16)) -> Self {
        Self {
            out: BufWriter::new(w),
            fd: None,
            size_override: Some(size),
            mouse_enabled: false,
            saved_termios: None,
        }
    }

    /// The band's sink, for the startup CPR probe: its query has to leave by the
    /// same terminal the reply comes back from.
    pub fn writer(&mut self) -> &mut impl Write {
        &mut self.out
    }

    /// The sink itself, for the oracle's tape. Buffered writes not yet flushed are
    /// dropped, so flush first.
    #[cfg(feature = "oracle")]
    #[allow(dead_code)]
    pub fn into_writer(self) -> W {
        match self.out.into_inner() {
            Ok(w) => w,
            Err(_) => panic!("the buffer was flushed"),
        }
    }

    fn fd(&self) -> Option<RawFd> {
        self.fd
    }
}

impl<W: Write> OuterTerminal for CrosstermTerminal<W> {
    /// Raw mode on the resolved device, by `termios` rather than through crossterm.
    ///
    /// crossterm sets it on stdin when stdin is a terminal and reopens `/dev/tty`
    /// otherwise — a third answer to which terminal gutter is talking to, and one that
    /// fails outright on the fallback route when stdin is a pipe. The line settings live
    /// on the device, not on the descriptor, so setting them through the band's sink is
    /// what the read side sees too (ADR-023).
    fn enable_raw_mode(&mut self) -> io::Result<()> {
        let Some(fd) = self.fd() else { return Ok(()) };
        let mut termios = current_termios(fd)?;
        if self.saved_termios.is_none() {
            self.saved_termios = Some(termios);
        }
        // SAFETY: `cfmakeraw` only rewrites the flags of the struct it is handed.
        unsafe { libc::cfmakeraw(&mut termios) };
        set_termios(fd, &termios)
    }

    fn enable_mouse(&mut self) -> io::Result<()> {
        self.out.write_all(MOUSE_ENABLE)?;
        self.out.flush()?;
        self.mouse_enabled = true;
        Ok(())
    }

    fn enter_alt_screen(&mut self) -> io::Result<()> {
        queue!(self.out, EnterAlternateScreen)?;
        self.out.flush()
    }

    fn terminal_size(&mut self) -> io::Result<(u16, u16)> {
        match (self.size_override, self.fd) {
            (Some(size), _) => Ok(size),
            (None, Some(fd)) => fd_size(fd),
            (None, None) => Err(io::Error::other("no terminal to size")),
        }
    }

    fn move_to(&mut self, col: u16, row: u16) -> io::Result<()> {
        queue!(self.out, MoveTo(col, row))
    }

    fn write_row(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.out.write_all(bytes)
    }

    fn newline(&mut self) -> io::Result<()> {
        // `\r\n` returns to column 0 then advances a line; on the bottom row the
        // terminal scrolls and the line just written enters its scrollback. The
        // leading `ESC[m` is the trait's reset — the line that scrolls in is filled
        // with the active background across the whole screen, gutters included.
        self.out.write_all(b"\x1b[m\r\n")
    }

    fn clear_gutter(
        &mut self,
        margin: u16,
        width: u16,
        real_cols: u16,
        row_start: u16,
        row_end: u16,
    ) -> io::Result<()> {
        let band_end = margin.saturating_add(width).min(real_cols);
        let left = b" ".repeat(margin as usize);
        let right = b" ".repeat(real_cols.saturating_sub(band_end) as usize);
        // Reset SGR first so the blanks are painted with the default background
        // (a leftover colour run would tint the gutter).
        self.out.write_all(b"\x1b[0m")?;
        for row in row_start..row_end {
            // Left gutter: physical columns [0, margin).
            if margin > 0 {
                queue!(self.out, MoveTo(0, row))?;
                self.out.write_all(&left)?;
            }
            // Right gutter: physical columns [band_end, real_cols).
            if real_cols > band_end {
                queue!(self.out, MoveTo(band_end, row))?;
                self.out.write_all(&right)?;
            }
        }
        Ok(())
    }

    fn clear_row_span(&mut self, row_start: u16, row_end: u16) -> io::Result<()> {
        // Reset SGR first so the cleared rows carry the default background (a leftover
        // colour run would tint them), matching clear_gutter.
        self.out.write_all(b"\x1b[0m")?;
        for row in row_start..row_end {
            queue!(self.out, MoveTo(0, row), Clear(ClearType::CurrentLine))?;
        }
        Ok(())
    }

    fn draw_rails(&mut self, rails: &Rails) -> io::Result<()> {
        self.out.write_all(b"\x1b[0m")?; // drop any leftover attribute run
        for row in rails.row_start..rails.row_end {
            if let Some(c) = rails.left_col {
                queue!(self.out, MoveTo(c, row))?;
                self.out.write_all("\x1b[2m\u{258f}\x1b[0m".as_bytes())?; // ▏
            }
            if let Some(c) = rails.right_col {
                queue!(self.out, MoveTo(c, row))?;
                self.out.write_all("\x1b[2m\u{2595}\x1b[0m".as_bytes())?; // ▕
            }
        }
        if let Some(r) = &rails.readout {
            queue!(self.out, MoveTo(r.col, r.row))?;
            self.out
                .write_all(format!("\x1b[2m{}\x1b[0m", r.text).as_bytes())?;
        }
        Ok(())
    }

    fn place_cursor(&mut self, col: u16, row: u16) -> io::Result<()> {
        self.move_to(col, row)
    }

    fn set_cursor_visible(&mut self, visible: bool) -> io::Result<()> {
        if visible {
            queue!(self.out, Show)
        } else {
            queue!(self.out, Hide)
        }
    }

    fn set_cursor_shape(&mut self, bytes: &[u8]) -> io::Result<()> {
        // Mirror what the child requested rather than re-deriving it.
        self.out.write_all(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }

    fn relay(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.out.write_all(bytes)?;
        self.out.flush()
    }

    fn leave_alt_screen(&mut self) -> io::Result<()> {
        queue!(self.out, LeaveAlternateScreen)?;
        self.out.flush()
    }

    fn disable_mouse(&mut self) -> io::Result<()> {
        // Disable only what was enabled. Emitting `DisableMouseCapture` when we
        // never captured would be harmless, but mirroring the push/pop rule keeps
        // the contract clean.
        if self.mouse_enabled {
            self.out.write_all(MOUSE_DISABLE)?;
            self.out.flush()?;
            self.mouse_enabled = false;
        }
        Ok(())
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        queue!(self.out, Show)?;
        self.out.flush()
    }

    /// Put back the line settings gutter found, if it ever took raw mode. Undoes only
    /// what was set up (ADR-010), and restores the user's own settings rather than a
    /// canonical default.
    fn disable_raw_mode(&mut self) -> io::Result<()> {
        match (self.saved_termios, self.fd()) {
            (Some(saved), Some(fd)) => set_termios(fd, &saved),
            _ => Ok(()),
        }
    }
}

/// The terminal's current line settings.
fn current_termios(fd: i32) -> io::Result<libc::termios> {
    // SAFETY: `tcgetattr` writes one `termios` through the pointer it is given.
    let mut termios: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut termios) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(termios)
}

/// Apply line settings, at once rather than after the output queue drains: gutter's own
/// frames are already in flight, and waiting on them would let the mode change lag the
/// keystroke that caused it.
fn set_termios(fd: i32, termios: &libc::termios) -> io::Result<()> {
    // SAFETY: `tcsetattr` only reads the `termios` it is given.
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, termios) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! [`CrosstermTerminal`] against a plain file: the sink is whatever `File` it
    //! was handed, so its buffering is testable without a terminal.

    use super::*;
    use std::path::PathBuf;

    /// A temp path that removes itself, so a test can read back what the sink wrote.
    struct SinkFile(PathBuf);

    impl SinkFile {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("gutter-sink-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_file(&path);
            Self(path)
        }

        fn terminal(&self) -> CrosstermTerminal {
            CrosstermTerminal::new(File::create(&self.0).expect("create the sink file"))
        }

        fn contents(&self) -> Vec<u8> {
            std::fs::read(&self.0).expect("read the sink file")
        }
    }

    impl Drop for SinkFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// The sink is buffered, so a queued frame reaches the terminal only when
    /// something flushes it. `process::exit` runs no destructor that would land it
    /// (ADR-010).
    #[test]
    fn queued_output_lands_only_on_flush() {
        let sink = SinkFile::new("flush");
        let mut term = sink.terminal();

        term.move_to(4, 2).expect("move to the band");
        term.write_row(b"band").expect("write a row");
        assert!(
            sink.contents().is_empty(),
            "queued output must not reach the terminal before the flush"
        );

        term.flush().expect("flush the sink");
        let out = sink.contents();
        assert!(
            out.ends_with(b"band"),
            "the flush must land the queued frame, got {:?}",
            String::from_utf8_lossy(&out)
        );
    }

    /// `show_cursor` is the last byte-producing step of the ordered restore, and it
    /// flushes — so everything the teardown queued ahead of it lands.
    #[test]
    fn show_cursor_lands_what_teardown_queued_before_it() {
        let sink = SinkFile::new("teardown");
        let mut term = sink.terminal();

        term.move_to(0, 5).expect("move below the band");
        term.write_row(b"\r\n\x1b[2mExited with: 3\x1b[0m")
            .expect("write the hand-back status");
        term.show_cursor().expect("show the cursor");

        let out = String::from_utf8_lossy(&sink.contents()).into_owned();
        assert!(
            out.contains("Exited with: 3"),
            "the restore's queued bytes must land, got {out:?}"
        );
    }

    /// The probe writes through the band's own sink: its query and the frames that
    /// follow it are the same buffered stream to the same terminal.
    #[test]
    fn the_probe_writes_through_the_band_sink() {
        let sink = SinkFile::new("probe");
        let mut term = sink.terminal();

        term.writer().write_all(b"\x1b[6n").expect("write the query");
        term.write_row(b"band").expect("write a row");
        term.flush().expect("flush the sink");

        let out = sink.contents();
        assert_eq!(
            out,
            b"\x1b[6nband",
            "query and band must share one sink, in order"
        );
    }
}

#[cfg(test)]
pub mod mock {
    //! A recording [`OuterTerminal`] for the restore-order and offset-repaint
    //! tests.

    use super::OuterTerminal;
    use std::io;

    /// One recorded side effect, in the order it was invoked. Render-output calls
    /// carry their arguments so physical-column assertions can read back what was
    /// painted where.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Call {
        EnableRawMode,
        EnableMouse,
        EnterAltScreen,
        MoveTo(u16, u16),
        WriteRow(Vec<u8>),
        /// `\r\n` — a departed line advanced into the real terminal's scrollback
        /// (ADR-013). The test asserts one `Newline` per departed line, not per
        /// frame.
        Newline,
        /// `clear_gutter(margin, width, real_cols, row_start, row_end)`.
        ClearGutter(u16, u16, u16, u16, u16),
        /// `clear_row_span(row_start, row_end)`.
        ClearRowSpan(u16, u16),
        /// `draw_rails(rails)`.
        DrawRails(crate::geometry::Rails),
        PlaceCursor(u16, u16),
        SetCursorVisible(bool),
        /// `set_cursor_shape(bytes)` — the mirrored `CSI Ps SP q`.
        SetCursorShape(Vec<u8>),
        /// `relay(bytes)` — a keyboard-mode request forwarded on the child's behalf,
        /// or the reset/replay that undoes and re-applies them (ADR-021). Its own
        /// variant so the ordering filters can see it without confusing it with a
        /// row paint.
        Relay(Vec<u8>),
        Flush,
        LeaveAltScreen,
        DisableMouse,
        ShowCursor,
        DisableRawMode,
        /// `Suspender::suspend_self` — `kill(0, SIGTSTP)`. Recorded by the mock
        /// suspender into the shared order log, so the suspend cycle's restore /
        /// self-stop / re-setup ordering is one assertable sequence (ADR-0019).
        SuspendSelf,
        /// `Suspender::continue_child` — `kill(-pgid, SIGCONT)`.
        ContinueChild,
    }

    /// Records the ordered sequence of [`OuterTerminal`] calls.
    #[derive(Default)]
    pub struct MockTerminal {
        pub calls: Vec<Call>,
        /// Tracks whether [`OuterTerminal::enable_mouse`] was called, so the mock
        /// disables only when capture was enabled — mirroring the real rule.
        mouse_enabled: bool,
        /// An optional shared order log the mock suspender also writes to, so the
        /// suspend cycle's terminal calls and the SuspendSelf/ContinueChild markers
        /// land in one interleaved sequence (ADR-0019). Every recorded call is
        /// pushed here too when present.
        log: Option<std::rc::Rc<std::cell::RefCell<Vec<Call>>>>,
        /// The `(cols, rows)` [`OuterTerminal::terminal_size`] reports, behind a
        /// shared cell so a test (or the mock suspender's on-suspend hook) can flip
        /// it mid-cycle to model a resize while gutter was suspended (ADR-0019).
        size: std::rc::Rc<std::cell::Cell<(u16, u16)>>,
        /// Call variants that return an error, so a test can drive a restore step
        /// that fails against a terminal that has gone away.
        failing: Vec<std::mem::Discriminant<Call>>,
    }

    impl MockTerminal {
        pub fn new() -> Self {
            Self::default()
        }

        /// A mock sharing `log` with a `MockSuspender::new(log)`, so both mocks
        /// record into one interleaved sequence.
        pub fn with_log(log: std::rc::Rc<std::cell::RefCell<Vec<Call>>>) -> Self {
            Self {
                log: Some(log),
                ..Self::default()
            }
        }

        /// The `(cols, rows)` this mock reports from `terminal_size`, and the shared
        /// cell backing it — hand the cell to a `MockSuspender` so its on-suspend
        /// hook can flip the size mid-cycle.
        pub fn set_terminal_size(&self, cols: u16, rows: u16) {
            self.size.set((cols, rows));
        }
        pub fn size_cell(&self) -> std::rc::Rc<std::cell::Cell<(u16, u16)>> {
            self.size.clone()
        }

        /// Make the given call fail, matched on its variant alone — any argument will
        /// do, so `fail_on(Call::WriteRow(vec![]))` fails every row write. The call is
        /// still recorded: it was invoked, it just did not succeed.
        pub fn fail_on(&mut self, call: Call) {
            self.failing.push(std::mem::discriminant(&call));
        }

        /// Record one call: into `calls`, and into the shared order log if one is
        /// attached. The single choke point every `OuterTerminal` method funnels
        /// through, so nothing bypasses the interleaved log — or the failure list.
        fn record(&mut self, c: Call) -> io::Result<()> {
            let fails = self.failing.contains(&std::mem::discriminant(&c));
            let named = fails.then(|| format!("{c:?} failed"));
            if let Some(log) = &self.log {
                log.borrow_mut().push(c.clone());
            }
            self.calls.push(c);
            match named {
                Some(msg) => Err(io::Error::other(msg)),
                None => Ok(()),
            }
        }

        /// The restore subsequence only, for the ADR-010 order assertion —
        /// filters out any render-output noise a frame emitted first.
        ///
        /// The attribute and cursor-shape resets are matched on their exact bytes, not
        /// on their call variant: `write_row` and `set_cursor_shape` are how the render
        /// path paints rows and mirrors the child's shape too, and a variant match would
        /// pull every frame's output into the order assertion.
        pub fn restore_calls(&self) -> Vec<Call> {
            self.calls
                .iter()
                .filter(|c| match c {
                    Call::LeaveAltScreen
                    | Call::Relay(_)
                    | Call::DisableMouse
                    | Call::ShowCursor
                    | Call::DisableRawMode => true,
                    Call::WriteRow(b) => b.as_slice() == crate::render::SGR_RESET,
                    Call::SetCursorShape(b) => {
                        b.as_slice() == crate::render::DEFAULT_CURSOR_SHAPE
                    }
                    _ => false,
                })
                .cloned()
                .collect()
        }
    }

    /// A physical-cell-readback [`OuterTerminal`]. Where [`MockTerminal`] records
    /// the sequence of calls, this paints gutter's own `move_to`/`write_row` bytes
    /// through a `vt100` parser sized to the **real** outer terminal
    /// (`phys_cols × rows`). Tests then read back cell `(row, margin + W)` and
    /// assert it is blank — the "no bleed past the band" check that a `COLUMNS ==
    /// W` child grid structurally cannot make, because the corruption lives in the
    /// physical outer cells (ADR-006).
    ///
    /// Render-output calls are translated to escape bytes and fed to the parser,
    /// just as `CrosstermTerminal` emits them. Lifecycle and teardown calls are
    /// no-ops.
    pub struct RecordingGrid {
        parser: vt100::Parser,
    }

    impl RecordingGrid {
        /// A recording grid sized to the **physical** outer terminal.
        pub fn new(phys_cols: u16, rows: u16) -> Self {
            Self {
                parser: vt100::Parser::new(rows, phys_cols, 0),
            }
        }

        /// A recording grid with bounded scrollback (ADR-013) — models the real
        /// terminal's own scrollback store, so scroll-off survival assertions can
        /// read back the departed lines the `\r\n`s scrolled in. The default `new`
        /// keeps `scrollback=0`, enough for the edge-of-band readback that needs no
        /// history.
        pub fn with_scrollback(phys_cols: u16, rows: u16, scrollback: usize) -> Self {
            Self {
                parser: vt100::Parser::new(rows, phys_cols, scrollback),
            }
        }

        /// Feed a CUP to the physical parser. vt100 is 1-based, gutter's API 0-based.
        fn goto(&mut self, col: u16, row: u16) {
            self.parser
                .process(format!("\x1b[{};{}H", row + 1, col + 1).as_bytes());
        }

        /// The trimmed contents of physical cell `(row, col)` — `""` when blank.
        /// The edge-of-band assertion reads `(row, margin + W)` and expects `""`.
        pub fn cell_contents(&self, row: u16, col: u16) -> String {
            self.parser
                .screen()
                .cell(row, col)
                .map(|c| c.contents().to_string())
                .unwrap_or_default()
        }

        /// Whether physical cell `(row, col)` carries reverse-video. A
        /// background-only flood (`ESC[K` under reverse video) erases the cell —
        /// `contents()` stays `""` while the background bleeds. The band-edge
        /// assertion reads this to catch a statusline highlight spilling past the
        /// band, which a content-only readback cannot see.
        pub fn cell_inverse(&self, row: u16, col: u16) -> bool {
            self.parser
                .screen()
                .cell(row, col)
                .map(|c| c.inverse())
                .unwrap_or(false)
        }

        /// Whether physical cell `(row, col)` carries the faint attribute — lets the
        /// rails test assert the glyph landed AND that it is dim.
        pub fn cell_dim(&self, row: u16, col: u16) -> bool {
            self.parser
                .screen()
                .cell(row, col)
                .map(|c| c.dim())
                .unwrap_or(false)
        }

        /// The visible band rows `[0, rows)`, each trimmed to the band columns
        /// `[margin, margin + width)`. Used to assert what stayed on screen.
        pub fn visible_band(&self, margin: u16, width: u16, rows: u16) -> Vec<String> {
            (0..rows)
                .map(|r| {
                    let mut s = String::new();
                    for c in margin..margin + width {
                        s.push_str(&self.cell_contents(r, c));
                    }
                    s.trim_end().to_string()
                })
                .collect()
        }

        /// The outer terminal's own input modes, as the relayed bytes left them:
        /// `(application_cursor, application_keypad, bracketed_paste)` (ADR-022).
        pub fn outer_input_modes(&self) -> (bool, bool, bool) {
            let s = self.parser.screen();
            (
                s.application_cursor(),
                s.application_keypad(),
                s.bracketed_paste(),
            )
        }

        /// The lines in the recorder's scrollback, oldest first, each read across
        /// the band columns `[0, width)` (the tests paint a margin-0 band).
        pub fn scrollback_top_rows(&mut self, width: u16) -> Vec<String> {
            // Probe the filled scrollback length by clamping the offset.
            self.parser.screen_mut().set_scrollback(usize::MAX);
            let n = self.parser.screen().scrollback();
            let mut out = Vec::with_capacity(n);
            for offset in (1..=n).rev() {
                self.parser.screen_mut().set_scrollback(offset);
                let mut s = String::new();
                for c in 0..width {
                    s.push_str(&self.cell_contents(0, c));
                }
                out.push(s.trim_end().to_string());
            }
            self.parser.screen_mut().set_scrollback(0);
            out
        }
    }

    impl OuterTerminal for RecordingGrid {
        fn enable_raw_mode(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn enable_mouse(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn enter_alt_screen(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn terminal_size(&mut self) -> io::Result<(u16, u16)> {
            let (rows, cols) = self.parser.screen().size();
            Ok((cols, rows))
        }
        fn move_to(&mut self, col: u16, row: u16) -> io::Result<()> {
            self.goto(col, row);
            Ok(())
        }
        fn write_row(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.parser.process(bytes);
            Ok(())
        }
        fn newline(&mut self) -> io::Result<()> {
            // Feed `\r\n` through the physical parser, so a departed line scrolls
            // into the recording grid. vt100 fills the scrolled-in line with blanks
            // whatever the SGR, so the trait's reset is invisible here — see the
            // painted-band check for the emulator that shows it.
            self.parser.process(b"\x1b[m\r\n");
            Ok(())
        }
        fn clear_gutter(
            &mut self,
            margin: u16,
            width: u16,
            real_cols: u16,
            row_start: u16,
            row_end: u16,
        ) -> io::Result<()> {
            // Paint blanks over the physical gutter columns, as the real terminal
            // would, so the readback sees them cleared.
            let band_end = margin.saturating_add(width).min(real_cols);
            self.parser.process(b"\x1b[0m");
            for row in row_start..row_end {
                if margin > 0 {
                    self.goto(0, row);
                    self.parser.process(&b" ".repeat(margin as usize));
                }
                if real_cols > band_end {
                    self.goto(band_end, row);
                    self.parser.process(&b" ".repeat((real_cols - band_end) as usize));
                }
            }
            Ok(())
        }
        fn clear_row_span(&mut self, row_start: u16, row_end: u16) -> io::Result<()> {
            // Blank whole physical rows via ESC[2K, as the real terminal would, so the
            // readback sees the band interior cleared.
            self.parser.process(b"\x1b[0m");
            for row in row_start..row_end {
                self.goto(0, row);
                self.parser.process(b"\x1b[2K");
            }
            Ok(())
        }
        fn draw_rails(&mut self, rails: &super::Rails) -> io::Result<()> {
            // Replay the same escapes CrosstermTerminal emits, into the physical
            // parser, so cell readback works.
            self.parser.process(b"\x1b[0m");
            for row in rails.row_start..rails.row_end {
                if let Some(c) = rails.left_col {
                    self.goto(c, row);
                    self.parser.process("\x1b[2m\u{258f}\x1b[0m".as_bytes());
                }
                if let Some(c) = rails.right_col {
                    self.goto(c, row);
                    self.parser.process("\x1b[2m\u{2595}\x1b[0m".as_bytes());
                }
            }
            if let Some(r) = &rails.readout {
                self.goto(r.col, r.row);
                self.parser
                    .process(format!("\x1b[2m{}\x1b[0m", r.text).as_bytes());
            }
            Ok(())
        }
        fn place_cursor(&mut self, col: u16, row: u16) -> io::Result<()> {
            self.goto(col, row);
            Ok(())
        }
        fn set_cursor_visible(&mut self, visible: bool) -> io::Result<()> {
            self.parser
                .process(if visible { b"\x1b[?25h" } else { b"\x1b[?25l" });
            Ok(())
        }
        fn set_cursor_shape(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.parser.process(bytes);
            Ok(())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn relay(&mut self, bytes: &[u8]) -> io::Result<()> {
            // Relayed bytes paint nothing — that is the premise of both the keyboard
            // allowlist (ADR-021) and the mode mirror (ADR-022) — but feeding them to
            // the parser lets a test read back the outer terminal's resulting input
            // modes instead of matching on byte spelling.
            self.parser.process(bytes);
            Ok(())
        }
        fn leave_alt_screen(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn disable_mouse(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn show_cursor(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn disable_raw_mode(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl OuterTerminal for MockTerminal {
        fn enable_raw_mode(&mut self) -> io::Result<()> {
            self.record(Call::EnableRawMode)
        }
        fn enable_mouse(&mut self) -> io::Result<()> {
            // Mirror the real impl: a failed enable leaves nothing to disable.
            self.record(Call::EnableMouse)?;
            self.mouse_enabled = true;
            Ok(())
        }
        fn enter_alt_screen(&mut self) -> io::Result<()> {
            self.record(Call::EnterAltScreen)
        }
        fn terminal_size(&mut self) -> io::Result<(u16, u16)> {
            Ok(self.size.get())
        }
        fn move_to(&mut self, col: u16, row: u16) -> io::Result<()> {
            self.record(Call::MoveTo(col, row))
        }
        fn write_row(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.record(Call::WriteRow(bytes.to_vec()))
        }
        fn newline(&mut self) -> io::Result<()> {
            self.record(Call::Newline)
        }
        fn clear_gutter(
            &mut self,
            margin: u16,
            width: u16,
            real_cols: u16,
            row_start: u16,
            row_end: u16,
        ) -> io::Result<()> {
            self.record(Call::ClearGutter(margin, width, real_cols, row_start, row_end))
        }
        fn clear_row_span(&mut self, row_start: u16, row_end: u16) -> io::Result<()> {
            self.record(Call::ClearRowSpan(row_start, row_end))
        }
        fn draw_rails(&mut self, rails: &super::Rails) -> io::Result<()> {
            self.record(Call::DrawRails(rails.clone()))
        }
        fn place_cursor(&mut self, col: u16, row: u16) -> io::Result<()> {
            self.record(Call::PlaceCursor(col, row))
        }
        fn set_cursor_visible(&mut self, visible: bool) -> io::Result<()> {
            self.record(Call::SetCursorVisible(visible))
        }
        fn set_cursor_shape(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.record(Call::SetCursorShape(bytes.to_vec()))
        }
        fn flush(&mut self) -> io::Result<()> {
            self.record(Call::Flush)
        }
        fn relay(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.record(Call::Relay(bytes.to_vec()))
        }
        fn leave_alt_screen(&mut self) -> io::Result<()> {
            self.record(Call::LeaveAltScreen)
        }
        fn disable_mouse(&mut self) -> io::Result<()> {
            // Mirror the real impl: only disable what was actually enabled, and a
            // failed disable leaves the mouse still enabled — the real one's `?`
            // returns before it clears the flag, so a later restore retries. Clearing
            // it here regardless would let a regression that never re-disables pass.
            if !self.mouse_enabled {
                return Ok(());
            }
            self.record(Call::DisableMouse)?;
            self.mouse_enabled = false;
            Ok(())
        }
        fn show_cursor(&mut self) -> io::Result<()> {
            self.record(Call::ShowCursor)
        }
        fn disable_raw_mode(&mut self) -> io::Result<()> {
            self.record(Call::DisableRawMode)
        }
    }
}
