//! PTY-driven integration tests for the modal resize-mode state machine.
//!
//! These drive the real `gutter` binary through a real PTY and assert on what
//! the outer terminal actually shows — never on gutter internals. The scope here
//! is the width change via the modal keys; the rails are `tests/rails_pty.rs`.
//!
//! `--resize-key ctrl-o` is the harness chord for most of the suite; the default
//! `ctrl-\` (raw `0x1C`) gets one dedicated smoke test so that path is exercised
//! end-to-end too. Under byte matching both are unambiguous.
//!
//! Inside the mode every key is swallowed, so nothing typed there can be echoed
//! back as proof it was handled. The rail at the band's right edge is the signal
//! instead: it is up once the chord is taken, it sits at `band_end` once the steps
//! are, and it is gone once a lone Escape has left the mode. A key typed straight
//! after that Escape would join it as Alt+<key>, so the next key waits for the rail
//! to go.
//!
//! The mode also leaves by itself after `RESIZE_IDLE` (3 s) without a resize key. A
//! test acts on a rail as soon as it sees it, and one that then waits on the child
//! does not need the mode to still be on afterwards, so that limit is only met by a
//! test starved for the whole 3 s.
//!
//! That idle exit takes the rails down too, so the rail going does not show that
//! the lone Escape was what left the mode. Two tests show an Escape leaving it:
//! `esc_exits_mode_key_reaches_child` for the lone byte, by the clock, and
//! `swallowed_key_does_not_leak_and_mode_persists` for the `CSI 27 u` form, which
//! needs no hold and so no clock.
//!
//! CI runs these headlessly: a real PTY, no display, `TERM=xterm-256color`.

use std::time::{Duration, Instant};

mod common;
use common::{cell_text, leave_resize_mode, rails, row_text, Gutter, SIZE_CHILD};

/// Whether the right rail is up at column `col`.
fn rail_at(s: &vt100::Screen, col: u16) -> bool {
    rails(s).1 == Some(col)
}

/// Whether some row of a left-aligned band `width` columns wide holds exactly `text`.
/// Only the band's columns are read: in the mode the rail shares the row.
fn has_row(s: &vt100::Screen, width: u16, text: &str) -> bool {
    s.rows(0, width).any(|r| r.trim_end() == text)
}

/// **`--resize-key ctrl-o` grows the band live.** Send the enter chord then several
/// `l`s; the columns the child reports must grow.
#[test]
fn resize_key_grows_band() {
    let mut gutter = Gutter::spawn(
        160,
        40,
        &format!("--width 60 --left --resize-key ctrl-o {SIZE_CHILD}"),
    );
    gutter.wait_for("the child's launch size, 60 columns", |s| has_row(s, 60, "40 60"));

    // Enter mode (Ctrl-O = 0x0F) then grow by 10 columns (l x10).
    gutter.send(&[0x0F]);
    gutter.send(b"llllllllll");
    gutter.wait_for("the rail at column 70 after 10 x l", |s| rail_at(s, 70));
    // gutter's own width readout says 70 before the child's PTY is resized, so the
    // condition is the child's line.
    gutter.wait_for("the child's own report of 70 columns", |s| {
        has_row(s, 70, "40 70")
    });

    leave_resize_mode(&mut gutter);
    gutter.send(b"\n");
    assert_eq!(gutter.finish().code, Some(0));
}

/// **A held step key keeps stepping under kitty event reporting.** A terminal
/// reporting event types sends a held `l` as one press, repeat reports and a
/// release. The press and each repeat step; the release is swallowed, so the
/// child's tty never echoes a stray report.
#[test]
fn held_step_key_steps_on_kitty_repeats() {
    // The child reports its width when asked rather than from a WINCH trap, so the
    // report is known to come after every key report was handled.
    let child = "/bin/sh -c 'stty size; read _; printf \"END \"; stty size; printf READY; read _'";
    let mut gutter = Gutter::spawn(
        160,
        40,
        &format!("--width 60 --left --resize-key ctrl-o {child}"),
    );
    gutter.wait_for("the child's launch size, 60 columns", |s| has_row(s, 60, "40 60"));

    gutter.send(&[0x0F]);
    gutter.send(b"\x1b[108u");
    for _ in 0..4 {
        gutter.send(b"\x1b[108;1:2u");
    }
    gutter.send(b"\x1b[108;1:3u");
    gutter.wait_for("the rail at column 65 after the press and four repeats", |s| {
        rail_at(s, 65)
    });

    // The Escape is read after the release, and the Enter after the Escape, so the
    // size the child prints next is the one every report above left it with.
    leave_resize_mode(&mut gutter);
    gutter.send(b"\n");
    gutter.wait_for("the child's READY", |s| s.contents().contains("READY"));
    gutter.send(b"\n");

    let done = gutter.finish();
    let lines: Vec<String> = done
        .screen
        .rows(0, 65)
        .map(|r| r.trim_end().to_string())
        .filter(|r| !r.is_empty())
        .collect();
    assert!(
        lines.iter().any(|l| l == "END 40 65"),
        "the press and four repeats step once each and the release not at all: the child \
         must report 65 columns, got {lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("108")),
        "no report reaches the child, got {lines:?}"
    );
    // The child's tty is cooked: a chord byte that reached it was echoed as `^O`.
    assert!(
        !lines.iter().any(|l| l.contains("^O")),
        "the chord byte must not reach the child, got {lines:?}"
    );
}

/// **Shrink narrows the band.** Grow then shrink back down with `H`; the child's
/// reported columns must decrease.
#[test]
fn resize_key_shrinks_band() {
    let mut gutter = Gutter::spawn(
        160,
        40,
        &format!("--width 60 --left --resize-key ctrl-o {SIZE_CHILD}"),
    );
    gutter.wait_for("the child's launch size, 60 columns", |s| has_row(s, 60, "40 60"));

    gutter.send(&[0x0F]);
    // Grow by 20, then shrink by 30 — net -10 from the start.
    gutter.send(b"llllllllllllllllllll");
    gutter.wait_for("the rail at column 80 after 20 x l", |s| rail_at(s, 80));
    gutter.send(b"HHH");
    gutter.wait_for("the rail at column 50 after +20 then -30 (H x3)", |s| {
        rail_at(s, 50)
    });
    gutter.wait_for("the child's own report of 50 columns", |s| {
        has_row(s, 50, "40 50")
    });

    leave_resize_mode(&mut gutter);
    gutter.send(b"\n");
    assert_eq!(gutter.finish().code, Some(0));
}

/// **`Esc` exits the mode and releases the key to the child.** Enter, `Esc`,
/// then type a line; `cat` must write it back (the mode really released the keys,
/// it did not swallow them as stray in-mode keys).
#[test]
fn esc_exits_mode_key_reaches_child() {
    let mut gutter = Gutter::spawn(120, 40, "--width 60 --resize-key ctrl-o /bin/cat");

    let entered = Instant::now();
    gutter.send(&[0x0F]); // enter
    gutter.wait_for("the rail at column 90", |s| rail_at(s, 90));
    leave_resize_mode(&mut gutter);
    // Exposed to RESIZE_IDLE: the mode leaves by itself 3 s after the chord, so only
    // rails that went sooner than that were cleared by the Escape.
    assert!(
        entered.elapsed() < Duration::from_secs(3),
        "the rails went, but no sooner than the idle exit would have cleared them"
    );

    gutter.send(b"MARKER_AFTER_ESC\r");
    // The tty echoes the line as it is typed; the second copy is cat's own.
    gutter.wait_for("cat's copy of the line typed after Esc", |s| {
        s.contents().matches("MARKER_AFTER_ESC").count() == 2
    });

    // Ctrl-D on an empty line ends `cat`.
    gutter.send(b"\x04");
    assert_eq!(gutter.finish().code, Some(0));
}

/// **The default chord (`Ctrl-\`) enters via the raw legacy byte `0x1C`.** One
/// dedicated smoke test for the default-chord path; the rest of the suite uses
/// `ctrl-o`.
#[test]
fn default_chord_enters_via_raw_fs_byte() {
    let mut gutter = Gutter::spawn(160, 40, &format!("--width 60 --left {SIZE_CHILD}"));
    gutter.wait_for("the child's launch size, 60 columns", |s| has_row(s, 60, "40 60"));

    gutter.send(&[0x1c]); // raw Ctrl-\ (FS)
    gutter.send(b"llllllllll"); // grow by 10
    gutter.wait_for(
        "the rail at column 70: the default Ctrl-\\ chord (raw 0x1C) must have entered \
         the mode and grown the band",
        |s| rail_at(s, 70),
    );
    gutter.wait_for("the child's own report of 70 columns", |s| {
        has_row(s, 70, "40 70")
    });

    leave_resize_mode(&mut gutter);
    gutter.send(b"\n");
    let done = gutter.finish();
    assert_eq!(
        row_text(&done.screen, 0),
        "40 60",
        "the launch line must still be painted after the grow"
    );
}

/// A stray key while in mode is swallowed, not leaked to the child, and the
/// mode stays active — a following `Esc` still leaves it.
///
/// The Escape is the `CSI 27 u` report a terminal in a relayed keyboard mode sends.
/// Unlike the lone byte it is complete as it stands, so nothing is held and the key
/// after it can follow at once. The three go in one write, which leaves the idle
/// exit no part: with an Escape that did nothing the second `z` is swallowed as
/// well, `cat` is never sent a line, and the wait below runs out.
#[test]
fn swallowed_key_does_not_leak_and_mode_persists() {
    let mut gutter = Gutter::spawn(120, 40, "--width 60 --resize-key ctrl-o /bin/cat");

    gutter.send(&[0x0F]); // enter
    gutter.wait_for("the rail at column 90", |s| rail_at(s, 90));
    // An unrecognised in-mode key, the Escape, and a line that passes through to cat.
    gutter.send(b"z\x1b[27uz\r");
    // The tty echoes what was typed on row 0 and cat writes what it read on row 1.
    // The band starts at column 30; the rails may still share the row.
    gutter.wait_for("cat's copy of the line typed after Esc", |s| {
        cell_text(s, 1, 30) == "z"
    });

    // Ctrl-D on an empty line ends `cat`.
    gutter.send(b"\x04");
    let done = gutter.finish();
    let lines = [row_text(&done.screen, 0), row_text(&done.screen, 1)];
    assert_eq!(
        lines.each_ref().map(|l| l.trim()),
        ["z", "z"],
        "the in-mode 'z' must be swallowed; only the post-Esc 'z' reaches cat"
    );
}
