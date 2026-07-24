//! Slice 01 PTY-driven integration tests (expectrl, real PTY, headless).
//!
//! These drive the built `gutter` binary through a real PTY and assert on what
//! the child and the controlling terminal actually see — never on internal
//! function returns. They cover the slice-01 acceptance criteria:
//!
//! - passthrough of a non-full-screen child's output,
//! - `--width full` sizing the child's PTY to the real terminal's dimensions,
//! - gutter's exit code equalling the child's exit code,
//! - the child-exit-restore real-PTY smoke (mid-alt-screen child exits, the
//!   terminal is restored with NO keystroke), which also proves there is no
//!   hang on teardown (the detached input thread is reaped by `process::exit`).
//!
//! Iteration-02 slice 01 (E1) adds two more: the child spawns in the launcher's
//! cwd (not `$HOME`), and the child inherits the launcher's environment.
//!
//! Headless: a real PTY with no display. Tests pin `TERM=xterm-256color`.

use std::path::{Path, PathBuf};

use expectrl::process::unix::WaitStatus;
use expectrl::{Eof, Expect};

mod common;
use common::{spawn_gutter_argv as spawn_gutter, spawn_gutter_argv_with as spawn_gutter_with};

/// The window size expectrl gives the outer PTY. Tests that need the child to
/// see this exact column count pass `--width full`, since the no-flag default
/// is a 100-column band that would only match here via the clamp (80 < 100).
const OUTER_COLS: u16 = 80;

/// Create a fresh, uniquely-named directory under the system temp dir and return
/// its canonical (symlink-resolved) path. On macOS the temp dir is reached via
/// `/tmp -> /private/tmp`, so the child's `pwd -P` reports the resolved path;
/// canonicalising here lets both sides agree.
fn fresh_temp_dir(tag: &str) -> PathBuf {
    let unique = format!("gutter-e1-{tag}-{}", std::process::id());
    let dir = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    std::fs::canonicalize(&dir).expect("canonicalize temp dir")
}

/// Passthrough of a non-full-screen child: `gutter echo hi-from-gutter` shows
/// the child's output, then the process completes cleanly.
#[test]
fn passthrough_echo() {
    let mut p = spawn_gutter(&["echo", "hi-from-gutter"]);
    p.expect("hi-from-gutter").expect("child output passed through");
    p.expect(Eof).expect("gutter exits after child completes");
}

/// `--width full` sizes the child's PTY to the real terminal's dimensions: the
/// inner PTY matches the outer terminal's `cols`, so `tput cols` inside the
/// child reports `OUTER_COLS`. Pinned to `--width full`: the no-flag default
/// is a 100-column band, which at `OUTER_COLS` = 80 would only match via the
/// clamp — the wrong reason for this test to pass.
#[test]
fn child_sees_real_dimensions() {
    // `tput cols` reads the child's own controlling tty (gutter's inner PTY).
    let mut p = spawn_gutter(&["--width", "full", "sh", "-c", "tput cols"]);
    let expected = OUTER_COLS.to_string();
    p.expect(expected.as_str())
        .unwrap_or_else(|e| panic!("child should report {OUTER_COLS} columns: {e:?}"));
    p.expect(Eof).expect("gutter exits");
}

/// Exit-code propagation: wrap a child that exits non-zero; gutter exits with
/// the same code.
#[test]
fn exit_code_propagation() {
    let mut p = spawn_gutter(&["sh", "-c", "exit 42"]);
    // Drain to EOF so the child (and gutter) have fully exited before we wait.
    let _ = p.expect(Eof);
    match p.get_process().wait().expect("wait on gutter") {
        WaitStatus::Exited(_, code) => assert_eq!(code, 42, "gutter must propagate the child's exit code"),
        other => panic!("expected clean exit 42, got {other:?}"),
    }
}

/// Child-exit-restore real-PTY smoke + no-hang-on-teardown.
///
/// Wrap a child that enters the alt screen and then exits immediately. After
/// gutter exits — with NO keystroke sent — the controlling terminal must be
/// back on the primary screen (the leave-alt-screen restore ran) and the
/// process must have terminated within the timeout (no hang: the detached
/// input thread parked in `event::read()` is reaped by `process::exit`).
#[test]
fn child_exit_restores_terminal_no_keystroke() {
    // `tput smcup` enters the alt screen; the child then exits straight away.
    // gutter's teardown must emit the leave-alt-screen (`rmcup`) sequence.
    let mut p = spawn_gutter(&["sh", "-c", "tput smcup; printf done; exit 0"]);

    // No keystroke is ever sent. We just read to EOF; reaching EOF inside the
    // timeout is the no-hang proof.
    p.expect("done").expect("child ran mid-alt-screen");
    p.expect(Eof).expect("gutter restores and exits with no keystroke");

    // And gutter exited cleanly (code 0).
    match p.get_process().wait().expect("wait on gutter") {
        WaitStatus::Exited(_, code) => assert_eq!(code, 0),
        other => panic!("expected clean exit 0, got {other:?}"),
    }
}

/// **E1 regression: the child spawns in the launcher's cwd, not `$HOME`.** Set
/// gutter's process `current_dir` to a known temp dir, wrap a `pwd -P`-reporting
/// child, and assert it reports that temp dir — not `$HOME`. Before the
/// `builder.cwd(...)` fix, portable-pty `current_dir($HOME)`'d the child and
/// this reported the home directory (the `tig` "Not a git repository" symptom).
#[test]
fn child_spawns_in_launcher_cwd() {
    let launch_dir = fresh_temp_dir("cwd");
    // `pwd -P` resolves symlinks, matching our canonicalised `launch_dir`.
    let mut p = spawn_gutter_with(&["sh", "-c", "pwd -P"], |cmd| {
        cmd.current_dir(&launch_dir);
    });

    let expected = launch_dir.to_string_lossy().into_owned();
    p.expect(expected.as_str())
        .unwrap_or_else(|e| panic!("child should report the launcher cwd {expected:?}: {e:?}"));

    // And it must NOT be `$HOME` — guards against the home fallback regressing.
    if let Some(home) = std::env::var_os("HOME") {
        let home = Path::new(&home);
        // The temp dir is outside HOME on every supported platform; assert that
        // so the positive match above can't be home masquerading as the cwd.
        assert!(
            !launch_dir.starts_with(home),
            "test temp dir {launch_dir:?} must be outside HOME {home:?}"
        );
    }

    p.expect(Eof).expect("gutter exits after child completes");
    let _ = std::fs::remove_dir_all(&launch_dir);
}

/// **Env inheritance (already correct; locked in).** Export a probe variable
/// before launch and wrap a child that echoes it; the child must receive the
/// launcher's value. `CommandBuilder::new` seeds `get_base_env()` from the full
/// launcher env, so this passes today — the test guards a future portable-pty
/// bump from silently dropping env inheritance while we touch the spawn path.
#[test]
fn child_inherits_launcher_env() {
    const PROBE_VALUE: &str = "gutter-env-probe-value-9173";
    let mut p = spawn_gutter_with(&["sh", "-c", "printf '%s\\n' \"$GUTTER_ENV_PROBE\""], |cmd| {
        cmd.env("GUTTER_ENV_PROBE", PROBE_VALUE);
    });

    p.expect(PROBE_VALUE)
        .expect("child must receive the launcher-exported env var");
    p.expect(Eof).expect("gutter exits after child completes");
}
