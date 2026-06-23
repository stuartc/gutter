//! Command-line parsing.
//!
//! `gutter [--width <N>] <cmd> [args...]`. The first non-flag positional is the
//! command, the rest are its arguments — a hand-rolled split of
//! `std::env::args`, no clap.
//!
//! `--width <N>` is **absolute only** in this slice: a fixed column count `W`
//! for the whole session (`W` never changes on resize). The proportional
//! `--width <N>pct` form and the `--center`/`--left` flag (with the centred
//! margin recompute-on-resize) land in slice 05 (ADR-011); they are not parsed
//! here yet. When `--width` is omitted, `W` defaults to the real terminal width
//! at startup, so gutter behaves as a transparent passthrough.

/// The minimum band width. A band narrower than this is not useful, so an
/// explicit `--width` below it is rejected rather than silently clamped.
pub const MIN_WIDTH: u16 = 1;

/// The parsed invocation: the band width and the child command + arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The absolute band width `W`, or `None` to default to the real terminal
    /// width at startup.
    pub width: Option<u16>,
    pub cmd: String,
    pub args: Vec<String>,
}

/// Parse `gutter [--width <N>] <cmd> [args...]` from an argument iterator
/// (excluding argv[0]).
///
/// Flags are only recognised before the command; once the command is seen,
/// everything that follows is the child's own argument (so
/// `gutter vim --width` passes `--width` to vim).
///
/// Returns `Err` with a usage message on a missing/invalid width or no command.
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Config, String> {
    let mut iter = args.into_iter().peekable();
    let mut width: Option<u16> = None;

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
        } else {
            break;
        }
    }

    let cmd = iter.next().ok_or_else(usage)?;

    Ok(Config {
        width,
        cmd,
        args: iter.collect(),
    })
}

fn usage() -> String {
    "usage: gutter [--width <N>] <cmd> [args...]".to_string()
}

/// Parse an absolute width. The proportional `Npct`/`N%` form is slice 05, so a
/// trailing `pct`/`%` is rejected here with a pointer rather than mis-parsed.
fn parse_width(s: &str) -> Result<u16, String> {
    if s.ends_with("pct") || s.ends_with('%') {
        return Err("gutter: proportional --width (Npct) is not supported yet".to_string());
    }
    let n: u16 = s
        .parse()
        .map_err(|_| format!("gutter: invalid --width value '{s}'"))?;
    if n < MIN_WIDTH {
        return Err(format!("gutter: --width must be at least {MIN_WIDTH}"));
    }
    Ok(n)
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
    fn parses_width_flag() {
        let cfg = parse(v(&["--width", "100", "vim", "file"])).unwrap();
        assert_eq!(cfg.width, Some(100));
        assert_eq!(cfg.cmd, "vim");
        assert_eq!(cfg.args, v(&["file"]));
    }

    #[test]
    fn parses_width_equals_form() {
        let cfg = parse(v(&["--width=80", "echo"])).unwrap();
        assert_eq!(cfg.width, Some(80));
        assert_eq!(cfg.cmd, "echo");
    }

    #[test]
    fn width_after_command_is_child_arg() {
        let cfg = parse(v(&["vim", "--width", "100"])).unwrap();
        assert_eq!(cfg.width, None);
        assert_eq!(cfg.cmd, "vim");
        assert_eq!(cfg.args, v(&["--width", "100"]));
    }

    #[test]
    fn rejects_proportional_width_for_now() {
        assert!(parse(v(&["--width", "50pct", "echo"])).is_err());
        assert!(parse(v(&["--width", "50%", "echo"])).is_err());
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
    fn rejects_empty() {
        assert!(parse(v(&[])).is_err());
    }
}
