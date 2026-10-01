//! Spawn-and-teardown PTY integration tests (expectrl, real PTY, headless).
//!
//! These drive the built `gutter` binary through a real PTY and assert on what
//! the child and the controlling terminal actually see — never on internal
//! function returns:
//!
//! - passthrough of a non-full-screen child's output,
//! - `--width full` sizing the child's PTY to the real terminal's dimensions,
//! - gutter's exit code equalling the child's exit code,
//! - the child-exit-restore real-PTY smoke (mid-alt-screen child exits, the
//!   terminal is restored with NO keystroke), which also proves there is no
//!   hang on teardown (the detached input thread is reaped by `process::exit`),
//! - the child spawning in the launcher's cwd (not `$HOME`) and inheriting the
//!   launcher's environment.
//!
//! Headless: a real PTY with no display. Tests pin `TERM=xterm-256color`.

use std::path::{Path, PathBuf};

mod common;
use common::{row_text, Gutter};

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

/// Passthrough of a non-full-screen child: its output shows on the outer terminal,
/// and gutter exits cleanly once it does.
#[test]
fn passthrough_echo() {
    let mut gutter = Gutter::spawn_argv(&["sh", "-c", "echo hi-from-gutter; read _"]);
    gutter.wait_for("the child's output", |s| s.contents().contains("hi-from-gutter"));
    gutter.send(b"\n");
    assert_eq!(gutter.finish().code, Some(0), "gutter exits after child completes");
}

/// `--width full` sizes the child's PTY to the real terminal's dimensions: the
/// inner PTY matches the outer terminal's `cols`, so `tput cols` inside the
/// child reports `OUTER_COLS`. Pinned to `--width full`: the no-flag default
/// is a 100-column band, which at `OUTER_COLS` = 80 would only match via the
/// clamp — the wrong reason for this test to pass.
#[test]
fn child_sees_real_dimensions() {
    // `tput cols` reads the child's own controlling tty (gutter's inner PTY).
    let mut gutter = Gutter::spawn_argv(&["--width", "full", "sh", "-c", "tput cols; read _"]);
    let expected = OUTER_COLS.to_string();
    gutter.wait_for("the child's column count", |s| row_text(s, 0) == expected);
    gutter.send(b"\n");
    gutter.finish();
}

/// Exit-code propagation: wrap a child that exits non-zero; gutter exits with
/// the same code.
#[test]
fn exit_code_propagation() {
    // The child prints nothing, so there is nothing to see first.
    let done = Gutter::spawn_argv(&["sh", "-c", "exit 42"]).finish();
    assert_eq!(done.code, Some(42), "gutter must propagate the child's exit code");
}

/// Child-exit-restore real-PTY smoke + no-hang-on-teardown.
///
/// Wrap a child that enters the alt screen and exits inside it. After gutter
/// exits — with NO keystroke sent — the controlling terminal must be back on the
/// primary screen (the leave-alt-screen restore ran) and the process must have
/// terminated (no hang: the detached input thread parked in `read()` is reaped by
/// `process::exit`).
#[test]
fn child_exit_restores_terminal_no_keystroke() {
    // The child is held on a FIFO rather than on its terminal, so the test can let
    // it go without typing anything at gutter.
    let gate = fresh_temp_dir("gate").join("fifo");
    let mut gutter = Gutter::spawn_argv(&[
        "sh",
        "-c",
        "mkfifo \"$0\"; tput smcup; printf done; read _ < \"$0\"",
        gate.to_str().expect("a UTF-8 temp path"),
    ]);
    gutter.wait_for("the child in the alt screen", |s| {
        s.alternate_screen() && s.contents().contains("done")
    });
    std::fs::write(&gate, "\n").expect("release the child");

    // Reaching the end of the stream at all is the no-hang proof.
    let done = gutter.finish();
    assert!(
        !done.screen.alternate_screen(),
        "gutter must leave the alt screen with no keystroke"
    );
    assert_eq!(done.code, Some(0));
    let _ = std::fs::remove_dir_all(gate.parent().expect("the gate's directory"));
}

/// **The child spawns in the launcher's cwd, not `$HOME`.** Set
/// gutter's process `current_dir` to a known temp dir, wrap a `pwd -P`-reporting
/// child, and assert it reports that temp dir — not `$HOME`. Without
/// `builder.cwd(...)`, portable-pty `current_dir($HOME)`s the child, which
/// surfaces as `tig`'s "Not a git repository".
#[test]
fn child_spawns_in_launcher_cwd() {
    let launch_dir = fresh_temp_dir("cwd");
    // `pwd -P` resolves symlinks, matching our canonicalised `launch_dir`.
    let mut gutter = Gutter::spawn_argv_with(&["sh", "-c", "pwd -P; read _"], |cmd| {
        cmd.current_dir(&launch_dir);
    });

    // The rows are joined because a long temp path wraps at the band's 80 columns.
    let expected = launch_dir.to_string_lossy().into_owned();
    gutter.wait_for("the launcher's cwd", |s| {
        s.rows(0, OUTER_COLS).collect::<String>().contains(&expected)
    });

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

    gutter.send(b"\n");
    gutter.finish();
    let _ = std::fs::remove_dir_all(&launch_dir);
}

/// **The child inherits the launcher's environment.** Export a probe variable
/// before launch and wrap a child that echoes it; the child must receive the
/// launcher's value. `CommandBuilder::new` seeds `get_base_env()` from the full
/// launcher env — the guard is against a portable-pty bump silently dropping that.
#[test]
fn child_inherits_launcher_env() {
    const PROBE_VALUE: &str = "gutter-env-probe-value-9173";
    let child = "printf '%s\\n' \"$GUTTER_ENV_PROBE\"; read _";
    let mut gutter = Gutter::spawn_argv_with(&["sh", "-c", child], |cmd| {
        cmd.env("GUTTER_ENV_PROBE", PROBE_VALUE);
    });

    gutter.wait_for("the launcher's env var", |s| s.contents().contains(PROBE_VALUE));
    gutter.send(b"\n");
    gutter.finish();
}
