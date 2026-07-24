//! Ctrl-Z / suspend PTY integration tests (ADR-0018/0019): drive the real
//! `gutter` binary through a real PTY and assert on the park → resume byte cycle
//! the outer terminal actually sees when the wrapped child stops.
//!
//! ## Two harness facts that shape every test here
//!
//! **1. Both process groups are orphaned, so `SIGTSTP` is discarded — we trigger
//! the stop with `SIGSTOP` instead.** `expectrl`/`ptyprocess` `setsid`s gutter
//! into its own session whose parent (this test process) lives in a *different*
//! session, so gutter's process group is orphaned; portable-pty likewise
//! `setsid`s the wrapped child into its own session, so the child's group is
//! orphaned too. POSIX discards terminal-stop signals (SIGTSTP/SIGTTIN/SIGTTOU)
//! sent to an orphaned group — verified empirically: a child that runs
//! `kill -TSTP $$` here never stops (it prints straight through). `SIGSTOP` is
//! *not* subject to that rule, always stops, and gutter's design reacts to every
//! stop signal identically (`WSTOPSIG` is carried for logging only — see the
//! ADR-0019 edge-case table), so `kill -STOP $$` is a faithful, deterministic
//! trigger for the exact same suspend path a real `Ctrl-Z` drives.
//!
//! **2. gutter's own `suspend_self` (`kill(0, SIGTSTP)`) is a no-op here, so we
//! assert on the emitted park/resume bytes, not on a real `WaitStatus::Stopped`.**
//! Because gutter's group is orphaned, `kill(0, SIGTSTP)` is discarded and gutter
//! never actually stops — the cycle self-completes (park → no-op self-stop →
//! unpark → `continue_child`) with no external `fg`/SIGCONT needed. That is fine:
//! the wrapped child's `SIGSTOP` is a *real* stop that gutter observes via
//! `waitpid`, and the child only ever resumes because gutter's `continue_child`
//! sends it SIGCONT. So a `RESUMED` marker reappearing after the park bytes is
//! positive proof the whole observe-stop → park → continue cycle ran. In a real
//! interactive session gutter's group is NOT orphaned, `kill(0, SIGTSTP)` really
//! stops it, and `fg` resumes it — that half is covered by the manual checklist,
//! not reproducible under this harness.
//!
//! Headless: a real PTY, no display, `TERM=xterm-256color`.

use std::io::Write;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use expectrl::process::unix::WaitStatus;
use expectrl::process::Healthcheck;
use expectrl::session::OsSession;

/// Serialize every PTY test in this binary — run in parallel they flake under
/// PTY/process contention (issue #1). Mirrors `tests/pty.rs`'s guard; take it on
/// the first line of each test and hold it for the whole test.
fn pty_guard() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Path to the freshly-built `gutter` binary (cargo sets this for the test).
fn gutter_bin() -> String {
    env!("CARGO_BIN_EXE_gutter").to_string()
}

/// Spawn gutter under a real 80x24 PTY wrapping `gutter_args` (a single string so
/// the inner `sh` parses any nested quoting). `stty` pins the size before gutter
/// reads it; `GUTTER_FORCE_ANCHOR_ROW=0` pins the inline anchor.
fn spawn_gutter(gutter_args: &str) -> OsSession {
    let script = format!(
        "stty cols 80 rows 24; exec env GUTTER_FORCE_ANCHOR_ROW=0 {} {gutter_args}",
        gutter_bin()
    );
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.arg("-c").arg(script);
    OsSession::spawn(cmd).expect("spawn gutter under PTY")
}

/// Drain a bounded wall-clock window using non-blocking reads, returning every
/// byte the outer terminal emitted. Stops early on EOF (child + gutter gone).
fn drain_window(session: &mut OsSession, window: Duration) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    let start = Instant::now();
    while start.elapsed() < window {
        match session.try_read(&mut buf) {
            Ok(0) => break, // EOF
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

/// Index of the first occurrence of `needle` in `hay`, if any.
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Block (up to `timeout`) on gutter's exit and return its code, or `None` if it
/// died by signal or never exited. Polls `get_status` (non-blocking `waitpid`)
/// like `tests/pty.rs`'s helper.
fn wait_exit(session: &OsSession, timeout: Duration) -> Option<i32> {
    let proc = session.get_process();
    let start = Instant::now();
    loop {
        match proc.get_status() {
            Ok(WaitStatus::Exited(_, code)) => return Some(code),
            Ok(WaitStatus::Signaled(_, _, _)) => return None,
            _ => {}
        }
        if start.elapsed() > timeout {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// The tail of crossterm's `DisableMouseCapture` and the cursor-show — both part
// of the park restore (and again at final teardown). Finding them BEFORE the
// resume marker is what distinguishes a real park from ordinary teardown.
const MOUSE_OFF: &[u8] = b"\x1b[?1000l";
const CURSOR_SHOW: &[u8] = b"\x1b[?25h";
const MOUSE_ON: &[u8] = b"\x1b[?1000h";

/// **Self-stop → park → resume.** A child prints `START`, stops itself
/// (`SIGSTOP`; see the module note on why not `SIGTSTP`), then prints `RESUMED`.
/// gutter must observe the stop, park the outer terminal (mouse off + cursor
/// shown) *before* handing it back, then — via `continue_child` — resume the
/// child so `RESUMED` appears. `RESUMED` reappearing after the park bytes is the
/// end-to-end proof the whole cycle ran (the child can only leave `SIGSTOP` via
/// gutter's SIGCONT). No external `fg` is sent: gutter's own no-op self-stop
/// means the cycle self-completes under this orphaned-group harness.
#[test]
fn self_stop_parks_then_resumes() {
    let _g = pty_guard();
    let mut s =
        spawn_gutter("--width 40 sh -c 'printf START; kill -STOP $$; printf RESUMED; sleep 0.2'");

    let out = drain_window(&mut s, Duration::from_secs(5));

    let start = find(&out, b"START").expect("child's pre-stop output must appear");
    let resumed = find(&out, b"RESUMED")
        .expect("RESUMED must reappear — proof continue_child resumed the stopped child");
    assert!(start < resumed, "START must precede RESUMED");

    // The park restore lands BEFORE the child is resumed: mouse disabled and
    // cursor shown so the (would-be) shell inherits a sane terminal.
    let park_mouse_off = find(&out, MOUSE_OFF).expect("park must disable mouse");
    let park_cursor_show = find(&out, CURSOR_SHOW).expect("park must show the cursor");
    assert!(
        park_mouse_off < resumed,
        "mouse-disable must be part of the park (before RESUMED), not just teardown"
    );
    assert!(
        park_cursor_show < resumed,
        "cursor-show must be part of the park (before RESUMED), not just teardown"
    );

    assert_eq!(wait_exit(&s, Duration::from_secs(5)), Some(0), "clean exit after resume");
}

/// **Alt-screen child: park leaves the alt screen, resume re-enters it.** The
/// child enters the alt screen and pauses (so gutter renders & mirrors the alt
/// edge to the outer terminal) before stopping. Park must leave the alt screen
/// (`?1049l`) so the shell inherits the primary screen; resume — with the child
/// still in alt — must re-enter it (`?1049h`). We assert a park `?1049l` followed
/// by a later resume `?1049h`.
#[test]
fn alt_screen_left_at_park_reentered_at_resume() {
    let _g = pty_guard();
    let mut s = spawn_gutter(
        "--width 40 sh -c 'tput smcup; printf READY; sleep 0.3; kill -STOP $$; sleep 0.3; tput rmcup'",
    );

    let out = drain_window(&mut s, Duration::from_secs(5));

    // The startup mirror enters alt; then park leaves it, then resume re-enters.
    // Locate the park leave-alt, then require a re-enter strictly after it.
    let park_leave = find(&out, b"\x1b[?1049l").expect("park must leave the alt screen");
    let after = &out[park_leave + 8..];
    assert!(
        find(after, b"\x1b[?1049h").is_some(),
        "resume must re-enter the alt screen (a ?1049h after the park's ?1049l)"
    );

    let _ = wait_exit(&s, Duration::from_secs(5));
}

/// **Child dies by SIGKILL across the suspend cycle → gutter exits 137.** The
/// child stops (`SIGSTOP`), and once gutter's `continue_child` resumes it, it
/// SIGKILLs itself. The waiter's `waitpid` reaps a `WIFSIGNALED(SIGKILL)` and
/// maps it to `128 + 9 = 137`, which gutter propagates as its own exit code.
///
/// This is the deterministic, non-flaky stand-in for the plan's "SIGKILL the
/// child *while stopped*": under the orphaned-group harness we cannot land an
/// external kill precisely inside the (~sub-100ms) window before `continue_child`
/// fires without a race, so the child self-kills the instant it is resumed. The
/// stop is a genuine `SIGSTOP` park cycle (park bytes below prove it ran), and the
/// assertion of record — signal-death mapped to 137 through the suspend path — is
/// exercised exactly.
#[test]
fn child_sigkilled_across_suspend_exits_137() {
    let _g = pty_guard();
    let mut s = spawn_gutter("--width 40 sh -c 'printf START; kill -STOP $$; kill -KILL $$'");

    let out = drain_window(&mut s, Duration::from_secs(2));
    assert!(find(&out, b"START").is_some(), "child ran before stopping");
    assert!(
        find(&out, MOUSE_OFF).is_some(),
        "a real SIGSTOP park cycle ran (mouse-disable emitted)"
    );

    // 137 = 128 + SIGKILL(9). gutter reaps the child and exits with that code.
    assert_eq!(
        wait_exit(&s, Duration::from_secs(5)),
        Some(137),
        "gutter must propagate the child's SIGKILL death as 137"
    );
}

/// **Input liveness after resume.** After the park/resume cycle, a keystroke
/// typed into gutter must still reach the child — proving the input path (Thread
/// 3's read pump and the render thread's scanner) is live post-resume. The child stops,
/// then (once resumed) `read`s a line; we send one and expect the child to echo
/// it back through gutter.
#[test]
fn input_reaches_child_after_resume() {
    let _g = pty_guard();
    let mut s = spawn_gutter(
        "--width 40 sh -c 'printf START; kill -STOP $$; read x; printf \"GOT-$x\"; sleep 0.1'",
    );

    // Drain until the resume has happened: mouse is re-enabled at unpark, so a
    // SECOND ?1000h (the first is startup capture) marks the child as resumed and
    // its `read` now waiting.
    let start = Instant::now();
    let mut seen = Vec::new();
    let mut buf = [0u8; 8192];
    while start.elapsed() < Duration::from_secs(3) {
        match s.try_read(&mut buf) {
            Ok(0) => break,
            Ok(n) => seen.extend_from_slice(&buf[..n]),
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
        let resumed = seen.windows(MOUSE_ON.len()).filter(|w| *w == MOUSE_ON).count() >= 2;
        if resumed {
            break;
        }
        std::thread::sleep(Duration::from_millis(3));
    }
    assert!(
        seen.windows(MOUSE_ON.len()).filter(|w| *w == MOUSE_ON).count() >= 2,
        "resume must have re-enabled mouse capture before we type"
    );

    s.write_all(b"hello\r").expect("send a keystroke to the resumed child");

    let out = drain_window(&mut s, Duration::from_secs(3));
    assert!(
        find(&out, b"GOT-hello").is_some(),
        "the keystroke typed after resume must reach the child's `read`"
    );

    let _ = wait_exit(&s, Duration::from_secs(5));
}
