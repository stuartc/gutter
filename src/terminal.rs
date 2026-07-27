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

use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
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
    /// Write a row's `rows_diff` byte run verbatim — `prepare_row_into` has made it
    /// self-contained, so it carries its own intra-row SGR and relative cursor
    /// moves, scoped to `[0, W)` (ADR-014).
    fn write_row(&mut self, bytes: &[u8]) -> io::Result<()>;
    /// Advance the real terminal one line (`\r\n`), scrolling when on the bottom
    /// row. The scroll-aware primary paint emits a departed top line then this
    /// newline so the line enters the real terminal's own scrollback — gutter
    /// keeps vt100 at `scrollback=0` and lets the real terminal be the store
    /// (ADR-013).
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
/// `TIOCSCTTY` — so the terminal stdin names is the second route, reopened **by name**:
/// `dup`ing descriptor 0 would hand back the caller's file description, where the
/// clipboard (ADR-004) and the probe/Thread-3 split both need independent ones.
///
/// The open doubles as gutter's terminal guard: both routes failing means there is no
/// screen to render a band on. The error reported is `/dev/tty`'s, the usual cause.
pub fn open_tty_write() -> io::Result<(File, PathBuf)> {
    let dev_tty = Path::new("/dev/tty");
    match open_write(dev_tty) {
        Ok(f) => Ok((f, dev_tty.to_path_buf())),
        Err(e) => {
            let Some(path) = stdin_tty_path() else {
                return Err(e);
            };
            match open_write(&path) {
                Ok(f) => Ok((f, path)),
                Err(_) => Err(e),
            }
        }
    }
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

/// The path of the terminal on descriptor 0, if there is one.
fn stdin_tty_path() -> Option<PathBuf> {
    let mut buf = [0u8; libc::PATH_MAX as usize];
    // SAFETY: `ttyname_r` writes at most `buf.len()` bytes into the buffer it is
    // given, and STDIN_FILENO is always a valid descriptor number to ask about.
    let rc = unsafe { libc::ttyname_r(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0)?;
    (end > 0).then(|| PathBuf::from(OsStr::from_bytes(&buf[..end])))
}

/// The real outer terminal, backed by crossterm against the controlling terminal.
pub struct CrosstermTerminal {
    /// Buffered: one `move_to` is several small writes, and unbuffered they would
    /// be several syscalls. Every write here is landed by an explicit flush — no
    /// destructor runs before `process::exit` (ADR-010).
    out: BufWriter<File>,
    /// Whether mouse capture was enabled at startup, so teardown disables only
    /// what it set.
    mouse_enabled: bool,
}

impl CrosstermTerminal {
    pub fn new(tty: File) -> Self {
        Self {
            out: BufWriter::new(tty),
            mouse_enabled: false,
        }
    }

    /// The band's sink, for the startup CPR probe: its query has to leave by the
    /// same terminal the reply comes back from.
    pub fn writer(&mut self) -> &mut impl Write {
        &mut self.out
    }
}

impl OuterTerminal for CrosstermTerminal {
    fn enable_raw_mode(&mut self) -> io::Result<()> {
        crossterm::terminal::enable_raw_mode()
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
        crossterm::terminal::size()
    }

    fn move_to(&mut self, col: u16, row: u16) -> io::Result<()> {
        queue!(self.out, MoveTo(col, row))
    }

    fn write_row(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.out.write_all(bytes)
    }

    fn newline(&mut self) -> io::Result<()> {
        // `\r\n` returns to column 0 then advances a line; on the bottom row the
        // terminal scrolls and the line just written enters its scrollback.
        self.out.write_all(b"\r\n")
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

    fn disable_raw_mode(&mut self) -> io::Result<()> {
        crossterm::terminal::disable_raw_mode()
    }
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

        /// Record one call: into `calls`, and into the shared order log if one is
        /// attached. The single choke point every `OuterTerminal` method funnels
        /// through, so nothing bypasses the interleaved log.
        fn record(&mut self, c: Call) {
            if let Some(log) = &self.log {
                log.borrow_mut().push(c.clone());
            }
            self.calls.push(c);
        }

        /// The restore subsequence only, for the ADR-010 order assertion —
        /// filters out any render-output noise a frame emitted first.
        pub fn restore_calls(&self) -> Vec<Call> {
            self.calls
                .iter()
                .filter(|c| {
                    matches!(
                        c,
                        Call::LeaveAltScreen
                            | Call::Relay(_)
                            | Call::DisableMouse
                            | Call::ShowCursor
                            | Call::DisableRawMode
                    )
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
            // into the recording grid.
            self.parser.process(b"\r\n");
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
            self.record(Call::EnableRawMode);
            Ok(())
        }
        fn enable_mouse(&mut self) -> io::Result<()> {
            self.record(Call::EnableMouse);
            self.mouse_enabled = true;
            Ok(())
        }
        fn enter_alt_screen(&mut self) -> io::Result<()> {
            self.record(Call::EnterAltScreen);
            Ok(())
        }
        fn terminal_size(&mut self) -> io::Result<(u16, u16)> {
            Ok(self.size.get())
        }
        fn move_to(&mut self, col: u16, row: u16) -> io::Result<()> {
            self.record(Call::MoveTo(col, row));
            Ok(())
        }
        fn write_row(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.record(Call::WriteRow(bytes.to_vec()));
            Ok(())
        }
        fn newline(&mut self) -> io::Result<()> {
            self.record(Call::Newline);
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
            self.record(Call::ClearGutter(margin, width, real_cols, row_start, row_end));
            Ok(())
        }
        fn clear_row_span(&mut self, row_start: u16, row_end: u16) -> io::Result<()> {
            self.record(Call::ClearRowSpan(row_start, row_end));
            Ok(())
        }
        fn draw_rails(&mut self, rails: &super::Rails) -> io::Result<()> {
            self.record(Call::DrawRails(rails.clone()));
            Ok(())
        }
        fn place_cursor(&mut self, col: u16, row: u16) -> io::Result<()> {
            self.record(Call::PlaceCursor(col, row));
            Ok(())
        }
        fn set_cursor_visible(&mut self, visible: bool) -> io::Result<()> {
            self.record(Call::SetCursorVisible(visible));
            Ok(())
        }
        fn set_cursor_shape(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.record(Call::SetCursorShape(bytes.to_vec()));
            Ok(())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.record(Call::Flush);
            Ok(())
        }
        fn relay(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.record(Call::Relay(bytes.to_vec()));
            Ok(())
        }
        fn leave_alt_screen(&mut self) -> io::Result<()> {
            self.record(Call::LeaveAltScreen);
            Ok(())
        }
        fn disable_mouse(&mut self) -> io::Result<()> {
            // Mirror the real impl: only disable what was actually enabled.
            if self.mouse_enabled {
                self.record(Call::DisableMouse);
                self.mouse_enabled = false;
            }
            Ok(())
        }
        fn show_cursor(&mut self) -> io::Result<()> {
            self.record(Call::ShowCursor);
            Ok(())
        }
        fn disable_raw_mode(&mut self) -> io::Result<()> {
            self.record(Call::DisableRawMode);
            Ok(())
        }
    }
}
