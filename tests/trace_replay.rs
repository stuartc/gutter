#![cfg(feature = "oracle")]
//! Diagnostic scaffolding, not a gate: replays a `GUTTER_TRACE_OUT` capture
//! through wezterm-term at a physical size and dumps the screen, flagging any
//! row whose content reaches outside a given band.
//!
//! `#[ignore]`d — it is driven by env vars and has no assertions.
//!   TRACE=<file> COLS=<n> ROWS=<n> [MARGIN=<n> BAND=<n>] \
//!     cargo test --features oracle --test trace_replay -- --ignored --nocapture
use std::sync::Arc;
use tattoy_wezterm_term::color::ColorPalette;
use tattoy_wezterm_term::{Terminal, TerminalConfiguration, TerminalSize};

#[derive(Debug)]
struct Cfg;
impl TerminalConfiguration for Cfg {
    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }
}

fn env_u16(k: &str) -> Option<u16> {
    std::env::var(k).ok().and_then(|v| v.parse().ok())
}

#[test]
#[ignore = "diagnostic; driven by TRACE/COLS/ROWS env vars"]
fn replay() {
    let path = std::env::var("TRACE").expect("set TRACE=<capture file>");
    let cols = env_u16("COLS").expect("set COLS");
    let rows = env_u16("ROWS").expect("set ROWS");
    let bytes = std::fs::read(&path).expect("read trace");
    eprintln!("{} bytes from {path}", bytes.len());

    let mut t = Terminal::new(
        TerminalSize { rows: rows as usize, cols: cols as usize, pixel_width: 0, pixel_height: 0, dpi: 0 },
        Arc::new(Cfg),
        "trace-replay",
        "0",
        Box::new(std::io::sink()),
    );
    t.advance_bytes(&bytes);

    let screen = t.screen();
    let lines = screen.lines_in_phys_range(screen.phys_range(&(0..rows as i64)));
    let band = env_u16("BAND");
    let margin = env_u16("MARGIN");

    for (i, line) in lines.iter().enumerate() {
        let cells: Vec<String> = (0..cols as usize)
            .map(|c| line.get_cell(c).map(|x| x.str().to_string()).unwrap_or_else(|| " ".into()))
            .collect();
        // Anything painted outside [margin, margin+band) is a band escape.
        let escape = match (margin, band) {
            (Some(m), Some(w)) => cells.iter().enumerate().any(|(c, s)| {
                (c < m as usize || c >= (m + w) as usize) && !s.trim().is_empty()
            }),
            _ => false,
        };
        eprintln!(
            "{i:3} {}{} |{}|",
            if escape { "ESCAPE " } else { "       " },
            if line.last_cell_was_wrapped() { "wrap" } else { "    " },
            cells.join("").trim_end()
        );
    }
}
