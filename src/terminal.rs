//! Outer-terminal lifecycle (Thread 2 only): raw mode, alt screen, eager
//! `EnableMouseCapture`, kitty `PushKeyboardEnhancementFlags` when
//! `supports_keyboard_enhancement()` grants it, and the explicit restore
//! (leave alt screen → pop kitty flags → disable mouse → show cursor →
//! disable raw mode) run BEFORE `process::exit` — `process::exit` does not run
//! destructors, so teardown cannot be a `Drop` guard. See ADR-010.
//!
//! The side effects sit behind the [`OuterTerminal`] trait so the ordered
//! restore can be unit-tested against a recording mock without a real terminal.
//! In slice 01 `pop_keyboard_flags` and `disable_mouse` are no-op placeholders
//! in the real impl (kitty is slice 04, mouse is slice 07) but stay in the
//! restore sequence so those slices drop in without re-sequencing.

use std::io;

/// The outer-terminal side effects the setup and teardown paths perform.
///
/// The real impl ([`CrosstermTerminal`]) wraps crossterm; the test mock
/// ([`mock::MockTerminal`]) records the ordered sequence of calls so the
/// ADR-010 restore-order test can assert against it.
pub trait OuterTerminal {
    /// Enter raw mode. Setup; first thing after the PTY is up.
    fn enable_raw_mode(&mut self) -> io::Result<()>;
    /// Enter the alternate screen. Setup; after raw mode.
    fn enter_alt_screen(&mut self) -> io::Result<()>;

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
pub struct CrosstermTerminal;

impl CrosstermTerminal {
    pub fn new() -> Self {
        Self
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
        use crossterm::{execute, terminal::EnterAlternateScreen};
        execute!(io::stdout(), EnterAlternateScreen)
    }

    fn leave_alt_screen(&mut self) -> io::Result<()> {
        use crossterm::{execute, terminal::LeaveAlternateScreen};
        execute!(io::stdout(), LeaveAlternateScreen)
    }

    fn pop_keyboard_flags(&mut self) -> io::Result<()> {
        // No-op in slice 01 — nothing was pushed. Slice 04 pops the kitty
        // enhancement flags here.
        Ok(())
    }

    fn disable_mouse(&mut self) -> io::Result<()> {
        // No-op in slice 01 — mouse capture is never enabled. Slice 07
        // disables capture here.
        Ok(())
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        use crossterm::{cursor::Show, execute};
        execute!(io::stdout(), Show)
    }

    fn disable_raw_mode(&mut self) -> io::Result<()> {
        crossterm::terminal::disable_raw_mode()
    }
}

#[cfg(test)]
pub mod mock {
    //! A recording [`OuterTerminal`] for the ADR-010 restore-order test.

    use super::OuterTerminal;
    use std::io;

    /// One recorded side effect, in the order it was invoked.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Call {
        EnableRawMode,
        EnterAltScreen,
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
