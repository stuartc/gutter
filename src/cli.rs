//! Command-line parsing.
//!
//! Slice 01 is deliberately minimal: `gutter <cmd> [args...]`. The first
//! positional is the command, the rest are its arguments — a hand-rolled split
//! of `std::env::args`, no clap. The `--width <N|Npct>` and
//! `[--center|--left]` flags (and the `width` module that interprets them)
//! land in slices 02 and 05; they are not parsed here yet.

/// The parsed invocation: the child command and its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub cmd: String,
    pub args: Vec<String>,
}

/// Parse `gutter <cmd> [args...]` from an argument iterator (excluding argv[0]).
///
/// Returns `Err` with a usage message if no command is given.
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Config, String> {
    let mut iter = args.into_iter();
    let cmd = iter
        .next()
        .ok_or_else(|| "usage: gutter <cmd> [args...]".to_string())?;
    Ok(Config {
        cmd,
        args: iter.collect(),
    })
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
    fn rejects_empty() {
        assert!(parse(v(&[])).is_err());
    }
}
