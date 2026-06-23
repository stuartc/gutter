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
//! Setup mirrors teardown: the eager startup `enable_mouse`
//! (`EnableMouseCapture`, ADR-005) and the kitty push are paired with
//! `disable_mouse`/`pop_keyboard_flags` in the restore — each undoes only what it
//! actually set.

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
    /// Probe whether the outer terminal supports the kitty keyboard protocol —
    /// the inbound `CSI ? u` grant `vt100` cannot observe (ADR-003). Called once
    /// at startup, after raw mode, before spawning the child. Wraps crossterm's
    /// `supports_keyboard_enhancement()`. The returned bool is the
    /// `outer_supports` clamp fed to the child's kitty state and the encoder.
    fn supports_keyboard_enhancement(&mut self) -> io::Result<bool>;
    /// Push the kitty enhancement flags (`DISAMBIGUATE_ESCAPE_CODES |
    /// REPORT_EVENT_TYPES`) onto the outer terminal so crossterm thereafter
    /// delivers `KeyEvent`s that distinguish Shift+Enter from Enter (ADR-003).
    /// Called at startup **only when** the probe returned `true`; paired with
    /// [`pop_keyboard_flags`] in teardown.
    ///
    /// [`pop_keyboard_flags`]: OuterTerminal::pop_keyboard_flags
    fn push_keyboard_flags(&mut self) -> io::Result<()>;
    /// Enable mouse capture eagerly (ADR-005): crossterm emits the fixed bundle
    /// `?1000h ?1002h ?1003h ?1015h ?1006h` (any-motion SGR reporting). Called
    /// once at startup, after raw mode and the kitty push, before the alt screen.
    /// Paired with [`disable_mouse`] in teardown — set once, never tracking the
    /// child's mode (the forwarding gate narrows in software).
    ///
    /// [`disable_mouse`]: OuterTerminal::disable_mouse
    fn enable_mouse(&mut self) -> io::Result<()>;
    /// Enter the alternate screen. Setup; after raw mode.
    fn enter_alt_screen(&mut self) -> io::Result<()>;

    // --- Render output (per frame) ---
    /// Move the cursor to physical `(col, row)`. Emitted by gutter before each
    /// repainted row so the row's bytes land at the band's left margin.
    fn move_to(&mut self, col: u16, row: u16) -> io::Result<()>;
    /// Write a row's `rows_diff` byte run verbatim (it carries its own intra-row
    /// SGR and relative cursor moves, scoped to `[0, W)`).
    fn write_row(&mut self, bytes: &[u8]) -> io::Result<()>;
    /// Clear the gutter columns — everything outside the band `[margin,
    /// margin + width)` across every physical row `[0, rows)` of a `real_cols`-wide
    /// terminal. Called on resize (ADR-008 step 4): a shrink that moved the margin
    /// leftward, or a centred→narrower transition, can strand painted cells where
    /// the gutter now is, and the `rows_diff` repaint only touches `[margin,
    /// margin + width)` — so the cells outside it must be cleared explicitly.
    fn clear_gutter(
        &mut self,
        margin: u16,
        width: u16,
        real_cols: u16,
        rows: u16,
    ) -> io::Result<()>;
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
    /// Disable mouse capture. Teardown step 3 — after the kitty pop, before the
    /// cursor show (ADR-010). Pairs with [`enable_mouse`]; runs via the explicit
    /// restore (not a `Drop` guard — `process::exit` skips destructors, which
    /// would leave the shell emitting mouse escapes after gutter dies).
    ///
    /// [`enable_mouse`]: OuterTerminal::enable_mouse
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
    /// Whether kitty enhancement flags were pushed at startup — so teardown only
    /// pops what it actually set (ADR-003: don't pop flags you never pushed).
    kitty_pushed: bool,
    /// Whether mouse capture was enabled at startup — so teardown only disables
    /// what it actually enabled (symmetry with the kitty pop).
    mouse_enabled: bool,
}

impl CrosstermTerminal {
    pub fn new() -> Self {
        Self {
            out: io::stdout(),
            kitty_pushed: false,
            mouse_enabled: false,
        }
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

    fn supports_keyboard_enhancement(&mut self) -> io::Result<bool> {
        crossterm::terminal::supports_keyboard_enhancement()
    }

    fn push_keyboard_flags(&mut self) -> io::Result<()> {
        use crossterm::event::{KeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
        use crossterm::queue;
        queue!(
            self.out,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
            )
        )?;
        self.out.flush()?;
        self.kitty_pushed = true;
        Ok(())
    }

    fn enable_mouse(&mut self) -> io::Result<()> {
        use crossterm::{event::EnableMouseCapture, queue};
        queue!(self.out, EnableMouseCapture)?;
        self.out.flush()?;
        self.mouse_enabled = true;
        Ok(())
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

    fn clear_gutter(
        &mut self,
        margin: u16,
        width: u16,
        real_cols: u16,
        rows: u16,
    ) -> io::Result<()> {
        use crossterm::{cursor::MoveTo, queue};
        let band_end = margin.saturating_add(width).min(real_cols);
        // Reset SGR first so the blanks are painted with the default background
        // (a leftover colour run would tint the gutter).
        self.out.write_all(b"\x1b[0m")?;
        for row in 0..rows {
            // Left gutter: physical columns [0, margin).
            if margin > 0 {
                queue!(self.out, MoveTo(0, row))?;
                self.out.write_all(&b" ".repeat(margin as usize))?;
            }
            // Right gutter: physical columns [band_end, real_cols).
            if real_cols > band_end {
                queue!(self.out, MoveTo(band_end, row))?;
                self.out
                    .write_all(&b" ".repeat((real_cols - band_end) as usize))?;
            }
        }
        Ok(())
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
        // Only pop what was actually pushed (ADR-003) — popping flags we never
        // set would corrupt an unrelated terminal state.
        if self.kitty_pushed {
            use crossterm::event::PopKeyboardEnhancementFlags;
            use crossterm::queue;
            queue!(self.out, PopKeyboardEnhancementFlags)?;
            self.out.flush()?;
            self.kitty_pushed = false;
        }
        Ok(())
    }

    fn disable_mouse(&mut self) -> io::Result<()> {
        // Only disable what was actually enabled (symmetry with the kitty pop):
        // emitting `DisableMouseCapture` when we never captured would still be
        // harmless, but mirroring the push/pop rule keeps the contract clean.
        if self.mouse_enabled {
            use crossterm::{event::DisableMouseCapture, queue};
            queue!(self.out, DisableMouseCapture)?;
            self.out.flush()?;
            self.mouse_enabled = false;
        }
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
        SupportsKeyboardEnhancement,
        PushKeyboardFlags,
        EnableMouse,
        EnterAltScreen,
        MoveTo(u16, u16),
        WriteRow(Vec<u8>),
        /// `clear_gutter(margin, width, real_cols, rows)`.
        ClearGutter(u16, u16, u16, u16),
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
        /// What the kitty-capability probe should report. Lets the teardown test
        /// drive both the kitty-capable (push then pop) and non-kitty (neither)
        /// paths without a real terminal.
        pub supports_kitty: bool,
        /// Whether [`OuterTerminal::push_keyboard_flags`] was actually called —
        /// so the mock's `pop` only records a [`Call::PopKeyboardFlags`] when
        /// flags were pushed, mirroring the real "pop only what you pushed" rule.
        kitty_pushed: bool,
        /// Whether [`OuterTerminal::enable_mouse`] was actually called — so the
        /// mock's `disable_mouse` only records a [`Call::DisableMouse`] when
        /// capture was enabled, mirroring the real "disable only what you enabled".
        mouse_enabled: bool,
    }

    impl MockTerminal {
        pub fn new() -> Self {
            Self::default()
        }

        /// A mock that reports the outer terminal as kitty-capable.
        pub fn kitty_capable() -> Self {
            Self {
                supports_kitty: true,
                ..Self::default()
            }
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

    /// A physical-cell-readback [`OuterTerminal`] (the ADR-006 seam). Unlike
    /// [`MockTerminal`], which only records the *sequence* of calls, this paints
    /// into an in-memory grid the size of the **real** outer terminal
    /// (`phys_cols × rows`) by replaying gutter's own `move_to` + `write_row`
    /// bytes through a `vt100` parser at that physical width. After a render the
    /// test reads back cell `(row, margin + W)` and asserts it is blank — the
    /// "no bleed past the band" assertion that `COLUMNS == W` can never make,
    /// because the corruption lives in the physical outer cells, not the child
    /// grid (ADR-006).
    ///
    /// Render-output calls (`move_to`/`write_row`/`place_cursor`/visibility) are
    /// translated to the equivalent escape bytes and fed to the parser, exactly
    /// as the real `CrosstermTerminal` would emit them to stdout. Lifecycle and
    /// teardown calls are no-ops here (the ordered-restore assertion is
    /// [`MockTerminal`]'s job).
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

        /// The trimmed contents of physical cell `(row, col)` — `""` when blank.
        /// The edge-of-band assertion reads `(row, margin + W)` and expects `""`.
        pub fn cell_contents(&self, row: u16, col: u16) -> String {
            self.parser
                .screen()
                .cell(row, col)
                .map(|c| c.contents().to_string())
                .unwrap_or_default()
        }
    }

    impl OuterTerminal for RecordingGrid {
        fn enable_raw_mode(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn supports_keyboard_enhancement(&mut self) -> io::Result<bool> {
            Ok(false)
        }
        fn push_keyboard_flags(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn enable_mouse(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn enter_alt_screen(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn move_to(&mut self, col: u16, row: u16) -> io::Result<()> {
            // CSI row+1 ; col+1 H — vt100 is 1-based, gutter's API 0-based.
            let seq = format!("\x1b[{};{}H", row + 1, col + 1);
            self.parser.process(seq.as_bytes());
            Ok(())
        }
        fn write_row(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.parser.process(bytes);
            Ok(())
        }
        fn clear_gutter(
            &mut self,
            margin: u16,
            width: u16,
            real_cols: u16,
            rows: u16,
        ) -> io::Result<()> {
            // Paint blanks over the physical gutter columns, exactly as the real
            // terminal would — so the physical-cell readback sees them cleared.
            let band_end = margin.saturating_add(width).min(real_cols);
            self.parser.process(b"\x1b[0m");
            for row in 0..rows {
                if margin > 0 {
                    let seq = format!("\x1b[{};1H", row + 1);
                    self.parser.process(seq.as_bytes());
                    self.parser.process(&b" ".repeat(margin as usize));
                }
                if real_cols > band_end {
                    let seq = format!("\x1b[{};{}H", row + 1, band_end + 1);
                    self.parser.process(seq.as_bytes());
                    self.parser.process(&b" ".repeat((real_cols - band_end) as usize));
                }
            }
            Ok(())
        }
        fn place_cursor(&mut self, col: u16, row: u16) -> io::Result<()> {
            let seq = format!("\x1b[{};{}H", row + 1, col + 1);
            self.parser.process(seq.as_bytes());
            Ok(())
        }
        fn set_cursor_visible(&mut self, visible: bool) -> io::Result<()> {
            self.parser
                .process(if visible { b"\x1b[?25h" } else { b"\x1b[?25l" });
            Ok(())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn leave_alt_screen(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn pop_keyboard_flags(&mut self) -> io::Result<()> {
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
            self.calls.push(Call::EnableRawMode);
            Ok(())
        }
        fn supports_keyboard_enhancement(&mut self) -> io::Result<bool> {
            self.calls.push(Call::SupportsKeyboardEnhancement);
            Ok(self.supports_kitty)
        }
        fn push_keyboard_flags(&mut self) -> io::Result<()> {
            self.calls.push(Call::PushKeyboardFlags);
            self.kitty_pushed = true;
            Ok(())
        }
        fn enable_mouse(&mut self) -> io::Result<()> {
            self.calls.push(Call::EnableMouse);
            self.mouse_enabled = true;
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
        fn clear_gutter(
            &mut self,
            margin: u16,
            width: u16,
            real_cols: u16,
            rows: u16,
        ) -> io::Result<()> {
            self.calls
                .push(Call::ClearGutter(margin, width, real_cols, rows));
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
            // Mirror the real impl: only pop what was actually pushed.
            if self.kitty_pushed {
                self.calls.push(Call::PopKeyboardFlags);
                self.kitty_pushed = false;
            }
            Ok(())
        }
        fn disable_mouse(&mut self) -> io::Result<()> {
            // Mirror the real impl: only disable what was actually enabled.
            if self.mouse_enabled {
                self.calls.push(Call::DisableMouse);
                self.mouse_enabled = false;
            }
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
