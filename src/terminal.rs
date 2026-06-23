//! Outer-terminal handle (Thread 2 only): lifecycle + the render output sink.
//!
//! Two responsibilities behind one injectable trait:
//!
//! 1. **Lifecycle** — raw mode, alt screen, and the explicit ordered restore
//!    (leave alt screen → pop kitty flags → disable mouse → show cursor →
//!    disable raw mode) run BEFORE `process::exit` (ADR-010). `process::exit`
//!    runs no destructors, so teardown cannot be a `Drop` guard.
//! 2. **Render output** — the offset repaint paints through this handle:
//!    `move_to(col, row)` positions a row at its physical left margin,
//!    `write_row(bytes)` emits that row's `rows_diff` byte run, `place_cursor`
//!    repositions the real cursor inside the band, and `set_cursor_visible`
//!    mirrors the child's DECTCEM state. Keeping output behind the trait lets
//!    the render path be unit-tested against a recording mock — physical-cell
//!    assertions read back what was painted to which column.
//!
//! In slice 02 `pop_keyboard_flags` and `disable_mouse` are inert in the real
//! impl (kitty is slice 04, mouse is slice 07) but stay in the restore sequence
//! so those slices drop in without re-sequencing.

use std::io::{self, Write};

/// The outer-terminal side effects the setup, render and teardown paths perform.
///
/// The real impl ([`CrosstermTerminal`]) wraps crossterm against stdout; the
/// test mock ([`mock::MockTerminal`]) records the ordered sequence of calls so
/// the ADR-010 restore-order test and the offset-repaint test can assert.
pub trait OuterTerminal {
    // --- Setup ---
    /// Enter raw mode. Setup; first thing after the PTY is up.
    fn enable_raw_mode(&mut self) -> io::Result<()>;
    /// Enter the alternate screen. Setup; after raw mode.
    fn enter_alt_screen(&mut self) -> io::Result<()>;

    // --- Render output (per frame) ---
    /// Move the cursor to physical `(col, row)`. Emitted by gutter before each
    /// repainted row so the row's bytes land at the band's left margin.
    fn move_to(&mut self, col: u16, row: u16) -> io::Result<()>;
    /// Write a row's `rows_diff` byte run verbatim (it carries its own intra-row
    /// SGR and relative cursor moves, scoped to `[0, W)`).
    fn write_row(&mut self, bytes: &[u8]) -> io::Result<()>;
    /// Reposition the real cursor inside the band at physical `(col, row)` after
    /// the repaint, from the child's `screen.cursor_position()`.
    fn place_cursor(&mut self, col: u16, row: u16) -> io::Result<()>;
    /// Mirror the child's cursor visibility (DECTCEM / `CSI ?25l`).
    fn set_cursor_visible(&mut self, visible: bool) -> io::Result<()>;
    /// Flush the queued frame to the real terminal. Exactly once per frame.
    fn flush(&mut self) -> io::Result<()>;

    // --- Teardown (ADR-010 order) ---
    /// Leave the alternate screen. Teardown step 1.
    fn leave_alt_screen(&mut self) -> io::Result<()>;
    /// Pop kitty keyboard enhancement flags. Teardown step 2. No-op until
    /// slice 04 pushes them.
    fn pop_keyboard_flags(&mut self) -> io::Result<()>;
    /// Disable mouse capture. Teardown step 3. No-op until slice 07 enables it.
    fn disable_mouse(&mut self) -> io::Result<()>;
    /// Show the cursor. Teardown step 4.
    fn show_cursor(&mut self) -> io::Result<()>;
    /// Disable raw mode. Teardown step 5 — must run **after** leaving the alt
    /// screen, or control sequences leak to the user's shell.
    fn disable_raw_mode(&mut self) -> io::Result<()>;
}

/// The real outer terminal, backed by crossterm against stdout.
pub struct CrosstermTerminal {
    out: io::Stdout,
}

impl CrosstermTerminal {
    pub fn new() -> Self {
        Self { out: io::stdout() }
    }
}

impl Default for CrosstermTerminal {
    fn default() -> Self {
        Self::new()
    }
}

impl OuterTerminal for CrosstermTerminal {
    fn enable_raw_mode(&mut self) -> io::Result<()> {
        crossterm::terminal::enable_raw_mode()
    }

    fn enter_alt_screen(&mut self) -> io::Result<()> {
        use crossterm::{queue, terminal::EnterAlternateScreen};
        queue!(self.out, EnterAlternateScreen)?;
        self.out.flush()
    }

    fn move_to(&mut self, col: u16, row: u16) -> io::Result<()> {
        use crossterm::{cursor::MoveTo, queue};
        queue!(self.out, MoveTo(col, row))
    }

    fn write_row(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.out.write_all(bytes)
    }

    fn place_cursor(&mut self, col: u16, row: u16) -> io::Result<()> {
        use crossterm::{cursor::MoveTo, queue};
        queue!(self.out, MoveTo(col, row))
    }

    fn set_cursor_visible(&mut self, visible: bool) -> io::Result<()> {
        use crossterm::{
            cursor::{Hide, Show},
            queue,
        };
        if visible {
            queue!(self.out, Show)
        } else {
            queue!(self.out, Hide)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }

    fn leave_alt_screen(&mut self) -> io::Result<()> {
        use crossterm::{queue, terminal::LeaveAlternateScreen};
        queue!(self.out, LeaveAlternateScreen)?;
        self.out.flush()
    }

    fn pop_keyboard_flags(&mut self) -> io::Result<()> {
        // No-op in slice 02 — nothing was pushed. Slice 04 pops the kitty
        // enhancement flags here.
        Ok(())
    }

    fn disable_mouse(&mut self) -> io::Result<()> {
        // No-op in slice 02 — mouse capture is never enabled. Slice 07
        // disables capture here.
        Ok(())
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        use crossterm::{cursor::Show, queue};
        queue!(self.out, Show)?;
        self.out.flush()
    }

    fn disable_raw_mode(&mut self) -> io::Result<()> {
        crossterm::terminal::disable_raw_mode()
    }
}

#[cfg(test)]
pub mod mock {
    //! A recording [`OuterTerminal`] for the ADR-010 restore-order test and the
    //! offset-repaint unit test.

    use super::OuterTerminal;
    use std::io;

    /// One recorded side effect, in the order it was invoked. Render-output
    /// calls carry their arguments so physical-column assertions can read back
    /// what was painted where.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Call {
        EnableRawMode,
        EnterAltScreen,
        MoveTo(u16, u16),
        WriteRow(Vec<u8>),
        PlaceCursor(u16, u16),
        SetCursorVisible(bool),
        Flush,
        LeaveAltScreen,
        PopKeyboardFlags,
        DisableMouse,
        ShowCursor,
        DisableRawMode,
    }

    /// Records the ordered sequence of [`OuterTerminal`] calls.
    #[derive(Default)]
    pub struct MockTerminal {
        pub calls: Vec<Call>,
    }

    impl MockTerminal {
        pub fn new() -> Self {
            Self::default()
        }

        /// The restore subsequence only, for the ADR-010 order assertion —
        /// filters out the render-output noise a frame may have emitted first.
        pub fn restore_calls(&self) -> Vec<Call> {
            self.calls
                .iter()
                .filter(|c| {
                    matches!(
                        c,
                        Call::LeaveAltScreen
                            | Call::PopKeyboardFlags
                            | Call::DisableMouse
                            | Call::ShowCursor
                            | Call::DisableRawMode
                    )
                })
                .cloned()
                .collect()
        }
    }

    impl OuterTerminal for MockTerminal {
        fn enable_raw_mode(&mut self) -> io::Result<()> {
            self.calls.push(Call::EnableRawMode);
            Ok(())
        }
        fn enter_alt_screen(&mut self) -> io::Result<()> {
            self.calls.push(Call::EnterAltScreen);
            Ok(())
        }
        fn move_to(&mut self, col: u16, row: u16) -> io::Result<()> {
            self.calls.push(Call::MoveTo(col, row));
            Ok(())
        }
        fn write_row(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.calls.push(Call::WriteRow(bytes.to_vec()));
            Ok(())
        }
        fn place_cursor(&mut self, col: u16, row: u16) -> io::Result<()> {
            self.calls.push(Call::PlaceCursor(col, row));
            Ok(())
        }
        fn set_cursor_visible(&mut self, visible: bool) -> io::Result<()> {
            self.calls.push(Call::SetCursorVisible(visible));
            Ok(())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.calls.push(Call::Flush);
            Ok(())
        }
        fn leave_alt_screen(&mut self) -> io::Result<()> {
            self.calls.push(Call::LeaveAltScreen);
            Ok(())
        }
        fn pop_keyboard_flags(&mut self) -> io::Result<()> {
            self.calls.push(Call::PopKeyboardFlags);
            Ok(())
        }
        fn disable_mouse(&mut self) -> io::Result<()> {
            self.calls.push(Call::DisableMouse);
            Ok(())
        }
        fn show_cursor(&mut self) -> io::Result<()> {
            self.calls.push(Call::ShowCursor);
            Ok(())
        }
        fn disable_raw_mode(&mut self) -> io::Result<()> {
            self.calls.push(Call::DisableRawMode);
            Ok(())
        }
    }
}
