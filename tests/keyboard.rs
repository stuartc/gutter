//! Keyboard re-encode PTY round-trips (slice 04 acceptance criteria).
//!
//! gutter ALWAYS re-encodes each `KeyEvent` (crossterm gives no raw bytes), so
//! these tests assert on what the **child actually receives** — never on the
//! grid, which a mis-encode leaves perfectly clean (ADR-002). The child is a
//! tiny reader that echoes the exact bytes it read back into gutter's band, so
//! the outer frame (parsed through a second `vt100`) shows what reached the
//! child.
//!
//! Byte-exactness of the kitty `CSI 13 u` / `CSI 13 ; 2 u` forms is proven
//! deterministically by the in-crate encoder unit/proptest and the
//! through-dispatch case-A/case-B tests (`src/render.rs`), which assert on the
//! bytes written to the PTY-master writer — i.e. the child-received bytes —
//! along the real `parser.process` → kitty-state → encode path. These PTY tests
//! layer the end-to-end leg on top: real binary, real PTY, headless.
//!
//! The kitty capability probe queries the real terminal, which a dumb test PTY
//! cannot answer, so `GUTTER_FORCE_KITTY` injects the `outer_supports` bool
//! (`1` → case A, `0` → case B) — the injectable seam the PRD names.
//!
//! CI runs these headlessly: a real PTY, no display, `TERM=xterm-256color`.

use std::time::{Duration, Instant};

use expectrl::session::OsSession;

fn gutter_bin() -> String {
    env!("CARGO_BIN_EXE_gutter").to_string()
}

/// Run gutter inside an outer terminal of a fixed size with an optional
/// `GUTTER_FORCE_KITTY` override: `sh -c 'stty ...; exec env VAR=.. gutter ..'`.
fn gutter_in_terminal(
    outer_cols: u16,
    outer_rows: u16,
    force_kitty: Option<bool>,
    gutter_args: &str,
) -> std::process::Command {
    let force = match force_kitty {
        Some(true) => "GUTTER_FORCE_KITTY=1 ",
        Some(false) => "GUTTER_FORCE_KITTY=0 ",
        None => "",
    };
    let script = format!(
        "stty cols {outer_cols} rows {outer_rows}; exec env {force}GUTTER_FORCE_ANCHOR_ROW=0 {} {gutter_args}",
        gutter_bin()
    );
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.arg("-c").arg(script);
    cmd
}

fn spawn(cmd: std::process::Command) -> OsSession {
    OsSession::spawn(cmd).expect("spawn gutter under PTY")
}

/// Drain a bounded window with non-blocking reads, so the window is a real
/// wall-clock cap even while the child keeps the PTY open.
fn drain_window(session: &mut OsSession, window: Duration) -> Vec<u8> {
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

/// Parse outer-terminal bytes through a vt100 at the physical size.
fn outer_grid(bytes: &[u8], cols: u16, rows: u16) -> vt100::Parser {
    let mut p = vt100::Parser::new(rows, cols, 0);
    p.process(bytes);
    p
}

/// The whole grid joined to a single string, so a test can search for echoed
/// content regardless of which row it landed on.
fn grid_text(screen: &vt100::Screen, cols: u16, rows: u16) -> String {
    (0..rows)
        .map(|r| screen.rows(0, cols).nth(r as usize).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n")
}

/// **Ordinary typing reaches the child correctly through the re-encoder** (the
/// AC's PTY-driven ordinary-typing leg). Type ASCII into a `cat` that echoes its
/// stdin; the echoed text must appear in gutter's band — proving plain keys are
/// re-encoded to the bytes the child reads back. Run under BOTH outer-capability
/// states to confirm ordinary keys are level-independent.
#[test]
fn ordinary_typing_reaches_child_both_capability_states() {
    for force in [Some(true), Some(false)] {
        let cmd = gutter_in_terminal(120, 40, force, "--width 100 /bin/cat");
        let mut session = spawn(cmd);
        // Let gutter come up.
        std::thread::sleep(Duration::from_millis(400));

        // Type a marker. `cat` echoes stdin to stdout → gutter renders it.
        use std::io::Write;
        session.write_all(b"HELLO_KBD").unwrap();
        session.flush().unwrap();

        let bytes = drain_window(&mut session, Duration::from_millis(600));
        let parser = outer_grid(&bytes, 120, 40);
        let text = grid_text(parser.screen(), 120, 40);
        assert!(
            text.contains("HELLO_KBD"),
            "ordinary typing must reach the child (force_kitty={force:?}); grid was {text:?}"
        );

        drop(session);
    }
}

/// **Enter reaches the child** (the AC names Enter among the common keys). A
/// shell `read` loop completes one read per Enter and prints a per-line marker,
/// so each Enter that the re-encoder delivers becomes a `GOT:` token — an
/// unambiguous proof (unlike `cat`, where a bare `\r` just rewinds the cursor
/// and visually overwrites the previous line). Two Enters → two markers. The
/// child stays alive (a long sleep after the loop) so the live alt-screen frame
/// is captured.
#[test]
fn enter_reaches_child() {
    let child = "/bin/sh -c 'i=0; while [ $i -lt 2 ] && read x; do printf \"GOT:%s \" \"$x\"; i=$((i+1)); done; sleep 3'";
    let cmd = gutter_in_terminal(120, 40, Some(false), &format!("--width 100 {child}"));
    let mut session = spawn(cmd);
    std::thread::sleep(Duration::from_millis(400));

    use std::io::Write;
    // Two lines, each terminated by Enter (\r): the read loop fires twice.
    session.write_all(b"ALPHA\r").unwrap();
    session.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    session.write_all(b"BETA\r").unwrap();
    session.flush().unwrap();

    let bytes = drain_window(&mut session, Duration::from_millis(700));
    let parser = outer_grid(&bytes, 120, 40);
    let text = grid_text(parser.screen(), 120, 40);
    assert!(
        text.contains("GOT:ALPHA") && text.contains("GOT:BETA"),
        "each Enter must complete a child read (two markers); grid was {text:?}"
    );

    drop(session);
}

/// **Case B end-to-end: Ctrl-C reaches the child as the legacy `0x03`.** Wrap a
/// shell that reports when it receives SIGINT (which the tty driver raises from
/// the `0x03` byte), then send Ctrl-C. Seeing the trap's marker on gutter's live
/// alt-screen frame proves the re-encoder delivered the control byte the child's
/// tty expects — independent of kitty. Forced non-kitty (case B).
///
/// The trap prints its marker then keeps the child ALIVE (a long sleep), so the
/// marker stays on the live alt-screen frame the drain window captures —
/// exiting would leave the alt screen and discard that frame.
#[test]
fn ctrl_c_delivers_interrupt_to_child() {
    let child = "/bin/sh -c 'trap \"printf GOTSIGINT; sleep 3\" INT; while true; do sleep 0.1; done'";
    let cmd = gutter_in_terminal(120, 40, Some(false), &format!("--width 100 {child}"));
    let mut session = spawn(cmd);
    std::thread::sleep(Duration::from_millis(500));

    use std::io::Write;
    session.write_all(&[0x03]).unwrap(); // raw Ctrl-C byte → crossterm decodes to Ctrl+C KeyEvent
    session.flush().unwrap();

    let bytes = drain_window(&mut session, Duration::from_millis(900));
    let parser = outer_grid(&bytes, 120, 40);
    let text = grid_text(parser.screen(), 120, 40);
    assert!(
        text.contains("GOTSIGINT"),
        "Ctrl-C must reach the child as 0x03 and raise SIGINT; grid was {text:?}"
    );

    drop(session);
}
