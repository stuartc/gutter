//! Thread 2 — the render loop (slice 01 = verbatim passthrough).
//!
//! Owns the outer-terminal output handle and the PTY master writer exclusively.
//! Blocks on a single `recv()` over the merged `Msg` channel (one park point,
//! zero idle CPU) and dispatches:
//!
//! - `Msg::Pty(b)`   → write `b` verbatim to the output and flush (no grid, no
//!   offset yet — that is slice 02).
//! - `Msg::Input(e)` → re-encode the key event (throwaway legacy encoder) and
//!   write it to the PTY master.
//! - `Msg::ChildExited(s)` → record the exit code, run the ordered restore on
//!   the [`OuterTerminal`], and **break** — returning the code to `main`, which
//!   calls `process::exit`. The loop must NOT call `process::exit` itself, so
//!   it stays unit-testable (ADR-010).
//!
//! Shutdown is driven by `Msg::ChildExited` (the waiter), never by PTY EOF and
//! never by a plain `recv()` `Disconnected` — `Disconnected` is only a backstop
//! for "all producers truly gone". The 60fps coalescing loop, the vt100 parser,
//! and the margin maths are all slice 02; keep this a flat `recv()` loop.

use std::io::Write;
use std::sync::mpsc::Receiver;

use crate::keyboard;
use crate::msg::Msg;
use crate::terminal::OuterTerminal;

/// Run the render loop until the child exits, then restore the terminal in
/// order and return the child's exit code.
///
/// Parameters are all injected so a test can drive the loop with a directly-fed
/// `Receiver<Msg>`, a `Vec<u8>` output sink, a `Vec<u8>` PTY-writer sink, and a
/// mock [`OuterTerminal`] — no threads, no PTY, no real terminal.
///
/// Returns the exit code to propagate. `None` means the channel disconnected
/// without a `ChildExited` (the backstop path) — `main` treats that as a clean
/// exit but it is not the normal shutdown route.
pub fn run<T, O, P>(
    rx: Receiver<Msg>,
    terminal: &mut T,
    out: &mut O,
    pty_writer: &mut P,
) -> Option<i32>
where
    T: OuterTerminal,
    O: Write,
    P: Write,
{
    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Pty(bytes) => {
                // Verbatim blit. A full-screen child renders as if gutter
                // weren't here. Slice 02 replaces this with a grid repaint.
                let _ = out.write_all(&bytes);
                let _ = out.flush();
            }
            Msg::Input(event) => {
                if let crossterm::event::Event::Key(key) = event {
                    if let Some(bytes) = keyboard::encode(&key) {
                        let _ = pty_writer.write_all(&bytes);
                        let _ = pty_writer.flush();
                    }
                }
                // Resize / focus / mouse events are swallowed in slice 01.
            }
            Msg::ChildExited(status) => {
                let _ = run_teardown(terminal);
                return Some(status.exit_code() as i32);
            }
        }
    }
    // Backstop only: all senders dropped with no ChildExited. Still restore so
    // the terminal is never left wedged.
    let _ = run_teardown(terminal);
    None
}

/// The explicit, ordered terminal restore (ADR-010). Run BEFORE `process::exit`
/// because `process::exit` runs no destructors — this cannot be a `Drop` guard.
///
/// Order is load-bearing: leave alt screen → pop kitty flags → disable mouse →
/// show cursor → disable raw mode. Pop/disable are no-ops in slice 01 but stay
/// in the sequence so slices 04/07 drop in without re-sequencing.
fn run_teardown<T: OuterTerminal>(terminal: &mut T) -> std::io::Result<()> {
    terminal.leave_alt_screen()?;
    terminal.pop_keyboard_flags()?;
    terminal.disable_mouse()?;
    terminal.show_cursor()?;
    terminal.disable_raw_mode()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::mock::{Call, MockTerminal};
    use portable_pty::ExitStatus;
    use std::sync::mpsc::channel;

    /// The ADR-010 keystone: feed `Msg::ChildExited` into the loop with the
    /// input channel empty, and assert the restore side effects fire in order,
    /// the loop exits rather than parks, and the returned code matches.
    #[test]
    fn child_exit_restores_in_order_and_returns_code() {
        let (tx, rx) = channel::<Msg>();
        let mut terminal = MockTerminal::new();
        let mut out: Vec<u8> = Vec::new();
        let mut pty: Vec<u8> = Vec::new();

        tx.send(Msg::ChildExited(ExitStatus::with_exit_code(42)))
            .unwrap();
        // Drop the sender so a missing ChildExited would Disconnect rather than
        // park forever — but ChildExited must drive the exit, not Disconnect.
        drop(tx);

        let code = run(rx, &mut terminal, &mut out, &mut pty);

        assert_eq!(code, Some(42), "exit code must equal status.exit_code()");
        assert_eq!(
            terminal.calls,
            vec![
                Call::LeaveAltScreen,
                Call::PopKeyboardFlags,
                Call::DisableMouse,
                Call::ShowCursor,
                Call::DisableRawMode,
            ],
            "restore must fire in the ADR-010 order"
        );
    }

    /// Shutdown is reachable purely from `Msg::ChildExited` — the loop has no
    /// PTY-EOF exit branch. PTY bytes flow through and the loop keeps running;
    /// only `ChildExited` breaks it. Encodes the ADR-010 "not EOF" decision.
    #[test]
    fn shutdown_driven_by_child_exited_not_pty_eof() {
        let (tx, rx) = channel::<Msg>();
        let mut terminal = MockTerminal::new();
        let mut out: Vec<u8> = Vec::new();
        let mut pty: Vec<u8> = Vec::new();

        // Some PTY output, then the child exits. No EOF signal exists in Msg.
        tx.send(Msg::Pty(b"hello".to_vec())).unwrap();
        tx.send(Msg::Pty(b" world".to_vec())).unwrap();
        tx.send(Msg::ChildExited(ExitStatus::with_exit_code(0)))
            .unwrap();

        let code = run(rx, &mut terminal, &mut out, &mut pty);

        assert_eq!(code, Some(0));
        assert_eq!(out, b"hello world", "PTY bytes blitted verbatim");
        // Restore fired exactly once, in order — proving ChildExited drove it.
        assert_eq!(terminal.calls.first(), Some(&Call::LeaveAltScreen));
        assert_eq!(terminal.calls.last(), Some(&Call::DisableRawMode));
    }

    /// A key event is re-encoded and written to the PTY writer (smoke only —
    /// encoding correctness is NOT a slice-01 criterion; deleted in slice 04).
    #[test]
    fn key_input_is_reencoded_to_pty() {
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

        let (tx, rx) = channel::<Msg>();
        let mut terminal = MockTerminal::new();
        let mut out: Vec<u8> = Vec::new();
        let mut pty: Vec<u8> = Vec::new();

        tx.send(Msg::Input(Event::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
        ))))
        .unwrap();
        tx.send(Msg::Input(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ))))
        .unwrap();
        tx.send(Msg::ChildExited(ExitStatus::with_exit_code(0)))
            .unwrap();

        run(rx, &mut terminal, &mut out, &mut pty);

        assert_eq!(pty, b"a\r", "letter then Enter produce plausible bytes");
    }
}
