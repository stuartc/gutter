//! gutter talks to exactly one terminal: its controlling terminal. The band, the
//! keyboard and the startup CPR probe all reach it through `/dev/tty`, so stdout is
//! never written and a redirect never captures the band — the tmux behaviour. The
//! same open is the guard: no controlling terminal, no run.

mod common;

use std::fs;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use common::{
    answer_cpr, drain_window, grid_text, outer_grid, pty_guard, spawn_gutter, spawn_gutter_probing,
};

/// A fresh path under the system temp dir, unique per process and tag. Not created —
/// the caller (or gutter's own redirect) makes it.
fn temp_path(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("gutter-tty-{tag}-{}", std::process::id()))
}

/// With no controlling terminal there is nothing to render a band on: gutter prints
/// one line to stderr, exits 1, and never gets as far as spawning the child.
///
/// `setsid` in the forked child is what arranges that — it leaves the session (and so
/// the controlling terminal) the test binary inherited from whoever ran `cargo test`.
/// A fresh `Command` child is never a process-group leader, so the call cannot fail.
#[test]
fn no_controlling_terminal_refuses_before_spawning_the_child() {
    let marker = temp_path("marker");
    let _ = fs::remove_file(&marker);

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gutter"));
    cmd.args(["sh", "-c", &format!("touch {}", marker.display())]);
    cmd.env("TERM", "xterm-256color");
    cmd.env_remove("GUTTER_FORCE_ANCHOR_ROW");
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    // SAFETY: `setsid` is async-signal-safe and touches nothing the child allocated.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let out = cmd.output().expect("run gutter with no controlling terminal");

    assert_eq!(out.status.code(), Some(1), "refusal must exit 1");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stderr.lines().count(),
        1,
        "the refusal must be one line, got {stderr:?}"
    );
    assert!(
        stderr.contains("no controlling terminal"),
        "the refusal must name the cause, got {stderr:?}"
    );
    assert!(
        out.stdout.is_empty(),
        "nothing may reach stdout — not the band, not the CPR query: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        !marker.exists(),
        "the guard must refuse before the child is spawned"
    );

    let _ = fs::remove_file(&marker);
}

/// `gutter cmd > log` paints on the screen and leaves the log empty: the band goes to
/// the controlling terminal, and the child's output is not teed into the redirect.
#[test]
fn a_redirect_captures_nothing_and_the_band_still_paints() {
    let _guard = pty_guard();
    let log = temp_path("redirect");
    let _ = fs::remove_file(&log);

    let mut session = spawn_gutter(
        80,
        24,
        &format!(
            "--width 40 sh -c 'printf hi-from-the-band; sleep 0.5' > {}",
            log.display()
        ),
    );
    let out = drain_window(&mut session, Duration::from_secs(2));
    drop(session);

    let text = grid_text(&out, 80, 24);
    assert!(
        text.contains("hi-from-the-band"),
        "the band must paint on the terminal even with stdout redirected: {text:?}"
    );
    let captured = fs::read(&log).expect("the redirect target must exist");
    assert!(
        captured.is_empty(),
        "the redirect must capture nothing, got {:?}",
        String::from_utf8_lossy(&captured)
    );

    let _ = fs::remove_file(&log);
}

/// The startup probe's query leaves by the terminal, not stdout — so it still gets an
/// answer under a redirect, and the band anchors where the terminal said rather than
/// falling back after a silent timeout.
#[test]
fn the_cpr_query_survives_a_redirect() {
    let _guard = pty_guard();
    let log = temp_path("probe");
    let _ = fs::remove_file(&log);
    const PROBED_ROW: u16 = 10;

    let mut session = spawn_gutter_probing(
        80,
        24,
        &format!(
            "--width 40 --left sh -c 'printf probed; sleep 0.5' > {}",
            log.display()
        ),
    );
    // Panics unless `ESC[6n` arrives on the PTY: proof the query took the terminal.
    answer_cpr(&mut session, PROBED_ROW, Duration::from_secs(5));
    let out = drain_window(&mut session, Duration::from_secs(2));
    drop(session);

    let parser = outer_grid(&out, 80, 24);
    let rows: Vec<String> = parser.screen().rows(0, 80).collect();
    assert_eq!(
        rows.iter().position(|r| r.contains("probed")),
        Some(PROBED_ROW as usize - 1),
        "the band must anchor at the probed row, not the rows-1 fallback; rows: {rows:?}"
    );
    let mut captured = Vec::new();
    fs::File::open(&log)
        .expect("the redirect target must exist")
        .read_to_end(&mut captured)
        .expect("read the redirect target");
    assert!(
        captured.is_empty(),
        "neither the query nor the band may reach the redirect, got {:?}",
        String::from_utf8_lossy(&captured)
    );

    let _ = fs::remove_file(&log);
}
