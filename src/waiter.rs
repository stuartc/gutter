//! Thread 4 — the waiter. On unix it loops on raw
//! `waitpid(pid, …, WUNTRACED | WCONTINUED)`, surviving stop/continue events and
//! only ending the thread on real child death. This — NOT PTY EOF (unreliable:
//! EIO on Linux, can block while a grandchild holds the slave fd) — is the
//! authoritative child-state signal. See ADR-0018.
//!
//! Reap-ownership rule: once raw `waitpid` owns reaping this pid, nothing else
//! may wait on it — no `child.wait()`, no `try_wait()`. One reaper, forever. The
//! `child` handle stays alive in scope (so the master fd survives) but its
//! `wait()` is never called; portable-pty's `Drop` swallows the post-reap
//! `ECHILD` at teardown.

use std::sync::mpsc::Sender;

use portable_pty::{Child, ExitStatus};

use crate::msg::Msg;

/// Thread 4 body. On unix, reap `pid` via raw `waitpid` so stops and continues
/// become `Msg::ChildStopped` / `Msg::ChildContinued` while only real death ends
/// the thread with `Msg::ChildExited`. `pid == None` (no `process_id`) falls back
/// to the legacy single `child.wait()` — no suspend support.
#[cfg(unix)]
pub fn run(pid: Option<u32>, child: Box<dyn Child + Send + Sync>, merged: Sender<Msg>) {
    let Some(pid) = pid else {
        return legacy_wait(child, merged);
    };
    // Keep `child` alive so the PTY master fd stays open, but NEVER call
    // child.wait()/try_wait(): `wait_loop` below is the sole reaper (reap-ownership
    // rule). A second reaper would race or hit ECHILD.
    let _child = child;
    wait_loop(pid as libc::pid_t, merged);
}

/// The raw reap loop, factored out so a unit test can drive it against a plain
/// `std::process::Command` child (no PTY): SIGSTOP → `ChildStopped`, SIGCONT →
/// `ChildContinued`, SIGKILL → `ChildExited(128+9)`.
#[cfg(unix)]
fn wait_loop(pid: libc::pid_t, merged: Sender<Msg>) {
    loop {
        let mut status: libc::c_int = 0;
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED | libc::WCONTINUED) };
        if r == -1 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            // No child to reap (already reaped, or never ours). Synthesise a
            // non-zero exit so the render loop still tears down rather than hangs.
            let _ = merged.send(Msg::ChildExited(ExitStatus::with_exit_code(1)));
            return;
        }
        if libc::WIFSTOPPED(status) {
            let _ = merged.send(Msg::ChildStopped {
                sig: libc::WSTOPSIG(status),
            });
            continue; // waitpid now blocks until the next state change
        }
        if libc::WIFCONTINUED(status) {
            let _ = merged.send(Msg::ChildContinued);
            continue;
        }
        // Real death — WIFEXITED or WIFSIGNALED. The conventional 128+sig mapping
        // gives a signal-killed child a non-zero code.
        let code = if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status) as u32
        } else {
            128 + libc::WTERMSIG(status) as u32
        };
        let _ = merged.send(Msg::ChildExited(ExitStatus::with_exit_code(code)));
        return;
    }
}

/// The legacy single-`wait()` path: no stop observation, no suspend support. Used
/// on unix only when the child has no `process_id`, and unconditionally off-unix.
#[cfg(unix)]
fn legacy_wait(mut child: Box<dyn Child + Send + Sync>, merged: Sender<Msg>) {
    let status = child
        .wait()
        .unwrap_or_else(|_| ExitStatus::with_exit_code(1));
    let _ = merged.send(Msg::ChildExited(status));
}

/// Off-unix: no `waitpid`, so keep today's behaviour — block on one `child.wait()`
/// and report the exit. No suspend support. `pid` is ignored.
#[cfg(not(unix))]
pub fn run(_pid: Option<u32>, mut child: Box<dyn Child + Send + Sync>, merged: Sender<Msg>) {
    let status = child
        .wait()
        .unwrap_or_else(|_| ExitStatus::with_exit_code(1));
    let _ = merged.send(Msg::ChildExited(status));
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::mpsc::channel;
    use std::time::Duration;

    /// Drive `wait_loop` against a real (non-PTY) child and assert a stop is
    /// reported as `ChildStopped` (never mistaken for exit) and a kill as
    /// `ChildExited(128+sig)`. The `ChildStopped` half is the empirical proof that
    /// `waitpid(WUNTRACED)` observes a self-stopping child — the load-bearing
    /// assumption behind the whole suspend design (ADR-0018).
    ///
    /// `ChildContinued` (from `WCONTINUED`) is best-effort: Linux reports it, but
    /// Darwin's `waitpid` does NOT wake on `SIGCONT`, so it never fires on macOS.
    /// That is fine — `ChildContinued` is only a non-load-bearing repaint hint
    /// (resume is driven by our own `kill(0, SIGTSTP)` returning, not by observing
    /// the child), so the test tolerates its absence.
    #[test]
    // `wait_loop` is the sole reaper (reap-ownership rule) — we deliberately never
    // call `child.wait()` here; the raw `waitpid` inside `wait_loop` reaps it.
    #[allow(clippy::zombie_processes)]
    fn wait_loop_reports_stop_continue_exit() {
        let child = Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as libc::pid_t;

        let (tx, rx) = channel::<Msg>();
        let h = std::thread::spawn(move || wait_loop(pid, tx));

        // Stop it — expect ChildStopped, not ChildExited. LOAD-BEARING.
        unsafe { libc::kill(pid, libc::SIGSTOP) };
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Msg::ChildStopped { sig }) => assert_eq!(sig, libc::SIGSTOP),
            other => panic!("expected ChildStopped(SIGSTOP), got {other:?}"),
        }

        // Continue it — ChildContinued IF the platform reports WCONTINUED. macOS
        // does not, and this hint is non-load-bearing, so tolerate a timeout.
        unsafe { libc::kill(pid, libc::SIGCONT) };
        let _ = rx.recv_timeout(Duration::from_secs(1));

        // Kill it — expect ChildExited(128+9), skipping a late ChildContinued if
        // one was queued (Linux) between the SIGCONT and the kill.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        loop {
            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(Msg::ChildContinued) => continue,
                Ok(Msg::ChildExited(status)) => {
                    assert_eq!(status.exit_code(), 128 + 9);
                    break;
                }
                other => panic!("expected ChildExited(137), got {other:?}"),
            }
        }

        h.join().expect("waiter thread joins after exit");
    }
}
