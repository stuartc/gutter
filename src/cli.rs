//! Command-line parsing.
//!
//! `gutter [--width <N|Npct|full>] [--center|--left] [--resize-key <chord>]
//! <cmd> [args...]`, or `gutter --version`. The first non-flag positional is
//! the command, the rest are its arguments — a hand-rolled split of
//! `std::env::args`, no clap.

use crate::geometry::{Layout, Width};
use crate::chord::{parse_chord, Chord};

/// The build stamp: `git describe --tags --dirty --always` where the build had a
/// checkout to describe, else the crate version. The `-dirty` suffix is
/// best-effort — see `build.rs`.
pub const VERSION: &str = env!("GUTTER_VERSION");

/// What the argument list asked for: a child run, or the version stamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    Run(Config),
    /// `--version`: print [`VERSION`] and exit 0, spawning nothing.
    Version,
}

/// Everything a child run needs: the band width, the alignment, the
/// resize-mode chord, and the command to spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The requested band width from `--width`, or `None` when the flag was
    /// omitted. `None` is resolved to the built-in default in `main::run`
    /// (currently `Width::Cols(100)`); it is NOT passthrough. Passthrough is an
    /// explicit `--width full` / `--width 100%` (`Some(Width::Percent(100))`).
    pub width: Option<Width>,
    /// Defaults to [`Layout::Center`].
    pub layout: Layout,
    /// The resize-mode enter chord (`--resize-key`). Defaults to Ctrl-\.
    pub resize_key: Chord,
    pub cmd: String,
    pub args: Vec<String>,
}

/// Parses `gutter [--width <N|Npct|full>] [--center|--left] <cmd> [args...]` from an
/// argument iterator (excluding argv[0]).
///
/// `--version` short-circuits to [`Invocation::Version`], so it needs no command.
///
/// Flags are only recognised before the command; once the command is seen,
/// everything that follows is the child's own argument (so
/// `gutter vim --width` passes `--width` to vim).
///
/// Returns `Err` with a usage message on a missing/invalid value or no command.
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Invocation, String> {
    let mut iter = args.into_iter().peekable();
    let mut width: Option<Width> = None;
    let mut layout: Option<Layout> = None;
    let mut resize_key: Option<Chord> = None;

    // Leading flags, terminated by the first non-flag (the command).
    while let Some(arg) = iter.peek() {
        if arg == "--width" {
            iter.next();
            let val = iter
                .next()
                .ok_or_else(|| "gutter: --width needs a value".to_string())?;
            width = Some(parse_width(&val)?);
        } else if let Some(val) = arg.strip_prefix("--width=") {
            let val = val.to_string();
            iter.next();
            width = Some(parse_width(&val)?);
        } else if arg == "--version" {
            return Ok(Invocation::Version);
        } else if arg == "--center" || arg == "--centre" {
            iter.next();
            layout = Some(Layout::Center);
        } else if arg == "--left" {
            iter.next();
            layout = Some(Layout::Left);
        } else if arg == "--resize-key" {
            iter.next();
            let val = iter
                .next()
                .ok_or_else(|| "gutter: --resize-key needs a value".to_string())?;
            resize_key = Some(parse_chord(&val)?);
        } else if let Some(val) = arg.strip_prefix("--resize-key=") {
            let val = val.to_string();
            iter.next();
            resize_key = Some(parse_chord(&val)?);
        } else {
            break;
        }
    }

    let cmd = iter.next().ok_or_else(usage)?;

    Ok(Invocation::Run(Config {
        width,
        layout: layout.unwrap_or_default(),
        resize_key: resize_key.unwrap_or_default(),
        cmd,
        args: iter.collect(),
    }))
}

fn usage() -> String {
    "usage: gutter [--width <N|Npct|full>] [--center|--left] [--resize-key <chord>] <cmd> [args...]\n       gutter --version"
        .to_string()
}

/// Parses a `--width` value into a [`Width`]: a bare integer is absolute
/// ([`Width::Cols`]); an integer with a `pct` or `%` suffix is proportional
/// ([`Width::Percent`]). `pct` is the documented spelling, `%` an accepted
/// alias; the bare word `full` is an alias for `100%` (full-width passthrough).
/// See ADR-011.
pub(crate) fn parse_width(s: &str) -> Result<Width, String> {
    if s == "full" {
        return Ok(Width::Percent(100));
    }
    if let Some(digits) = s.strip_suffix("pct").or_else(|| s.strip_suffix('%')) {
        let p: u8 = digits
            .parse()
            .map_err(|_| format!("gutter: invalid --width percentage '{s}'"))?;
        if p == 0 || p > 100 {
            return Err(format!("gutter: --width percentage must be 1..=100, got '{s}'"));
        }
        Ok(Width::Percent(p))
    } else {
        let n: u16 = s
            .parse()
            .map_err(|_| format!("gutter: invalid --width value '{s}'"))?;
        if n < 1 {
            return Err("gutter: --width must be at least 1".to_string());
        }
        Ok(Width::Cols(n))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn cfg(args: Vec<String>) -> Result<Config, String> {
        match parse(args)? {
            Invocation::Run(cfg) => Ok(cfg),
            Invocation::Version => panic!("expected a child run, got --version"),
        }
    }

    #[test]
    fn parses_cmd_and_args() {
        let c = cfg(v(&["echo", "hi", "there"])).unwrap();
        assert_eq!(c.width, None);
        assert_eq!(c.layout, Layout::Center); // default
        assert_eq!(c.cmd, "echo");
        assert_eq!(c.args, v(&["hi", "there"]));
    }

    #[test]
    fn parses_bare_cmd() {
        let c = cfg(v(&["cat"])).unwrap();
        assert_eq!(c.cmd, "cat");
        assert!(c.args.is_empty());
    }

    #[test]
    fn parses_absolute_width_flag() {
        let c = cfg(v(&["--width", "100", "vim", "file"])).unwrap();
        assert_eq!(c.width, Some(Width::Cols(100)));
        assert_eq!(c.cmd, "vim");
        assert_eq!(c.args, v(&["file"]));
    }

    #[test]
    fn parses_width_equals_form() {
        let c = cfg(v(&["--width=80", "echo"])).unwrap();
        assert_eq!(c.width, Some(Width::Cols(80)));
    }

    #[test]
    fn parses_proportional_width_pct() {
        let c = cfg(v(&["--width", "50pct", "echo"])).unwrap();
        assert_eq!(c.width, Some(Width::Percent(50)));
    }

    #[test]
    fn parses_proportional_width_percent_alias() {
        let c = cfg(v(&["--width", "50%", "echo"])).unwrap();
        assert_eq!(c.width, Some(Width::Percent(50)));
        let c = cfg(v(&["--width=33%", "echo"])).unwrap();
        assert_eq!(c.width, Some(Width::Percent(33)));
    }

    #[test]
    fn parses_center_flag() {
        let c = cfg(v(&["--center", "echo"])).unwrap();
        assert_eq!(c.layout, Layout::Center);
        // British spelling accepted too.
        let c = cfg(v(&["--centre", "echo"])).unwrap();
        assert_eq!(c.layout, Layout::Center);
    }

    #[test]
    fn parses_left_flag() {
        let c = cfg(v(&["--left", "echo"])).unwrap();
        assert_eq!(c.layout, Layout::Left);
    }

    #[test]
    fn parses_width_and_alignment_together() {
        let c = cfg(v(&["--width", "100", "--center", "claude"])).unwrap();
        assert_eq!(c.width, Some(Width::Cols(100)));
        assert_eq!(c.layout, Layout::Center);
        assert_eq!(c.cmd, "claude");

        let c = cfg(v(&["--left", "--width=50pct", "claude"])).unwrap();
        assert_eq!(c.width, Some(Width::Percent(50)));
        assert_eq!(c.layout, Layout::Left);
    }

    #[test]
    fn flags_after_command_are_child_args() {
        let c = cfg(v(&["vim", "--width", "100", "--center"])).unwrap();
        assert_eq!(c.width, None);
        assert_eq!(c.layout, Layout::Center);
        assert_eq!(c.cmd, "vim");
        assert_eq!(c.args, v(&["--width", "100", "--center"]));
    }

    #[test]
    fn rejects_missing_width_value() {
        assert!(cfg(v(&["--width"])).is_err());
    }

    #[test]
    fn rejects_non_numeric_width() {
        assert!(cfg(v(&["--width", "wide", "echo"])).is_err());
    }

    #[test]
    fn rejects_bad_percentage() {
        assert!(cfg(v(&["--width", "0pct", "echo"])).is_err());
        assert!(cfg(v(&["--width", "101pct", "echo"])).is_err());
        assert!(cfg(v(&["--width", "abcpct", "echo"])).is_err());
    }

    #[test]
    fn rejects_empty() {
        assert!(cfg(v(&[])).is_err());
    }

    #[test]
    fn default_resize_key_is_ctrl_backslash() {
        let c = cfg(v(&["echo"])).unwrap();
        assert_eq!(c.resize_key, Chord::default());
    }

    #[test]
    fn parses_resize_key_flag() {
        let c = cfg(v(&["--resize-key", "ctrl-g", "echo"])).unwrap();
        assert_eq!(
            c.resize_key,
            crate::chord::parse_chord("ctrl-g").unwrap()
        );
        let c = cfg(v(&["--resize-key=ctrl-o", "echo"])).unwrap();
        assert_eq!(
            c.resize_key,
            crate::chord::parse_chord("ctrl-o").unwrap()
        );
    }

    #[test]
    fn rejects_bad_resize_key() {
        assert!(cfg(v(&["--resize-key", "wat-x", "echo"])).is_err());
        assert!(cfg(v(&["--resize-key"])).is_err());
    }

    #[test]
    fn resize_key_after_command_is_child_arg() {
        let c = cfg(v(&["vim", "--resize-key", "ctrl-g"])).unwrap();
        assert_eq!(c.resize_key, Chord::default());
        assert_eq!(c.cmd, "vim");
        assert_eq!(c.args, v(&["--resize-key", "ctrl-g"]));
    }

    #[test]
    fn parses_full_literal() {
        let c = cfg(v(&["--width", "full", "echo"])).unwrap();
        assert_eq!(c.width, Some(Width::Percent(100)));
        let c = cfg(v(&["--width=full", "echo"])).unwrap();
        assert_eq!(c.width, Some(Width::Percent(100)));
    }

    #[test]
    fn full_equals_percent_100() {
        let full = cfg(v(&["--width", "full", "echo"])).unwrap();
        let pct = cfg(v(&["--width", "100%", "echo"])).unwrap();
        assert_eq!(full.width, pct.width);
    }

    #[test]
    fn parses_version_flag() {
        assert_eq!(parse(v(&["--version"])).unwrap(), Invocation::Version);
        // Still an early exit with other leading flags in front of it.
        assert_eq!(
            parse(v(&["--width", "80", "--version"])).unwrap(),
            Invocation::Version
        );
    }

    #[test]
    fn version_after_command_is_child_arg() {
        let c = cfg(v(&["vim", "--version"])).unwrap();
        assert_eq!(c.cmd, "vim");
        assert_eq!(c.args, v(&["--version"]));
    }

    #[test]
    fn rejects_capitalised_full() {
        assert!(cfg(v(&["--width", "Full", "echo"])).is_err());
        assert!(cfg(v(&["--width", "FULL", "echo"])).is_err());
    }
}
