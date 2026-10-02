//! The painted-band check: replay the bytes gutter writes through wezterm-term at
//! the **physical** terminal size, and compare the band's rectangle against the
//! child's own `W`-column vt100 grid.
//!
//! Distinct from the equivalence gate next door, which diffs two grids at the same
//! width and never looks at gutter's output at all. Nothing else in the repo can
//! see a misplaced cursor move: the render tests read back through
//! [`RecordingGrid`], which is itself a `vt100::Parser`, and the clipper's column
//! tracker is built on that same model — so the readback and the code under test
//! agree by construction. wezterm-term is a second model, and it implements
//! deferred wrap, which vt100 does not: a glyph written into the screen's last
//! column leaves a real cursor **on** that column with the wrap pending, where
//! vt100 reports it one further right. Every band whose right edge is the screen's
//! right edge — the default width on a terminal no wider than it, and every
//! `--width full` run — lands on that difference.
//!
//! [`RecordingGrid`]: crate::terminal::mock::RecordingGrid

use std::collections::VecDeque;
use std::io;

use super::cellview::{CellView, Grid};
use crate::geometry::Rails;
use crate::terminal::{CrosstermTerminal, OuterTerminal};

/// An [`OuterTerminal`] that keeps the byte stream instead of a screen: the
/// production [`CrosstermTerminal`] over a `Vec<u8>`, so the tape is exactly what
/// gutter would have written and can be handed to another emulator verbatim.
pub struct Tape {
    inner: CrosstermTerminal<Vec<u8>>,
    size: (u16, u16),
    /// The sizes the terminal goes on to take, oldest first.
    resizes: VecDeque<(u16, u16)>,
    cuts: Vec<Cut>,
}

/// One stretch of a tape that was resized as it ran: what gutter wrote, or the terminal
/// taking a new size between two writes.
#[derive(Debug, PartialEq, Eq)]
pub enum Cut {
    Bytes(Vec<u8>),
    Resized { cols: u16, rows: u16 },
}

impl Tape {
    /// The tape opens with the host autowrap gutter's own setup turns off, so what the
    /// replay sees is the terminal a real run paints on rather than a default one.
    #[must_use]
    pub fn new(cols: u16, rows: u16) -> Self {
        let mut tape = Self {
            inner: CrosstermTerminal::from_writer(Vec::new(), (cols, rows)),
            size: (cols, rows),
            resizes: VecDeque::new(),
            cuts: Vec::new(),
        };
        tape.set_autowrap(false).expect("a Vec sink cannot fail");
        tape
    }

    /// Script the terminal's next change of size. It takes effect the next time gutter
    /// asks for the size, which the render loop does once per `Msg::Resize` and nowhere
    /// else short of a suspend: by then a real terminal has already changed, so what
    /// gutter wrote before asking went to the old size and what it writes after goes to
    /// the new one.
    pub fn queue_resize(&mut self, cols: u16, rows: u16) {
        self.resizes.push_back((cols, rows));
    }

    /// Everything since the last call, in order, leaving the tape empty.
    pub fn cut(&mut self) -> Vec<Cut> {
        self.cut_bytes();
        std::mem::take(&mut self.cuts)
    }

    fn cut_bytes(&mut self) {
        self.inner.flush().expect("flush the tape");
        let bytes = std::mem::take(self.inner.writer().get_mut());
        if !bytes.is_empty() {
            self.cuts.push(Cut::Bytes(bytes));
        }
    }

    /// The recorded stream.
    #[must_use]
    pub fn into_bytes(mut self) -> Vec<u8> {
        self.inner.flush().expect("flush the tape");
        self.inner.into_writer()
    }
}

impl OuterTerminal for Tape {
    fn enable_raw_mode(&mut self) -> io::Result<()> {
        self.inner.enable_raw_mode()
    }

    fn enable_mouse(&mut self) -> io::Result<()> {
        self.inner.enable_mouse()
    }

    fn set_autowrap(&mut self, on: bool) -> io::Result<()> {
        self.inner.set_autowrap(on)
    }

    fn enter_alt_screen(&mut self) -> io::Result<()> {
        self.inner.enter_alt_screen()
    }

    fn terminal_size(&mut self) -> io::Result<(u16, u16)> {
        if let Some((cols, rows)) = self.resizes.pop_front() {
            self.cut_bytes();
            self.cuts.push(Cut::Resized { cols, rows });
            self.size = (cols, rows);
        }
        Ok(self.size)
    }

    fn move_to(&mut self, col: u16, row: u16) -> io::Result<()> {
        self.inner.move_to(col, row)
    }

    fn write_row(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.inner.write_row(bytes)
    }

    fn newline(&mut self) -> io::Result<()> {
        self.inner.newline()
    }

    fn clear_gutter(
        &mut self,
        margin: u16,
        width: u16,
        real_cols: u16,
        row_start: u16,
        row_end: u16,
    ) -> io::Result<()> {
        self.inner
            .clear_gutter(margin, width, real_cols, row_start, row_end)
    }

    fn clear_row_span(&mut self, row_start: u16, row_end: u16) -> io::Result<()> {
        self.inner.clear_row_span(row_start, row_end)
    }

    fn draw_rails(&mut self, rails: &Rails) -> io::Result<()> {
        self.inner.draw_rails(rails)
    }

    fn place_cursor(&mut self, col: u16, row: u16) -> io::Result<()> {
        self.inner.place_cursor(col, row)
    }

    fn set_cursor_visible(&mut self, visible: bool) -> io::Result<()> {
        self.inner.set_cursor_visible(visible)
    }

    fn set_cursor_shape(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.inner.set_cursor_shape(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }

    fn relay(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.inner.relay(bytes)
    }

    fn leave_alt_screen(&mut self) -> io::Result<()> {
        self.inner.leave_alt_screen()
    }

    fn disable_mouse(&mut self) -> io::Result<()> {
        self.inner.disable_mouse()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }

    fn disable_raw_mode(&mut self) -> io::Result<()> {
        self.inner.disable_raw_mode()
    }
}

/// The geometry one painted-band case is rendered at: a child grid of `width × rows`,
/// painted at `margin`/`base_row` on a `phys_cols × phys_rows` screen.
///
/// A check that compares the band rectangle against the child grid needs
/// `base_row + rows <= phys_rows`. Otherwise the rectangle runs off the bottom of the
/// screen, and `diff_cells` — which compares the smaller of the two grids — leaves the
/// child rows past the edge unlooked-at.
///
/// The renderer reads the child grid's height as the terminal's, which is production's
/// shape. So a case that means to scroll — the band growing past the bottom — wants
/// `phys_rows == rows`, or the row it scrolls at and the screen's bottom row are
/// different rows.
#[derive(Clone, Copy)]
pub struct BandGeometry {
    pub width: u16,
    pub rows: u16,
    pub margin: u16,
    pub base_row: u16,
    pub phys_cols: u16,
    pub phys_rows: u16,
}

/// The band's rectangle on the physical screen, read as a [`Grid`]: view cell
/// `(row, col)` is physical cell `(base_row + row, left_margin + col)`. Lets the
/// gate's own `diff_cells`/`classify` compare the painted band against the child's
/// grid without either side knowing about the offset.
pub struct BandRect<'a, G: Grid> {
    inner: &'a G,
    base_row: u16,
    left_margin: u16,
    rows: u16,
    cols: u16,
}

impl<'a, G: Grid> BandRect<'a, G> {
    #[must_use]
    pub fn new(inner: &'a G, base_row: u16, left_margin: u16, rows: u16, cols: u16) -> Self {
        Self {
            inner,
            base_row,
            left_margin,
            rows,
            cols,
        }
    }
}

impl<G: Grid> Grid for BandRect<'_, G> {
    fn dims(&self) -> (u16, u16) {
        (self.rows, self.cols)
    }

    fn cell(&self, row: u16, col: u16) -> CellView {
        self.inner.cell(self.base_row + row, self.left_margin + col)
    }
}

#[cfg(test)]
mod painted_band {
    use super::*;
    use crate::oracle::gate::{classify, diff_cells, Vt100Grid};
    use crate::oracle::{
        cellview::{Color, Verdict},
        WeztermGrid,
    };
    use crate::render::{paint_frames_to_tape, Renderer};
    use crate::rowclip::{clip_row_to_width, Placement};

    /// A stream that fills a `W`-wide row and wraps one glyph past it inside a single
    /// `process` call — the shape that makes vt100 flip the row's wrapped flag and
    /// repair it with an absolute jump back to the row's last cell. On a screen exactly
    /// `W` wide the repair lands on the deferred-wrap column, so this is the minimal
    /// trigger for the model difference.
    const WRAP_FLIP: &[u8] = b"0123456789X";
    const WRAP_FLIP_WIDTH: u16 = 10;

    /// A full-width row erased back to blank. The erase is what puts a row-final
    /// `ESC[K` in a run, and under default SGR it writes no glyph anywhere — the only
    /// readback that can tell it from doing nothing is one whose cells already held
    /// something. It needs a frame boundary between the fill and the erase, which the
    /// chunk sweep supplies.
    const ERASE_TO_EDGE: &[u8] = b"AAAAAAAAAA\x1b[1;1H\x1b[K";

    /// The wide-edge fixture and the size it was recorded at. Replaying at any other
    /// size misaligns the diff.
    const WIDE_EDGE: &[u8] = include_bytes!("../../tests/fixtures/wide-edge.cast");
    const WIDE_EDGE_WIDTH: u16 = 80;
    const WIDE_EDGE_ROWS: u16 = 24;

    /// Coloured lines past a screenful on the primary screen, which is the only way to
    /// reach the two paints that scroll the real terminal — `scroll_to_make_room` while
    /// the band still has room to grow, then `emit_scroll_stream` once it fills the
    /// screen. Plain text, so any width replays it faithfully.
    const PLAIN_SCROLL: &[u8] = include_bytes!("../../tests/fixtures/plain-scroll.cast");

    /// The realistic, SGR-dense Claude Code baseline — the only fixture the equivalence
    /// gate treats as primary (`gate.rs`) — and the size it was recorded at. Enters the
    /// alt screen in its first byte (`ESC[?1049h`) and never leaves it, so it exercises
    /// `painted_band_matches_the_child_grid_on_a_real_terminal` (the band-vs-child diff,
    /// which is geometry-only and does not care which screen is active) but not
    /// `nothing_is_painted_outside_the_band`'s sentinel seed (alt-screen tapes skip the
    /// seed and fall back to an all-blank `untouched` cell, so the check still runs, just
    /// without the shell-history sentinel) or the scroll/blank-row tests, which need the
    /// primary screen and use their own hardcoded cases regardless.
    const CLAUDE_CODE_FLOW: &[u8] = include_bytes!("../../tests/fixtures/claude-code-flow.cast");
    const CLAUDE_CODE_FLOW_WIDTH: u16 = 80;
    const CLAUDE_CODE_FLOW_ROWS: u16 = 24;

    /// One band-on-screen case: the child stream and the geometry it is painted at.
    struct Case {
        name: &'static str,
        stream: &'static [u8],
        geom: BandGeometry,
    }

    /// Every configuration the checks cover. `margin == 0 && phys_cols == width` is the
    /// one the in-repo tests structurally cannot reach: the band's right edge is the
    /// screen's, so deferred wrap is live on the cells being asserted.
    fn cases() -> Vec<Case> {
        vec![
            Case {
                name: "wrap-flip, full width (margin 0, band edge == screen edge)",
                stream: WRAP_FLIP,
                geom: BandGeometry {
                    width: WRAP_FLIP_WIDTH,
                    rows: 4,
                    margin: 0,
                    base_row: 0,
                    phys_cols: WRAP_FLIP_WIDTH,
                    phys_rows: 4,
                },
            },
            Case {
                name: "wrap-flip, full width at a non-zero base_row",
                stream: WRAP_FLIP,
                geom: BandGeometry {
                    width: WRAP_FLIP_WIDTH,
                    rows: 5,
                    margin: 0,
                    base_row: 3,
                    phys_cols: WRAP_FLIP_WIDTH,
                    phys_rows: 8,
                },
            },
            Case {
                name: "wrap-flip, inset band",
                stream: WRAP_FLIP,
                geom: BandGeometry {
                    width: WRAP_FLIP_WIDTH,
                    rows: 4,
                    margin: 6,
                    base_row: 0,
                    phys_cols: 22,
                    phys_rows: 4,
                },
            },
            Case {
                name: "wrap-flip, inset band at a non-zero base_row",
                stream: WRAP_FLIP,
                geom: BandGeometry {
                    width: WRAP_FLIP_WIDTH,
                    rows: 5,
                    margin: 6,
                    base_row: 3,
                    phys_cols: 22,
                    phys_rows: 8,
                },
            },
            Case {
                name: "erase to the band edge, inset band",
                stream: ERASE_TO_EDGE,
                geom: BandGeometry {
                    width: WRAP_FLIP_WIDTH,
                    rows: 4,
                    margin: 6,
                    base_row: 0,
                    phys_cols: 22,
                    phys_rows: 4,
                },
            },
            Case {
                // The band starts below the launch row and grows until it fills the
                // screen, then the stream scrolls: both scrolling paints, in order.
                name: "plain-scroll fixture, inset band from a non-zero base_row",
                stream: PLAIN_SCROLL,
                geom: BandGeometry {
                    width: 40,
                    rows: 10,
                    margin: 5,
                    base_row: 3,
                    phys_cols: 50,
                    phys_rows: 10,
                },
            },
            Case {
                name: "wide-edge fixture, full width (margin 0)",
                stream: WIDE_EDGE,
                geom: BandGeometry {
                    width: WIDE_EDGE_WIDTH,
                    rows: WIDE_EDGE_ROWS,
                    margin: 0,
                    base_row: 0,
                    phys_cols: WIDE_EDGE_WIDTH,
                    phys_rows: WIDE_EDGE_ROWS,
                },
            },
            Case {
                name: "wide-edge fixture, inset band",
                stream: WIDE_EDGE,
                geom: BandGeometry {
                    width: WIDE_EDGE_WIDTH,
                    rows: WIDE_EDGE_ROWS,
                    margin: 6,
                    base_row: 0,
                    phys_cols: 92,
                    phys_rows: WIDE_EDGE_ROWS,
                },
            },
            Case {
                // margin 0, band edge == screen edge: deferred wrap is live on the cells
                // this check asserts, which is the configuration the rest of the suite
                // structurally cannot reach.
                name: "claude-code-flow fixture, full width (margin 0, band edge == screen edge)",
                stream: CLAUDE_CODE_FLOW,
                geom: BandGeometry {
                    width: CLAUDE_CODE_FLOW_WIDTH,
                    rows: CLAUDE_CODE_FLOW_ROWS,
                    margin: 0,
                    base_row: 0,
                    phys_cols: CLAUDE_CODE_FLOW_WIDTH,
                    phys_rows: CLAUDE_CODE_FLOW_ROWS,
                },
            },
            Case {
                // Centred: a real gutter on both sides. Strengthens
                // `painted_band_matches_the_child_grid_on_a_real_terminal`, and — unlike
                // the margin-0 case above — also exercises `nothing_is_painted_outside_the_band`
                // (alt-screen tapes aren't skipped there, just seeded with a blank
                // `untouched` cell instead of shell-history sentinel text). See
                // CLAUDE_CODE_FLOW's doc comment for what neither case can draw from this
                // fixture regardless of geometry.
                name: "claude-code-flow fixture, inset band",
                stream: CLAUDE_CODE_FLOW,
                geom: BandGeometry {
                    width: CLAUDE_CODE_FLOW_WIDTH,
                    rows: CLAUDE_CODE_FLOW_ROWS,
                    margin: 6,
                    base_row: 0,
                    phys_cols: 92,
                    phys_rows: CLAUDE_CODE_FLOW_ROWS,
                },
            },
        ]
    }

    /// The frame boundaries every case is replayed at — the sizes `painted_runs` sweeps,
    /// plus the whole stream as one frame. A row run's shape depends on where the stream
    /// was cut, and the paths that describe a change since the last frame — the `prev`
    /// baseline diff, `scroll_to_make_room`, `emit_scroll_stream` — are only reached at
    /// the second frame and after.
    fn chunk_sizes(stream: &[u8]) -> [usize; 4] {
        [1, 7, 64, stream.len().max(1)]
    }

    /// Replay `stream` as a run of frames `chunk` bytes wide.
    fn paint_chunked(stream: &[u8], chunk: usize, geom: BandGeometry) -> (Renderer, Vec<u8>) {
        let frames: Vec<&[u8]> = stream.chunks(chunk).collect();
        paint_frames_to_tape(&frames, geom)
    }

    /// The band on screen is the child's grid. For each configuration and each frame
    /// boundary: paint, replay the emitted bytes through wezterm-term at the physical
    /// size, and diff the band's rectangle against the child's own `W`-column vt100 grid.
    ///
    /// A character mismatch fails. An attribute-only difference is the benign
    /// convention gap ADR-001 already tolerates between the two emulators, and would be
    /// noise here.
    #[test]
    fn painted_band_matches_the_child_grid_on_a_real_terminal() {
        for case in cases() {
            let geom = case.geom;
            for chunk in chunk_sizes(case.stream) {
                let (renderer, tape) = paint_chunked(case.stream, chunk, geom);
                let screen = WeztermGrid::replay(&tape, geom.phys_cols, geom.phys_rows);
                let child = Vt100Grid::new(renderer.screen());
                let offset = renderer.span_offset();

                assert!(
                    offset + geom.rows <= geom.phys_rows,
                    "{}: the band's {} rows at offset {offset} run off a {}-row screen, \
                     so the rows past the bottom were never painted anywhere",
                    case.name,
                    geom.rows,
                    geom.phys_rows
                );
                let band = BandRect::new(&screen, offset, geom.margin, geom.rows, geom.width);
                assert_eq!(
                    band.dims(),
                    child.dims(),
                    "{}: the band rectangle and the child grid must be the same size — \
                     `diff_cells` walks the smaller of the two and leaves the rest of \
                     the larger unchecked",
                    case.name
                );

                let corrupting: Vec<_> = diff_cells(&band, &child)
                    .into_iter()
                    .filter(|d| classify(d) == Verdict::Corrupting)
                    .collect();
                assert!(
                    corrupting.is_empty(),
                    "{} (chunk {chunk}): {} band cell(s) on the real terminal differ \
                     from the child grid. (band row, band col, on screen, child): {:?}",
                    case.name,
                    corrupting.len(),
                    corrupting
                        .iter()
                        .take(8)
                        .map(|d| (d.row, d.col, &d.bare_cell.contents, &d.wrapped_cell.contents))
                        .collect::<Vec<_>>()
                );
            }
        }
    }

    /// Painted over every cell of the physical screen before the tape is replayed.
    ///
    /// On a screen that starts blank, an escape that ERASES outside the band leaves the
    /// cell exactly as it found it and the readback sees nothing — so the unbounded
    /// row-final `ESC[K` this whole module exists to catch reads as untouched. The
    /// primary screen a band launches into is full of shell history; the sentinel stands
    /// in for it, and a wiped cell then shows up as the missing character it is.
    const SENTINEL: &str = "#";

    /// [`SENTINEL`] on every cell of a `cols × rows` screen, as bytes to replay ahead of
    /// a tape. Each row is addressed absolutely, so the pending wrap left by filling a
    /// row's last column is discarded by the next move rather than scrolling the screen.
    fn sentinel_fill(cols: u16, rows: u16) -> Vec<u8> {
        let mut out = Vec::new();
        for row in 1..=rows {
            out.extend_from_slice(format!("\x1b[{row};1H").as_bytes());
            for _ in 0..cols {
                out.extend_from_slice(SENTINEL.as_bytes());
            }
        }
        out.extend_from_slice(b"\x1b[H");
        out
    }

    /// The paint changes no cell outside the band's rectangle. Every cell in the gutter,
    /// and every cell above `base_row`, still holds what stood there before the tape ran
    /// — same character, same colours, same attributes — so a stray glyph, an erase and
    /// a coloured flood all fail alike.
    #[test]
    fn nothing_is_painted_outside_the_band() {
        let mut checked = 0usize;
        for case in cases() {
            let geom = case.geom;
            for chunk in chunk_sizes(case.stream) {
                let (renderer, tape) = paint_chunked(case.stream, chunk, geom);
                let offset = renderer.span_offset();

                // A band that fills the screen has no outside to escape to. Those shapes
                // are here for the check above, where the band's right edge being the
                // screen's is the whole point of them.
                if offset == 0 && geom.margin == 0 && geom.margin + geom.width >= geom.phys_cols {
                    continue;
                }
                // A `newline` scrolls the whole physical screen, carrying the seeded rows
                // up with it, and the blank row that scrolls in at the bottom then reads
                // exactly like an erase. What the gutter holds after a scroll is
                // `a_scroll_leaves_the_gutter_untinted`'s question, not this one's.
                if tape.windows(2).any(|w| w == b"\r\n") {
                    continue;
                }

                // The alt screen is the terminal's own: cleared on entry, never holding
                // what stood on the primary. There is nothing out there to seed, and
                // nothing out there to lose. The primary screen is where a band lands in
                // the middle of someone's shell history.
                let (seed, untouched) = if renderer.screen().alternate_screen() {
                    (Vec::new(), CellView::blank())
                } else {
                    (
                        sentinel_fill(geom.phys_cols, geom.phys_rows),
                        CellView {
                            contents: SENTINEL.to_string(),
                            ..CellView::blank()
                        },
                    )
                };
                let screen =
                    WeztermGrid::replay(&[seed, tape].concat(), geom.phys_cols, geom.phys_rows);

                checked += 1;
                let mut inspected = 0usize;
                for row in 0..geom.phys_rows {
                    for col in 0..geom.phys_cols {
                        let in_band =
                            row >= offset && (geom.margin..geom.margin + geom.width).contains(&col);
                        if in_band {
                            continue;
                        }
                        inspected += 1;
                        assert_eq!(
                            screen.cell(row, col),
                            untouched,
                            "{} (chunk {chunk}): physical cell (row {row}, col {col}) is \
                             outside the band rectangle rows [{offset}, {}) x cols \
                             [{}, {}) and the paint changed it",
                            case.name,
                            geom.phys_rows,
                            geom.margin,
                            geom.margin + geom.width
                        );
                    }
                }
                assert!(
                    inspected > 0,
                    "{}: every cell on the screen is inside the band, so this case \
                     asserted nothing",
                    case.name
                );
            }
        }
        assert!(
            checked > 0,
            "every case was skipped, so the check ran on nothing"
        );
    }

    /// A row the child paints under a background colour and erases to the band edge —
    /// the shape whose row run ends with the background still set, which is the state
    /// a scroll smears across the full physical width.
    const TINTED_LINE: &[u8] = b"\x1b[44mtinted\x1b[K\x1b[m\r\n";

    /// The palette index `ESC[44m` selects.
    const BLUE: Color = Color::Indexed(4);

    /// One multi-frame band-on-screen case: a frame per element of `frames`, where the
    /// split matters to what is being checked and a chunk sweep would not reproduce it.
    struct FrameCase {
        name: &'static str,
        frames: Vec<&'static [u8]>,
        geom: BandGeometry,
    }

    /// Scrolling the real terminal never colours the gutter. Both loops that scroll it
    /// do so with a `newline`, and a terminal fills the line that scrolls in with the
    /// active background across its full width — so a band row painted under a
    /// background SGR would tint both gutters on the next scroll, on rows the diff
    /// baseline never repaints.
    #[test]
    fn a_scroll_leaves_the_gutter_untinted() {
        let cases = vec![
            FrameCase {
                // Past the bottom the stream scrolls per line: `emit_scroll_stream`.
                name: "scroll stream, band spanning the screen",
                frames: vec![TINTED_LINE; 12],
                geom: BandGeometry {
                    width: 20,
                    rows: 5,
                    margin: 3,
                    base_row: 0,
                    phys_cols: 26,
                    phys_rows: 5,
                },
            },
            FrameCase {
                // The band grows down from the launch row: `scroll_to_make_room`.
                name: "inline growth from a non-zero base row",
                frames: vec![TINTED_LINE; 4],
                geom: BandGeometry {
                    width: 20,
                    rows: 5,
                    margin: 3,
                    base_row: 3,
                    phys_cols: 26,
                    phys_rows: 5,
                },
            },
        ];

        for case in cases {
            let geom = case.geom;
            let (_, tape) = paint_frames_to_tape(&case.frames, geom);
            let screen = WeztermGrid::replay(&tape, geom.phys_cols, geom.phys_rows);
            let band = geom.margin..geom.margin + geom.width;

            for row in 0..geom.phys_rows {
                for col in (0..geom.phys_cols).filter(|c| !band.contains(c)) {
                    let cell = screen.cell(row, col);
                    assert_eq!(
                        (cell.bgcolor, cell.contents.as_str()),
                        (Color::Default, ""),
                        "{}: gutter cell (row {row}, col {col}) is not blank",
                        case.name
                    );
                }
            }
            // The band itself is meant to be blue to its own edge — a check that
            // passed by painting nothing would be no check at all.
            let painted = (0..geom.phys_rows).any(|row| {
                screen.cell(row, geom.margin).bgcolor == BLUE
                    && screen.cell(row, geom.margin + geom.width - 1).bgcolor == BLUE
            });
            assert!(painted, "{}: no band row is coloured", case.name);
        }
    }

    /// A scrolling frame rewrites every band row, so a row that is now blank has to
    /// blank the physical row it is painted on. Its run describes only the cells that
    /// differ from a blank one — nothing at all for an empty row — and the following
    /// `newline` carries whatever it left standing up the screen, into rows the diff
    /// baseline never revisits.
    #[test]
    fn a_scrolled_row_that_went_blank_clears_what_stood_there() {
        // Frame 1 fills the band. Frame 2 scrolls one line off the top and erases what
        // is now the band's second row, so its run is empty while the physical row
        // under it still holds "ccc".
        let case = FrameCase {
            name: "a row emptied in the frame that scrolls",
            frames: vec![b"aaa\r\nbbb\r\nccc\r\nddd", b"\r\neee\x1b[2;1H\x1b[2K\x1b[4;4H"],
            geom: BandGeometry {
                width: 10,
                rows: 4,
                margin: 3,
                base_row: 0,
                phys_cols: 16,
                phys_rows: 4,
            },
        };
        let geom = case.geom;

        let (renderer, tape) = paint_frames_to_tape(&case.frames, geom);
        let screen = WeztermGrid::replay(&tape, geom.phys_cols, geom.phys_rows);
        let child = Vt100Grid::new(renderer.screen());
        let band = BandRect::new(&screen, 0, geom.margin, geom.rows, geom.width);

        let corrupting: Vec<_> = diff_cells(&band, &child)
            .into_iter()
            .filter(|d| classify(d) == Verdict::Corrupting)
            .collect();
        assert!(
            corrupting.is_empty(),
            "{}: {} band cell(s) on the real terminal differ from the child grid. \
             (band row, band col, on screen, child): {:?}",
            case.name,
            corrupting.len(),
            corrupting
                .iter()
                .take(8)
                .map(|d| (d.row, d.col, &d.bare_cell.contents, &d.wrapped_cell.contents))
                .collect::<Vec<_>>()
        );
    }

    /// A clipped run's absolute moves land on the band's own cells, read back off a real
    /// emulator, at a band whose right edge is the screen's.
    ///
    /// The run is handed to the clipper directly rather than coming out of a child
    /// stream, because `CHA` (`ESC[…G`) has no other way in: vt100 0.16.2's writer emits
    /// `CUP`, `MoveRight`, `Crlf` and backspace and never a `CHA`, so no stream can put
    /// one in a `rows_diff` run and the arm that rewrites it is otherwise unreachable.
    /// The painting is what `render_once` does with a run — one `move_to` to the band's
    /// origin, then the clipped bytes — so the bytes under test are the ones it would
    /// write.
    ///
    /// Ten glyphs into the screen's last column leave wezterm's cursor **on** that
    /// column with the wrap pending, a column behind where vt100 has it. This is the
    /// difference an absolute move exists to survive.
    #[test]
    fn a_clipped_runs_absolute_moves_land_on_the_bands_own_cells() {
        let (width, margin, phys_cols, phys_rows) = (10u16, 6u16, 16u16, 4u16);
        let at = Placement {
            left_margin: margin,
            phys_row: 2,
            grid_row: 1,
        };
        // Both moves name the band's last column, 0-based 9: `CUP` on the run's own row,
        // `CHA` by column alone.
        for run in [
            b"0123456789\x1b[2;10Hz".as_slice(),
            b"0123456789\x1b[10Gz".as_slice(),
        ] {
            let mut tape = Tape::new(phys_cols, phys_rows);
            tape.move_to(at.left_margin, at.phys_row).unwrap();
            tape.write_row(&clip_row_to_width(run, width, at)).unwrap();
            let screen = WeztermGrid::replay(&tape.into_bytes(), phys_cols, phys_rows);

            let shown = String::from_utf8_lossy(run);
            assert_eq!(
                screen.cell(at.phys_row, margin + width - 1).contents,
                "z",
                "{shown}: the move must restamp the band's last cell on its own row"
            );
            let strays: Vec<_> = (0..phys_rows)
                .flat_map(|row| (0..phys_cols).map(move |col| (row, col)))
                .filter(|&(row, col)| screen.cell(row, col).contents == "z")
                .collect();
            assert_eq!(
                strays,
                vec![(at.phys_row, margin + width - 1)],
                "{shown}: the glyph landed somewhere else as well"
            );
        }
    }

    /// A backward and a whole-line erase stop at the band's edges, read back off a real
    /// emulator with the gutters seeded.
    ///
    /// Handed to the clipper directly, like
    /// `a_clipped_runs_absolute_moves_land_on_the_bands_own_cells` above: vt100 0.16.2's
    /// writer emits `ClearRowForward` and no other row erase, so no child stream can put
    /// an `ESC[1K` or an `ESC[2K` in a run.
    #[test]
    fn a_backward_erase_stops_at_the_bands_edges() {
        let (width, margin, phys_cols, phys_rows) = (10u16, 3u16, 16u16, 4u16);
        let at = Placement {
            left_margin: margin,
            phys_row: 1,
            grid_row: 1,
        };
        // Under a blue background, so an erased cell is a tinted one and a flood past the
        // band shows up as colour where the sentinel stood.
        for (run, erased) in [
            (b"\x1b[44mAB\x1b[2K".as_slice(), 0..width),
            (b"\x1b[44mAB\x1b[1K".as_slice(), 0..3),
        ] {
            let mut tape = Tape::new(phys_cols, phys_rows);
            tape.move_to(at.left_margin, at.phys_row).unwrap();
            tape.write_row(&clip_row_to_width(run, width, at)).unwrap();

            let mut bytes = sentinel_fill(phys_cols, phys_rows);
            bytes.extend(tape.into_bytes());
            let screen = WeztermGrid::replay(&bytes, phys_cols, phys_rows);
            let shown = String::from_utf8_lossy(run).to_string();

            for row in 0..phys_rows {
                for col in 0..phys_cols {
                    let in_band = row == at.phys_row
                        && col.checked_sub(margin).is_some_and(|c| erased.contains(&c));
                    let cell = screen.cell(row, col);
                    if in_band {
                        assert_eq!(
                            cell.bgcolor, BLUE,
                            "{shown}: band cell (row {row}, col {col}) was not erased"
                        );
                    } else {
                        assert_eq!(
                            (cell.contents.as_str(), cell.bgcolor),
                            (SENTINEL, Color::Default),
                            "{shown}: the erase reached (row {row}, col {col})"
                        );
                    }
                }
            }
        }
    }

    /// The tape is the byte stream, not a screen: what the check replays is exactly
    /// what a terminal would receive, `MoveTo`s included and in order.
    #[test]
    fn tape_records_the_moves_and_the_rows() {
        let mut tape = Tape::new(20, 4);
        tape.move_to(6, 2).unwrap();
        tape.write_row(b"hi").unwrap();
        assert_eq!(tape.into_bytes(), b"\x1b[?7l\x1b[3;7Hhi".to_vec());
    }

    /// A run that overruns the screen's last column stops there instead of spilling onto
    /// the next row. Autowrap-off is the only thing standing between the two: with the
    /// host wrapping, the overflow lands a row down, on cells the diff baseline never
    /// repaints, and the terminal marks the line soft-wrapped so the next reflow joins it.
    ///
    /// The band's right edge is the screen's here, which is `--width full` and a default
    /// `gutter claude` on a terminal no wider than the band.
    #[test]
    fn a_run_past_the_screens_last_column_does_not_reach_the_next_row() {
        let (phys_cols, phys_rows) = (10u16, 3u16);
        let mut tape = Tape::new(phys_cols, phys_rows);
        tape.move_to(0, 1).unwrap();
        tape.write_row(b"0123456789ABCDE").unwrap();
        let screen = WeztermGrid::replay(&tape.into_bytes(), phys_cols, phys_rows);

        assert_eq!(
            screen.cell(1, phys_cols - 1).contents,
            "E",
            "the overrun must pile up on the row's last column"
        );
        for col in 0..phys_cols {
            assert_eq!(
                screen.cell(2, col).contents,
                "",
                "the run reached the row below at column {col}"
            );
        }
    }

    /// The stale-width scenario, on the emulator that models wrapping (ADR-024).
    ///
    /// The clear is composed believing the terminal is 130 columns wide — a centred
    /// 80-wide band with 25-column gutters — and replayed on the 128 it has already
    /// shrunk to mid-drag. A run of spaces sized from the believed width would spill two
    /// cells onto the row below and leave the row soft-wrapped; the erases reach the real
    /// edge, wherever it is, so they cannot.
    ///
    /// Replayed with the host's autowrap left ON, unlike every other case here. The
    /// wrap-off belt would hide the mechanism entirely, and the question this asks is
    /// whether the emission is safe without it.
    #[test]
    fn a_gutter_clear_composed_at_a_stale_width_stays_on_its_rows() {
        let (believed, real, rows) = (130u16, 128u16, 4u16);
        let (margin, width) = (25u16, 80u16);

        let mut bytes = sentinel_fill(real, rows);
        crate::terminal::clear_gutter_bytes(&mut bytes, margin, width, believed, 0, rows);
        let screen = WeztermGrid::replay(&bytes, real, rows);

        for row in 0..rows {
            assert!(
                !screen.row_wrapped(row),
                "row {row} was left soft-wrapped, so the next reflow joins it with the \
                 row below"
            );
            for col in 0..real {
                let want = if (margin..margin + width).contains(&col) {
                    SENTINEL
                } else {
                    ""
                };
                assert_eq!(
                    screen.cell(row, col).contents,
                    want,
                    "physical cell (row {row}, col {col})"
                );
            }
        }
    }

    /// The relative-hop scars ADR-014 records still corrupt a cell under the autowrap-off
    /// replay, so the absolute-`CUP` rewrite in the clipper is still what keeps them out.
    ///
    /// Wrap-off removes deferred wrap from the replay, which is the condition these were
    /// originally demonstrated under — but not the disagreement itself: the cursor clamps
    /// at the band's last column while the clipper's tracker has counted `W`, so a `CUB`
    /// computed from the tracker still lands a column early. Proving only that the current
    /// correct code stays green would say nothing about whether this check can still catch
    /// the regression it was built for.
    #[test]
    fn the_relative_hop_scars_still_corrupt_under_autowrap_off() {
        let (phys_cols, phys_rows) = (10u16, 2u16);
        let row = |run: &[u8]| {
            let mut tape = Tape::new(phys_cols, phys_rows);
            tape.move_to(0, 0).unwrap();
            tape.write_row(run).unwrap();
            let screen = WeztermGrid::replay(&tape.into_bytes(), phys_cols, phys_rows);
            (0..phys_cols)
                .map(|c| screen.cell(0, c).contents)
                .collect::<String>()
        };

        // ADR-014's own case: ten glyphs to the screen's edge, then a `CUB(1)` back to
        // restamp the last cell. The hop undershoots and takes out the `8`.
        assert_eq!(
            row(b"0123456789\x1b[1D9"),
            "0123456799",
            "the relative hop must still land a column early"
        );
        // The rewrite that replaced it, on the same cell.
        assert_eq!(row(b"0123456789\x1b[1;10H9"), "0123456789");

        // The erase scar the Decision originally shipped: a `W`-bounded fill from column
        // 2 to the edge, then a `CUB` back to the tracked column. The walk back lands on
        // column 1, and the glyph after it is shifted left.
        assert_eq!(
            row(b"AB        \x1b[8Dz"),
            "Az",
            "the fill's relative walk back must still land a column early"
        );
        assert_eq!(row(b"AB        \x1b[1;3Hz"), "ABz");
    }
}
