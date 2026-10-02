//! Session recording: what the render loop took in, written one event per line
//! to the file `GUTTER_RECORD` names, so a later replay can run the same inputs
//! back through the production render path.
//!
//! The file is plain text. The first line is the version marker; every line
//! after it is `<ms> <kind> <fields>`, where `<ms>` is the loop clock's
//! [`Clock::elapsed_ms`](crate::clock::Clock::elapsed_ms):
//!
//! ```text
//! gutter-record 1
//! 0 start <cols> <rows> <base_row> <width: N | N%> <center | left>
//! 3 out <child bytes, `escape_ascii`-escaped>
//! 250 resize <cols> <rows>
//! 900 mode <on | off>
//! 950 step <delta>
//! ```
//!
//! `mode` and `step` are resize mode (ADR-016): entering and leaving it, and one
//! width nudge in the width's own unit with the real size held fixed.

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

use crate::geometry::{Layout, Width};

const HEADER: &str = "gutter-record 1";

/// One thing the render loop took in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The geometry the run started from, enough to resolve `W` and the margin
    /// through `geometry` again.
    Start {
        cols: u16,
        rows: u16,
        base_row: u16,
        width: Width,
        layout: Layout,
    },
    /// One `Msg::Pty` chunk, exactly as received.
    Output(Vec<u8>),
    /// The physical size read while handling a `Msg::Resize`.
    Resize { cols: u16, rows: u16 },
    /// Resize mode entered (`true`) or left (`false`).
    ResizeMode(bool),
    /// One resize-mode width step.
    Step(i32),
}

pub struct Recorder {
    // Unbuffered on purpose: gutter leaves through `process::exit`, which runs
    // no destructors (ADR-010), so a buffered tail would be lost. Each event is
    // one `write` straight to the file.
    file: File,
}

impl Recorder {
    pub fn create(path: &Path) -> io::Result<Self> {
        let mut file = File::create(path)?;
        writeln!(file, "{HEADER}")?;
        Ok(Self { file })
    }

    pub fn record(&mut self, at_ms: u64, event: &Event) {
        let body = match event {
            Event::Start {
                cols,
                rows,
                base_row,
                width,
                layout,
            } => {
                let width = match width {
                    Width::Cols(n) => n.to_string(),
                    Width::Percent(p) => format!("{p}%"),
                };
                let layout = match layout {
                    Layout::Center => "center",
                    Layout::Left => "left",
                };
                format!("start {cols} {rows} {base_row} {width} {layout}")
            }
            Event::Output(bytes) => format!("out {}", bytes.escape_ascii()),
            Event::Resize { cols, rows } => format!("resize {cols} {rows}"),
            Event::ResizeMode(on) => format!("mode {}", if *on { "on" } else { "off" }),
            Event::Step(delta) => format!("step {delta}"),
        };
        let _ = self.file.write_all(format!("{at_ms} {body}\n").as_bytes());
    }
}

/// Read a recording back as `(ms, event)` pairs, in file order.
#[cfg(test)]
pub fn parse(text: &str) -> Result<Vec<(u64, Event)>, String> {
    let mut lines = text.lines();
    match lines.next() {
        Some(HEADER) => {}
        other => return Err(format!("not a recording: first line is {other:?}")),
    }
    lines
        .enumerate()
        .map(|(i, line)| parse_line(line).map_err(|e| format!("line {}: {e}: {line:?}", i + 2)))
        .collect()
}

#[cfg(test)]
fn parse_line(line: &str) -> Result<(u64, Event), String> {
    fn num<T: std::str::FromStr>(field: Option<&str>) -> Result<T, String> {
        field
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| "bad or missing number".to_string())
    }

    let (at, rest) = line.split_once(' ').ok_or("no timestamp")?;
    let at: u64 = num(Some(at))?;
    // The payload of `out` is everything after the one separating space, spaces
    // included; an empty chunk has no separator at all.
    let (kind, fields) = rest.split_once(' ').unwrap_or((rest, ""));
    let mut f = fields.split(' ');
    let event = match kind {
        "start" => Event::Start {
            cols: num(f.next())?,
            rows: num(f.next())?,
            base_row: num(f.next())?,
            width: crate::cli::parse_width(f.next().ok_or("no width")?)?,
            layout: match f.next() {
                Some("center") => Layout::Center,
                Some("left") => Layout::Left,
                other => return Err(format!("unknown layout {other:?}")),
            },
        },
        "out" => Event::Output(unescape(fields)?),
        "resize" => Event::Resize {
            cols: num(f.next())?,
            rows: num(f.next())?,
        },
        "mode" => Event::ResizeMode(match fields {
            "on" => true,
            "off" => false,
            other => return Err(format!("unknown mode {other:?}")),
        }),
        "step" => Event::Step(num(f.next())?),
        other => return Err(format!("unknown event {other:?}")),
    };
    Ok((at, event))
}

/// The inverse of `<[u8]>::escape_ascii`.
#[cfg(test)]
fn unescape(s: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        if b != b'\\' {
            out.push(b);
            continue;
        }
        out.push(match bytes.next() {
            Some(b'n') => b'\n',
            Some(b'r') => b'\r',
            Some(b't') => b'\t',
            Some(c @ (b'\\' | b'\'' | b'"')) => c,
            Some(b'x') => {
                let hex = [bytes.next(), bytes.next()];
                match hex {
                    [Some(hi), Some(lo)] => std::str::from_utf8(&[hi, lo])
                        .ok()
                        .and_then(|h| u8::from_str_radix(h, 16).ok())
                        .ok_or("bad \\x escape")?,
                    _ => return Err("truncated \\x escape".to_string()),
                }
            }
            other => return Err(format!("unknown escape {other:?}")),
        });
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) fn temp_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("gutter-record-{}-{name}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_round_trip_through_the_file() {
        // "é" is C3 A9; the two chunks split it.
        let events = vec![
            (
                0,
                Event::Start {
                    cols: 120,
                    rows: 40,
                    base_row: 39,
                    width: Width::Percent(50),
                    layout: Layout::Left,
                },
            ),
            (1, Event::Output(b"\x1b[31m\0\xff\\ \"q' \t\r\n caf\xc3".to_vec())),
            (2, Event::Output(b"\xa9 ".to_vec())),
            (3, Event::Output(Vec::new())),
            (250, Event::Resize { cols: 60, rows: 20 }),
            (900, Event::ResizeMode(true)),
            (950, Event::Step(-10)),
            (4000, Event::ResizeMode(false)),
            (
                4001,
                Event::Start {
                    cols: 80,
                    rows: 24,
                    base_row: 0,
                    width: Width::Cols(100),
                    layout: Layout::Center,
                },
            ),
        ];

        let path = temp_path("round-trip");
        let mut rec = Recorder::create(&path).unwrap();
        for (at, event) in &events {
            rec.record(*at, event);
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert_eq!(text.lines().count(), events.len() + 1, "one line per event:\n{text}");
        assert_eq!(parse(&text).unwrap(), events);
    }

    #[test]
    fn parse_rejects_a_file_without_the_version_line() {
        assert!(parse("0 resize 80 24\n").is_err());
        assert!(parse("gutter-record 1\n0 resize 80\n").is_err());
    }
}
