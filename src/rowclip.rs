//! Clip a vt100 row run to the band width. See ADR-014.
//!
//! [`clip_row_to_width_into`] rewrites a row's erases (`ESC[K` and friends) into
//! `W`-bounded fills so a painted row stays inside its `[0, W)` rectangle rather
//! than flooding a gutter. To place a fill it tracks a virtual in-band column
//! across the run, honouring absolute moves (`CUP`/`CHA`) as well as relative ones
//! — a rightward-only tracker would desync at a backward jump.
//!
//! Those absolute moves are rewritten. A run is painted at the band's left margin
//! and row offset, so an absolute coordinate in the run's own band-local terms
//! would address the raw screen instead: the cursor would leave the band, and the
//! caller's diff baseline never learns of the cells it stamped. The rewrite puts
//! the same cell back in the band's physical coordinates ([`Placement`]).

use unicode_width::UnicodeWidthChar;

/// Where a run is painted on the real terminal, and which grid row it was built for.
///
/// A run carries no origin of its own — the caller establishes one with a single
/// `move_to` before writing it — so the clipper needs those same coordinates to
/// re-express the run's absolute moves in them.
#[derive(Clone, Copy, Debug)]
pub struct Placement {
    /// Physical column the band starts at.
    pub left_margin: u16,
    /// Physical row the caller positioned to before writing the run.
    pub phys_row: u16,
    /// The 0-based grid row vt100 built the run for — the row every absolute move
    /// inside the run names.
    pub grid_row: u16,
}

/// CSI parameter bytes are `0x30..=0x3f`; the final byte is `0x40..=0x7e`.
fn is_csi_final(b: u8) -> bool {
    (0x40..=0x7e).contains(&b)
}

/// Parse the decimal parameter at `params[..]` up to the first `;`, defaulting to
/// `default` when the field is empty (vt100's own convention).
fn first_param(params: &[u8], default: u16) -> u16 {
    let field = params.split(|&b| b == b';').next().unwrap_or(&[]);
    parse_param(field, default)
}

/// The decimal parameter at `params` after the **last** `;` (the column field of
/// a `CUP` `ESC[row;colH`), defaulting to `default`.
fn last_param(params: &[u8], default: u16) -> u16 {
    let field = params.rsplit(|&b| b == b';').next().unwrap_or(&[]);
    parse_param(field, default)
}

/// Move the cursor to in-band column `target`, or emit nothing when the tracker is
/// already there, and return the new tracked column. `target` is clamped to `W - 1`,
/// the last column of the band: the callers vt100 produces only ever aim inside the
/// band, so the clamp guards a malformed run rather than a case that arises in
/// practice, but a move to `W` would land on the first gutter column.
fn move_to_col(out: &mut Vec<u8>, col: u16, target: u16, w: u16, at: Placement) -> u16 {
    let target = target.min(w.saturating_sub(1));
    if target != col {
        emit_cup(out, target, at);
    }
    target
}

/// Position the cursor on the band's `target` column of the row this run is painted
/// at, as an absolute physical `CUP`.
///
/// Absolute rather than a `CUF`/`CUB` from the tracked column because of deferred
/// wrap. A glyph written into the *screen's* last column leaves the real cursor on
/// that column with the wrap pending, while the tracker has already counted it as
/// column `W`; so wherever the band's right edge is the screen's right edge — the
/// default band on a terminal no wider than it, and every `--width full` run — a
/// relative move would land one column short. An absolute one re-establishes the
/// position whatever the cursor was doing.
fn emit_cup(out: &mut Vec<u8>, target: u16, at: Placement) {
    let row1 = at.phys_row.saturating_add(1);
    let col1 = at.left_margin.saturating_add(target).saturating_add(1);
    out.extend_from_slice(b"\x1b[");
    out.extend_from_slice(row1.to_string().as_bytes());
    out.push(b';');
    out.extend_from_slice(col1.to_string().as_bytes());
    out.push(b'H');
}

/// Erase the band's `[start, end)` columns under the active SGR, then put the cursor
/// back where the erase found it so the rest of the run still aligns. Both ends are
/// clamped to the band, so a caller may name a span an erase's own semantics would put
/// past the right edge.
///
/// The return is skipped at the pending-wrap column: the tracker's `W` names the first
/// gutter column, and the fill has already left the cursor in the state the erase found
/// it — past the band's last cell, with the wrap deferred where the band's right edge is
/// the screen's.
fn fill_span(out: &mut Vec<u8>, start: u16, end: u16, col: u16, w: u16, at: Placement) {
    let (start, end) = (start.min(w), end.min(w));
    if start >= end {
        return;
    }
    if start != col {
        emit_cup(out, start, at);
    }
    out.extend(std::iter::repeat_n(b' ', usize::from(end - start)));
    if col < w {
        emit_cup(out, col, at);
    }
}

fn parse_param(field: &[u8], default: u16) -> u16 {
    if field.is_empty() {
        return default;
    }
    let mut n: u16 = 0;
    for &b in field {
        if b.is_ascii_digit() {
            n = n.saturating_mul(10).saturating_add(u16::from(b - b'0'));
        } else {
            return default;
        }
    }
    n
}

/// Rewrites the erases in `run` into `W`-bounded fills, and any absolute cursor move
/// into one in `at`'s physical coordinates, so the run stays within its
/// `[0, W)` rectangle once painted at the band offset. Returns the run unchanged when
/// it carries neither.
#[cfg(test)]
pub fn clip_row_to_width(run: &[u8], w: u16, at: Placement) -> Vec<u8> {
    let mut out = Vec::with_capacity(run.len());
    clip_row_to_width_into(run, w, at, &mut out);
    out
}

/// As [`clip_row_to_width`], but appends to a caller-owned buffer instead of
/// allocating. The paint hot path seeds `out` with the per-row `ESC[m` reset and
/// clips straight into it, building a painted row in one allocation, not two.
pub fn clip_row_to_width_into(run: &[u8], w: u16, at: Placement, out: &mut Vec<u8>) {
    let mut col: u16 = 0;
    let mut i = 0;

    while i < run.len() {
        let b = run[i];

        if b == 0x1b {
            // CSI (`ESC[`) carries the cursor-affecting verbs; any other `ESC x`
            // two-byte escape moves no column.
            if run.get(i + 1) == Some(&b'[') {
                let params_start = i + 2;
                let mut j = params_start;
                while j < run.len() && !is_csi_final(run[j]) {
                    j += 1;
                }
                if j >= run.len() {
                    // Truncated CSI — copy the tail verbatim and stop.
                    out.extend_from_slice(&run[i..]);
                    break;
                }
                let final_byte = run[j];
                let params = &run[params_start..j];
                match final_byte {
                    b'K' => {
                        // Every erase becomes a bounded fill under the active SGR, then
                        // a reposition so trailing bytes still align. Erases are read
                        // against the physical row — nothing re-bases the line after the
                        // caller's `move_to` — so passing one through would erase across
                        // the gutters, where the diff baseline never repaints.
                        match first_param(params, 0) {
                            0 => fill_span(out, col, w, col, w, at),
                            // `ESC[1K` reaches up to and including the cursor's own
                            // cell (ECMA-48 EL 1).
                            1 => fill_span(out, 0, col.saturating_add(1), col, w, at),
                            2 => fill_span(out, 0, w, col, w, at),
                            // `ESC[3K` erases the scrollback's saved copy of the line
                            // and a parameter of 4 or more is undefined, so a terminal
                            // ignores it: neither paints a cell. Both are dropped rather
                            // than forwarded — the scrollback `3K` would clear is the
                            // outer terminal's, holding lines the band never owned.
                            _ => {}
                        }
                    }
                    b'C' => {
                        col = col.saturating_add(first_param(params, 1));
                        out.extend_from_slice(&run[i..=j]);
                    }
                    b'D' => {
                        col = col.saturating_sub(first_param(params, 1));
                        out.extend_from_slice(&run[i..=j]);
                    }
                    b'H' | b'f' => {
                        // CUP `ESC[row;colH`: the column is the last param (1-based).
                        // A single param is the row only (`ESC[5H`); the column then
                        // defaults to 1, i.e. in-band column 0.
                        //
                        // The run's row replaces the named one: a run covers one row of
                        // the band, already positioned to by the caller, and vt100 only
                        // emits `CUP` within a run to revisit that same row. The assert
                        // catches a vt100 that stopped doing so, but only in a debug
                        // build — `debug_assert_eq!` is compiled out of a release one,
                        // which discards the row parameter in silence. That silence is
                        // still inside the band: the move below is absolute on the
                        // run's own physical row with the column clamped to `[0, W)`,
                        // so a mismatched row can at worst stamp a cell on the wrong
                        // line of the band, never in the gutter. Dropping the move
                        // instead would desync the tracker and misplace every glyph
                        // after it.
                        let col1 = if params.contains(&b';') {
                            last_param(params, 1)
                        } else {
                            1
                        };
                        debug_assert_eq!(
                            first_param(params, 1),
                            at.grid_row.saturating_add(1),
                            "CUP names a row other than the run's own ({:?})",
                            String::from_utf8_lossy(&run[i..=j])
                        );
                        col = move_to_col(out, col, col1.saturating_sub(1), w, at);
                    }
                    b'G' => {
                        // CHA `ESC[colG`: absolute column, 1-based.
                        col =
                            move_to_col(out, col, first_param(params, 1).saturating_sub(1), w, at);
                    }
                    // SGR (`m`), EraseChar (`X`) and anything else move no column.
                    _ => out.extend_from_slice(&run[i..=j]),
                }
                i = j + 1;
            } else {
                // `ESC x` (e.g. save/restore cursor): copy the two bytes, no move.
                let end = (i + 2).min(run.len());
                out.extend_from_slice(&run[i..end]);
                i = end;
            }
            continue;
        }

        if b == 0x08 {
            // Live on the scroll path, unlike the `CHA` and `D` arms: `rows_formatted`
            // opens a run with `' ' 0x08 ESC[X` when the row above wrapped and this
            // row's first cell is default. Without this the tracker sits a column right
            // of the cursor for the rest of the run and the `ESC[K` fill comes up short.
            col = col.saturating_sub(1);
            out.push(b);
            i += 1;
            continue;
        }

        if b == b'\r' {
            // The first half of the `\r\n` that vt100's `MoveFromTo` writer emits for
            // the start of the next row. Copied through it would put the cursor on
            // *physical* column 0, in the left gutter, and every glyph after it with
            // it; the absolute move puts it on the band's own column 0 instead.
            col = move_to_col(out, col, 0, w, at);
            i += 1;
            continue;
        }

        if b == b'\n' {
            // The other half of that writer's `\r\n`. A run is defined for one
            // physical row, so there is no in-band translation of a line feed: written
            // out it would paint the rest of the run a row below the placement, and on
            // the screen's bottom row scroll the whole screen with no baseline change
            // to repair it. Dropping it leaves the tracker's column alone and the
            // rest of the run on the placement's own row.
            i += 1;
            continue;
        }

        if b < 0x20 {
            // Other C0 controls advance no column.
            out.push(b);
            i += 1;
            continue;
        }

        // A literal character: advance by its display width (vt100's model).
        let len = utf8_len(b);
        let end = (i + len).min(run.len());
        let bytes = &run[i..end];
        if let Ok(s) = std::str::from_utf8(bytes) {
            if let Some(ch) = s.chars().next() {
                col = col.saturating_add(ch.width().unwrap_or(0) as u16);
            }
        }
        out.extend_from_slice(bytes);
        i = end;
    }
}

/// The byte length of a UTF-8 sequence from its lead byte (1 for ASCII / a stray
/// continuation byte, so the scan always advances).
fn utf8_len(lead: u8) -> usize {
    match lead {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The band at the screen's top-left corner, painting the grid's first row — the
    /// placement most cases use, where an in-band column and a physical one coincide.
    const HOME: Placement = Placement {
        left_margin: 0,
        phys_row: 0,
        grid_row: 0,
    };

    /// The absolute move the clip emits to put the cursor on in-band column `col`.
    fn cup_at(at: Placement, col: u16) -> Vec<u8> {
        format!("\x1b[{};{}H", at.phys_row + 1, at.left_margin + col + 1).into_bytes()
    }

    /// [`cup_at`] for [`HOME`].
    fn cup(col: u16) -> Vec<u8> {
        cup_at(HOME, col)
    }

    /// Whether `bytes` holds a CSI terminated by `want`.
    fn has_csi_final(bytes: &[u8], want: u8) -> bool {
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'[') {
                let mut j = i + 2;
                while j < bytes.len() && !is_csi_final(bytes[j]) {
                    j += 1;
                }
                if bytes.get(j) == Some(&want) {
                    return true;
                }
                i = j + 1;
            } else {
                i += 1;
            }
        }
        false
    }

    /// A run with no `ESC[K` is returned byte-identical; the clip only ever touches
    /// the one unbounded sequence.
    #[test]
    fn no_erase_is_identity() {
        let run = b"\x1b[1;31mhello\x1b[mworld";
        assert_eq!(clip_row_to_width(run, 80, HOME), run.to_vec());
    }

    /// Reverse-video full-width row: a leading `ESC[7m` then an immediate `ESC[K`
    /// (the attributed-but-empty statusline) at column 0 clips to exactly `W` spaces
    /// under the reverse SGR plus a reposition, so the highlight reaches the band edge
    /// and the cursor is restored.
    #[test]
    fn reverse_video_full_width_clips_to_w_spaces_plus_reposition() {
        let w = 10u16;
        let run = b"\x1b[7m\x1b[K";
        let got = clip_row_to_width(run, w, HOME);

        let mut want = Vec::new();
        want.extend_from_slice(b"\x1b[7m");
        want.extend(std::iter::repeat_n(b' ', usize::from(w)));
        want.extend(cup(0));
        assert_eq!(got, want);
        // The original unbounded ESC[K is gone.
        assert!(!got.windows(3).any(|s| s == b"\x1b[K"));
    }

    /// The fill starts at the cursor, not column 0: after some glyphs and an
    /// `ESC[K`, only `W - col` spaces are emitted, so the fill stops at the band
    /// edge regardless of where the erase began.
    #[test]
    fn fill_is_remaining_columns_from_cursor() {
        let w = 10u16;
        let run = b"\x1b[7mABC\x1b[K"; // cursor at col 3 after "ABC"
        let got = clip_row_to_width(run, w, HOME);

        let mut want = Vec::new();
        want.extend_from_slice(b"\x1b[7mABC");
        want.extend(std::iter::repeat_n(b' ', 7)); // W - 3
        want.extend(cup(3));
        assert_eq!(got, want);
    }

    /// The fill's reposition is the band's physical cell, not a `CUB` by the fill's
    /// own length. A fill that runs to the screen's right edge leaves the real cursor
    /// on that last column with a deferred wrap pending, one short of where the
    /// tracker has it, so a relative walk back would undershoot by one.
    #[test]
    fn fill_reposition_is_absolute_and_physical() {
        let at = Placement {
            left_margin: 5,
            phys_row: 3,
            grid_row: 3,
        };
        let w = 10u16;
        let got = clip_row_to_width(b"\x1b[7mAB\x1b[K", w, at);

        let mut want = Vec::new();
        want.extend_from_slice(b"\x1b[7mAB");
        want.extend(std::iter::repeat_n(b' ', 8)); // W - 2
        want.extend_from_slice(b"\x1b[4;8H"); // physical row 3, column 5 + 2
        assert_eq!(got, want);
        assert!(!has_csi_final(&got, b'D'));
    }

    /// A wide glyph at the band edge: the tracker must agree with vt100's width
    /// model, so a double-width glyph counts as two columns and the fill after it is
    /// `W - (cols consumed)`, not `W - (chars consumed)`. Here `一` (col 0) then
    /// `ESC[K` at a `W = 4` band leaves `4 - 2 = 2` fill spaces.
    #[test]
    fn wide_glyph_counts_two_columns() {
        let w = 4u16;
        let mut run = Vec::new();
        run.extend_from_slice(b"\x1b[7m");
        run.extend_from_slice("\u{4e00}".as_bytes()); // 一, width 2
        run.extend_from_slice(b"\x1b[K");
        let got = clip_row_to_width(&run, w, HOME);

        let mut want = Vec::new();
        want.extend_from_slice(b"\x1b[7m");
        want.extend_from_slice("\u{4e00}".as_bytes());
        want.extend(std::iter::repeat_n(b' ', 2)); // 4 - 2
        want.extend(cup(2));
        assert_eq!(got, want);
    }

    /// An absolute cursor move resets the tracked column. A run prints to column 8,
    /// jumps back to column 2 via an absolute `CUP` (`ESC[1;3H`), then erases: a
    /// rightward-only tracker would fill from column 8 and under-fill. The fill must
    /// be `W - 2`.
    #[test]
    fn absolute_move_resets_column_not_rightward_only() {
        let w = 20u16;
        // 8 glyphs → col 8, then CUP to row 1 col 3 (0-based col 2), then ESC[K.
        let run = b"\x1b[7mAAAAAAAA\x1b[1;3H\x1b[K";
        let got = clip_row_to_width(run, w, HOME);

        let fill = w - 2; // from the absolute column 2, NOT from 8
        let mut want = Vec::new();
        want.extend_from_slice(b"\x1b[7mAAAAAAAA");
        want.extend(cup(2)); // 8 → 2
        want.extend(std::iter::repeat_n(b' ', usize::from(fill)));
        want.extend(cup(2));
        assert_eq!(got, want);
    }

    /// Row-only `CUP` defaults the column to 1: `ESC[5H` moves to row 5, column
    /// default (in-band column 0), not column 4. A run that prints to column 8, then
    /// a row-only `CUP`, then erases must retreat to column 0 and fill the full `W`.
    #[test]
    fn row_only_cup_defaults_column_to_zero() {
        let w = 20u16;
        // `ESC[5H` names grid row 4, so this is the run for the band's fifth row.
        let at = Placement {
            left_margin: 0,
            phys_row: 0,
            grid_row: 4,
        };
        let run = b"\x1b[7mAAAAAAAA\x1b[5H\x1b[K";
        let got = clip_row_to_width(run, w, at);

        let mut want = Vec::new();
        want.extend_from_slice(b"\x1b[7mAAAAAAAA");
        want.extend(cup_at(at, 0)); // 8 → 0
        want.extend(std::iter::repeat_n(b' ', usize::from(w)));
        want.extend(cup_at(at, 0));
        assert_eq!(got, want);
    }

    /// A `CUP`'s band-local coordinates are replaced by the band's physical ones: the
    /// run's own row becomes the row the caller positioned to, and the column is
    /// offset by the left margin. Read raw, `ESC[1;10H` would have addressed the
    /// screen's row 0, column 9 — outside the band on both axes.
    #[test]
    fn cup_is_re_expressed_in_physical_coordinates() {
        let at = Placement {
            left_margin: 5,
            phys_row: 3,
            grid_row: 0,
        };
        assert_eq!(
            clip_row_to_width(b"AAA\x1b[1;10Hz", 20, at),
            b"AAA\x1b[4;15Hz".to_vec()
        );
    }

    /// The soft-wrap repair vt100 emits when a row's wrapped flag flips inside one
    /// frame: a full `W = 10` row, then an absolute jump back to the last cell to
    /// restamp it. The rewritten move must land that trailing `9` at the band's last
    /// column of the row the run is painted at — here physical (row 4, column 15).
    #[test]
    fn wrap_repair_cup_stays_in_band() {
        let at = Placement {
            left_margin: 6,
            phys_row: 4,
            grid_row: 0,
        };
        let got = clip_row_to_width(b"0123456789\x1b[1;10H9", 10, at);
        assert_eq!(got, b"0123456789\x1b[5;16H9".to_vec());
    }

    /// The same repair with the band at the screen's origin, where the run's own
    /// coordinates already are the physical ones and the rewrite is byte-for-byte an
    /// identity. This is the case a relative `CUB(1)` gets wrong: ten glyphs into a
    /// ten-column screen leave the real cursor on column 9 with a deferred wrap, not
    /// past the edge, so stepping back one would restamp column 8.
    #[test]
    fn wrap_repair_at_the_screen_origin_is_an_identity() {
        let run = b"0123456789\x1b[1;10H9";
        assert_eq!(clip_row_to_width(run, 10, HOME), run.to_vec());
    }

    /// The target is absolute, so the same column is addressed the same way whichever
    /// side of it the cursor sits — and a move to where the tracker already is emits
    /// nothing at all.
    #[test]
    fn move_targets_the_column_regardless_of_direction() {
        let w = 20u16;
        // col 2 → 8, col 8 → 2, col 8 → 8.
        let forward = clip_row_to_width(b"AA\x1b[1;9Hz", w, HOME);
        let backward = clip_row_to_width(b"AAAAAAAA\x1b[1;3Hz", w, HOME);
        let stationary = clip_row_to_width(b"AAAAAAAA\x1b[1;9Hz", w, HOME);

        let mut want_forward = b"AA".to_vec();
        want_forward.extend(cup(8));
        want_forward.push(b'z');
        let mut want_backward = b"AAAAAAAA".to_vec();
        want_backward.extend(cup(2));
        want_backward.push(b'z');

        assert_eq!(forward, want_forward);
        assert_eq!(backward, want_backward);
        assert_eq!(stationary, b"AAAAAAAAz".to_vec());
    }

    /// A column past the band edge clamps to the band's LAST column, `W - 1`, so no
    /// emitted move can put the cursor — or the glyph that follows it — on the first
    /// gutter column.
    #[test]
    fn out_of_band_column_is_clamped() {
        let w = 10u16;
        let mut want = cup(w - 1);
        want.push(b'z');
        assert_eq!(clip_row_to_width(b"\x1b[1;40Hz", w, HOME), want);
        assert_eq!(clip_row_to_width(b"\x1b[40Gz", w, HOME), want);
    }

    /// The band away from the screen's origin, where an in-band column and a physical
    /// one differ on both axes.
    const OFFSET: Placement = Placement {
        left_margin: 10,
        phys_row: 4,
        grid_row: 4,
    };

    /// `ESC[2K` erases the whole line, which read against the physical row is both
    /// gutters as well as the band. It becomes a fill of the band's own `W` columns
    /// from its column 0, and the cursor goes back to where the erase found it, so the
    /// `z` after it still lands on in-band column 2.
    #[test]
    fn erase_whole_line_fills_the_band_only() {
        let w = 40u16;
        let got = clip_row_to_width(b"\x1b[7mAB\x1b[2Kz", w, OFFSET);

        let mut want = Vec::new();
        want.extend_from_slice(b"\x1b[7mAB");
        want.extend(cup_at(OFFSET, 0));
        want.extend(std::iter::repeat_n(b' ', 40));
        want.extend(cup_at(OFFSET, 2));
        want.push(b'z');
        assert_eq!(got, want);
    }

    /// `ESC[1K` erases from the line's start to the cursor **inclusive** (ECMA-48 EL 1),
    /// so at in-band column 2 the fill is three cells from the band's column 0 — not the
    /// whole physical line up to it. The cursor comes back to column 2, where the `z`
    /// overwrites the last erased cell.
    #[test]
    fn erase_to_cursor_fills_the_cursor_cell_too() {
        let w = 40u16;
        let got = clip_row_to_width(b"\x1b[7mAB\x1b[1Kz", w, OFFSET);

        let mut want = Vec::new();
        want.extend_from_slice(b"\x1b[7mAB");
        want.extend(cup_at(OFFSET, 0));
        want.extend(std::iter::repeat_n(b' ', 3)); // columns 0, 1 and the cursor's own
        want.extend(cup_at(OFFSET, 2));
        want.push(b'z');
        assert_eq!(got, want);
    }

    /// The inclusive cell cannot push the fill past the band: with the cursor on the
    /// last column the fill is exactly `W`, never `W + 1`.
    #[test]
    fn erase_to_cursor_stops_at_the_band_edge() {
        let w = 4u16;
        let got = clip_row_to_width(b"ABC\x1b[1K", w, OFFSET);

        let mut want = b"ABC".to_vec();
        want.extend(cup_at(OFFSET, 0));
        want.extend(std::iter::repeat_n(b' ', usize::from(w)));
        want.extend(cup_at(OFFSET, 3));
        assert_eq!(got, want);
    }

    /// At the pending-wrap column no reposition is emitted at all. The tracker holds
    /// `W`, which as a physical cell is the first gutter column, and the fill has left
    /// the cursor in exactly the state the erase found it — past the band's last cell,
    /// with the wrap deferred where the band's edge is the screen's.
    #[test]
    fn erase_at_the_pending_wrap_column_emits_no_reposition() {
        let w = 4u16;
        for run in [b"ABCD\x1b[1K".as_slice(), b"ABCD\x1b[2K".as_slice()] {
            let got = clip_row_to_width(run, w, OFFSET);
            let mut want = b"ABCD".to_vec();
            want.extend(cup_at(OFFSET, 0));
            want.extend(std::iter::repeat_n(b' ', usize::from(w)));
            assert_eq!(got, want, "{}", String::from_utf8_lossy(run));
        }
    }

    /// An erase takes the run's live background, as a real `EL` would: the `ESC[41m`
    /// set before it is still in force across the fill, and nothing resets it, so the
    /// erased cells come out red and the glyph after them still does too.
    #[test]
    fn the_fill_inherits_the_runs_active_sgr() {
        let got = clip_row_to_width(b"\x1b[41mAB\x1b[2Kz", 40, OFFSET);

        let mut want = Vec::new();
        want.extend_from_slice(b"\x1b[41mAB");
        want.extend(cup_at(OFFSET, 0));
        want.extend(std::iter::repeat_n(b' ', 40));
        want.extend(cup_at(OFFSET, 2));
        want.push(b'z');
        assert_eq!(got, want);
    }

    /// `ESC[3K` erases the scrollback's saved copy of the line and paints nothing, so
    /// there is no band to bound it to — it is dropped, and the run either side of it is
    /// untouched, the tracked column included.
    #[test]
    fn erase_saved_line_is_dropped() {
        let got = clip_row_to_width(b"\x1b[7mAB\x1b[3Kz\x1b[K", 40, OFFSET);

        let mut want = Vec::new();
        want.extend_from_slice(b"\x1b[7mABz");
        want.extend(std::iter::repeat_n(b' ', 37)); // W - 3, so the drop moved no column
        want.extend(cup_at(OFFSET, 3));
        assert_eq!(got, want);
    }

    /// `CHA` (`ESC[…G`) is also absolute — the same desync guard, and the same
    /// rewrite into the band's physical coordinates, for the column-only absolute
    /// move.
    #[test]
    fn cha_is_absolute() {
        let w = 12u16;
        let run = b"\x1b[7mXXXXX\x1b[4G\x1b[K"; // jump to col 4 (0-based 3)
        let got = clip_row_to_width(run, w, HOME);
        let fill = w - 3;
        let mut want = Vec::new();
        want.extend_from_slice(b"\x1b[7mXXXXX");
        want.extend(cup(3)); // 5 → 3
        want.extend(std::iter::repeat_n(b' ', usize::from(fill)));
        want.extend(cup(3));
        assert_eq!(got, want);
        assert!(!has_csi_final(&got, b'G'));
    }

    /// `CHA`'s column is the first param, 1-based, and it too is re-expressed:
    /// `ESC[5G` targets in-band column 4, at the band's own margin.
    #[test]
    fn cha_column_is_first_param() {
        let at = Placement {
            left_margin: 7,
            phys_row: 2,
            grid_row: 2,
        };
        let mut want = b"A".to_vec();
        want.extend(cup_at(at, 4));
        want.push(b'z');
        assert_eq!(clip_row_to_width(b"A\x1b[5Gz", 20, at), want);
    }

    /// `MoveRight` advances the column: `ESC[5C` skips five columns before the
    /// erase, so the fill is `W - 5`.
    #[test]
    fn move_right_advances_column() {
        let w = 10u16;
        let run = b"\x1b[7m\x1b[5C\x1b[K";
        let got = clip_row_to_width(run, w, HOME);
        let fill = w - 5;
        let mut want = Vec::new();
        want.extend_from_slice(b"\x1b[7m\x1b[5C");
        want.extend(std::iter::repeat_n(b' ', usize::from(fill)));
        want.extend(cup(5));
        assert_eq!(got, want);
    }

    /// Backspace retreats the column: three glyphs then a backspace leaves the
    /// cursor at column 2, so the fill is `W - 2`.
    #[test]
    fn backspace_retreats_column() {
        let w = 10u16;
        let run = b"\x1b[7mABC\x08\x1b[K";
        let got = clip_row_to_width(run, w, HOME);
        let fill = w - 2;
        let mut want = Vec::new();
        want.extend_from_slice(b"\x1b[7mABC\x08");
        want.extend(std::iter::repeat_n(b' ', usize::from(fill)));
        want.extend(cup(2));
        assert_eq!(got, want);
    }

    /// The erase at the band edge clips to nothing: with the cursor already at
    /// column `W` the fill is empty and no reposition is emitted, so the `ESC[K`
    /// vanishes.
    #[test]
    fn erase_at_edge_emits_no_fill() {
        let w = 3u16;
        let run = b"\x1b[7mABC\x1b[K"; // col 3 == W
        let got = clip_row_to_width(run, w, HOME);
        assert_eq!(got, b"\x1b[7mABC".to_vec());
    }

    /// A bare `\r` is a move to the start of the row, which read against the physical
    /// row is the left gutter. It becomes an absolute move to the band's own column 0,
    /// and the tracker follows it, so the erase after it fills the whole band.
    #[test]
    fn carriage_return_returns_to_the_bands_column_zero() {
        let w = 40u16;
        let got = clip_row_to_width(b"\x1b[7mAB\rz\x1b[K", w, OFFSET);

        let mut want = Vec::new();
        want.extend_from_slice(b"\x1b[7mAB");
        want.extend(cup_at(OFFSET, 0));
        want.push(b'z');
        want.extend(std::iter::repeat_n(b' ', usize::from(w - 1)));
        want.extend(cup_at(OFFSET, 1));
        assert_eq!(got, want);
        assert!(!got.contains(&b'\r'));
    }

    /// A line feed is dropped rather than written: it would paint the rest of the run a
    /// row below the placement. Nothing else in the run shifts — the tracker still
    /// reads 2, so the erase fills `W - 2`.
    #[test]
    fn line_feed_is_dropped() {
        let w = 40u16;
        let got = clip_row_to_width(b"\x1b[7mAB\n\x1b[K", w, OFFSET);

        let mut want = Vec::new();
        want.extend_from_slice(b"\x1b[7mAB");
        want.extend(std::iter::repeat_n(b' ', usize::from(w - 2)));
        want.extend(cup_at(OFFSET, 2));
        assert_eq!(got, want);
        assert!(!got.contains(&b'\n'));
    }

    /// `EraseChar` moves no column: an interior `ESC[3X` between glyphs and the
    /// trailing erase does not shift the fill length.
    #[test]
    fn erase_char_is_no_move() {
        let w = 10u16;
        let run = b"\x1b[7mAB\x1b[3X\x1b[K"; // col still 2 after the in-place erase
        let got = clip_row_to_width(run, w, HOME);
        let fill = w - 2;
        let mut want = Vec::new();
        want.extend_from_slice(b"\x1b[7mAB\x1b[3X");
        want.extend(std::iter::repeat_n(b' ', usize::from(fill)));
        want.extend(cup(2));
        assert_eq!(got, want);
    }
}
