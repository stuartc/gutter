//! Command-line parsing.
//!
//! `gutter [--width <N|Npct|full>] [--center|--left] <cmd> [args...]`. The first
//! non-flag positional is the command, the rest are its arguments — a
//! hand-rolled split of `std::env::args`, no clap.

use crate::geometry::{Layout, Width};

/// The parsed invocation: the band width, the alignment, and the child command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The requested band width from `--width`, or `None` when the flag was
    /// omitted. `None` is resolved to the built-in default in `main::run`
    /// (currently `Width::Cols(100)`); it is NOT passthrough. Passthrough is an
    /// explicit `--width full` / `--width 100%` (`Some(Width::Percent(100))`).
    pub width: Option<Width>,
    /// Defaults to [`Layout::Center`].
    pub layout: Layout,
    pub cmd: String,
    pub args: Vec<String>,
}

/// Parses `gutter [--width <N|Npct|full>] [--center|--left] <cmd> [args...]` from an
/// argument iterator (excluding argv[0]).
///
/// Flags are only recognised before the command; once the command is seen,
/// everything that follows is the child's own argument (so
/// `gutter vim --width` passes `--width` to vim).
///
/// Returns `Err` with a usage message on a missing/invalid value or no command.
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Config, String> {
    let mut iter = args.into_iter().peekable();
    let mut width: Option<Width> = None;
    let mut layout: Option<Layout> = None;

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
        } else if arg == "--center" || arg == "--centre" {
            iter.next();
            layout = Some(Layout::Center);
        } else if arg == "--left" {
            iter.next();
            layout = Some(Layout::Left);
        } else {
            break;
        }
    }

    let cmd = iter.next().ok_or_else(usage)?;

    Ok(Config {
        width,
        layout: layout.unwrap_or_default(),
        cmd,
        args: iter.collect(),
    })
}

fn usage() -> String {
    "usage: gutter [--width <N|Npct|full>] [--center|--left] <cmd> [args...]".to_string()
}

/// Parses a `--width` value into a [`Width`]: a bare integer is absolute
/// ([`Width::Cols`]); an integer with a `pct` or `%` suffix is proportional
/// ([`Width::Percent`]). `pct` is the documented spelling, `%` an accepted
/// alias; the bare word `full` is an alias for `100%` (full-width passthrough).
/// See ADR-011.
fn parse_width(s: &str) -> Result<Width, String> {
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

    #[test]
    fn parses_cmd_and_args() {
        let cfg = parse(v(&["echo", "hi", "there"])).unwrap();
        assert_eq!(cfg.width, None);
        assert_eq!(cfg.layout, Layout::Center); // default
        assert_eq!(cfg.cmd, "echo");
        assert_eq!(cfg.args, v(&["hi", "there"]));
    }

    #[test]
    fn parses_bare_cmd() {
        let cfg = parse(v(&["cat"])).unwrap();
        assert_eq!(cfg.cmd, "cat");
        assert!(cfg.args.is_empty());
    }

    #[test]
    fn parses_absolute_width_flag() {
        let cfg = parse(v(&["--width", "100", "vim", "file"])).unwrap();
        assert_eq!(cfg.width, Some(Width::Cols(100)));
        assert_eq!(cfg.cmd, "vim");
        assert_eq!(cfg.args, v(&["file"]));
    }

    #[test]
    fn parses_width_equals_form() {
        let cfg = parse(v(&["--width=80", "echo"])).unwrap();
        assert_eq!(cfg.width, Some(Width::Cols(80)));
    }

    #[test]
    fn parses_proportional_width_pct() {
        let cfg = parse(v(&["--width", "50pct", "echo"])).unwrap();
        assert_eq!(cfg.width, Some(Width::Percent(50)));
    }

    #[test]
    fn parses_proportional_width_percent_alias() {
        let cfg = parse(v(&["--width", "50%", "echo"])).unwrap();
        assert_eq!(cfg.width, Some(Width::Percent(50)));
        let cfg = parse(v(&["--width=33%", "echo"])).unwrap();
        assert_eq!(cfg.width, Some(Width::Percent(33)));
    }

    #[test]
    fn parses_center_flag() {
        let cfg = parse(v(&["--center", "echo"])).unwrap();
        assert_eq!(cfg.layout, Layout::Center);
        // British spelling accepted too.
        let cfg = parse(v(&["--centre", "echo"])).unwrap();
        assert_eq!(cfg.layout, Layout::Center);
    }

    #[test]
    fn parses_left_flag() {
        let cfg = parse(v(&["--left", "echo"])).unwrap();
        assert_eq!(cfg.layout, Layout::Left);
    }

    #[test]
    fn parses_width_and_alignment_together() {
        let cfg = parse(v(&["--width", "100", "--center", "claude"])).unwrap();
        assert_eq!(cfg.width, Some(Width::Cols(100)));
        assert_eq!(cfg.layout, Layout::Center);
        assert_eq!(cfg.cmd, "claude");

        let cfg = parse(v(&["--left", "--width=50pct", "claude"])).unwrap();
        assert_eq!(cfg.width, Some(Width::Percent(50)));
        assert_eq!(cfg.layout, Layout::Left);
    }

    #[test]
    fn flags_after_command_are_child_args() {
        let cfg = parse(v(&["vim", "--width", "100", "--center"])).unwrap();
        assert_eq!(cfg.width, None);
        assert_eq!(cfg.layout, Layout::Center);
        assert_eq!(cfg.cmd, "vim");
        assert_eq!(cfg.args, v(&["--width", "100", "--center"]));
    }

    #[test]
    fn rejects_missing_width_value() {
        assert!(parse(v(&["--width"])).is_err());
    }

    #[test]
    fn rejects_non_numeric_width() {
        assert!(parse(v(&["--width", "wide", "echo"])).is_err());
    }

    #[test]
    fn rejects_bad_percentage() {
        assert!(parse(v(&["--width", "0pct", "echo"])).is_err());
        assert!(parse(v(&["--width", "101pct", "echo"])).is_err());
        assert!(parse(v(&["--width", "abcpct", "echo"])).is_err());
    }

    #[test]
    fn rejects_empty() {
        assert!(parse(v(&[])).is_err());
    }

    #[test]
    fn parses_full_literal() {
        let cfg = parse(v(&["--width", "full", "echo"])).unwrap();
        assert_eq!(cfg.width, Some(Width::Percent(100)));
        let cfg = parse(v(&["--width=full", "echo"])).unwrap();
        assert_eq!(cfg.width, Some(Width::Percent(100)));
    }

    #[test]
    fn full_equals_percent_100() {
        let full = parse(v(&["--width", "full", "echo"])).unwrap();
        let pct = parse(v(&["--width", "100%", "echo"])).unwrap();
        assert_eq!(full.width, pct.width);
    }

    #[test]
    fn rejects_capitalised_full() {
        assert!(parse(v(&["--width", "Full", "echo"])).is_err());
        assert!(parse(v(&["--width", "FULL", "echo"])).is_err());
    }
}
