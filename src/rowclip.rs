//! Clip a vt100 row run to the band width. See ADR-014.
//!
//! [`clip_row_to_width_into`] rewrites the row-final `ESC[K` (erase to right edge)
//! into a `W`-bounded fill so a painted row stays inside its `[0, W)` rectangle
//! rather than flooding the right gutter. To place the fill it tracks a virtual
//! in-band column across the run, honouring absolute moves (`CUP`/`CHA`) as well
//! as relative ones — a rightward-only tracker would desync at a backward jump.

use unicode_width::UnicodeWidthChar;

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

/// Rewrites the row-final `ESC[K` in `run` into a `W`-bounded fill so the run stays
/// within its `[0, W)` rectangle once painted at the band offset. Returns the run
/// unchanged when it carries no `ESC[K`.
#[cfg(test)]
pub fn clip_row_to_width(run: &[u8], w: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(run.len());
    clip_row_to_width_into(run, w, &mut out);
    out
}

/// As [`clip_row_to_width`], but appends to a caller-owned buffer instead of
/// allocating. The paint hot path seeds `out` with the per-row `ESC[m` reset and
/// clips straight into it, building a painted row in one allocation, not two.
pub fn clip_row_to_width_into(run: &[u8], w: u16, out: &mut Vec<u8>) {
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
                        // Forward erase (`ESC[K`/`ESC[0K`) is the only sequence that
                        // floods past `W`. Replace it with a bounded fill under the
                        // active SGR, then a `CUB` so trailing bytes still align.
                        // `ESC[1K`/`ESC[2K` are already bounded — copy them verbatim.
                        if first_param(params, 0) == 0 {
                            let rem = w.saturating_sub(col);
                            if rem > 0 {
                                out.extend(std::iter::repeat_n(b' ', usize::from(rem)));
                                out.extend_from_slice(b"\x1b[");
                                out.extend_from_slice(rem.to_string().as_bytes());
                                out.push(b'D');
                            }
                        } else {
                            out.extend_from_slice(&run[i..=j]);
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
                        let col1 = if params.contains(&b';') {
                            last_param(params, 1)
                        } else {
                            1
                        };
                        col = col1.saturating_sub(1);
                        out.extend_from_slice(&run[i..=j]);
                    }
                    b'G' => {
                        // CHA `ESC[colG`: absolute column, 1-based.
                        col = first_param(params, 1).saturating_sub(1);
                        out.extend_from_slice(&run[i..=j]);
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
            col = col.saturating_sub(1);
            out.push(b);
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

    /// CUB(n) the clip appends to restore the cursor after the bounded fill.
    fn cub(n: u16) -> Vec<u8> {
        format!("\x1b[{n}D").into_bytes()
    }

    /// A run with no `ESC[K` is returned byte-identical; the clip only ever touches
    /// the one unbounded sequence.
    #[test]
    fn no_erase_is_identity() {
        let run = b"\x1b[1;31mhello\x1b[mworld";
        assert_eq!(clip_row_to_width(run, 80), run.to_vec());
    }

    /// Reverse-video full-width row: a leading `ESC[7m` then an immediate `ESC[K`
    /// (the attributed-but-empty statusline) at column 0 clips to exactly `W` spaces
    /// under the reverse SGR plus a `CUB(W)`, so the highlight reaches the band edge
    /// and the cursor is restored.
    #[test]
    fn reverse_video_full_width_clips_to_w_spaces_plus_cub() {
        let w = 10u16;
        let run = b"\x1b[7m\x1b[K";
        let got = clip_row_to_width(run, w);

        let mut want = Vec::new();
        want.extend_from_slice(b"\x1b[7m");
        want.extend(std::iter::repeat_n(b' ', usize::from(w)));
        want.extend(cub(w));
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
        let got = clip_row_to_width(run, w);

        let mut want = Vec::new();
        want.extend_from_slice(b"\x1b[7mABC");
        want.extend(std::iter::repeat_n(b' ', 7)); // W - 3
        want.extend(cub(7));
        assert_eq!(got, want);
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
        let got = clip_row_to_width(&run, w);

        let mut want = Vec::new();
        want.extend_from_slice(b"\x1b[7m");
        want.extend_from_slice("\u{4e00}".as_bytes());
        want.extend(std::iter::repeat_n(b' ', 2)); // 4 - 2
        want.extend(cub(2));
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
        let got = clip_row_to_width(run, w);

        let fill = w - 2; // from the absolute column 2, NOT from 8
        let mut want = Vec::new();
        want.extend_from_slice(b"\x1b[7mAAAAAAAA\x1b[1;3H");
        want.extend(std::iter::repeat_n(b' ', usize::from(fill)));
        want.extend(cub(fill));
        assert_eq!(got, want);
    }

    /// Row-only `CUP` defaults the column to 1: `ESC[5H` moves to row 5, column
    /// default (in-band column 0), not column 4. A run that prints to column 8, then
    /// a row-only `CUP`, then erases must fill the full `W`.
    #[test]
    fn row_only_cup_defaults_column_to_zero() {
        let w = 20u16;
        let run = b"\x1b[7mAAAAAAAA\x1b[5H\x1b[K";
        let got = clip_row_to_width(run, w);

        let mut want = Vec::new();
        want.extend_from_slice(b"\x1b[7mAAAAAAAA\x1b[5H");
        want.extend(std::iter::repeat_n(b' ', usize::from(w)));
        want.extend(cub(w));
        assert_eq!(got, want);
    }

    /// Only the forward erase is clipped. `ESC[1K` (erase-left) and `ESC[2K`
    /// (erase-whole-line) are already bounded, so they pass through verbatim.
    #[test]
    fn non_forward_erase_is_copied_verbatim() {
        let w = 10u16;
        for run in [
            b"\x1b[7mAB\x1b[1K".as_slice(),
            b"\x1b[7mAB\x1b[2K".as_slice(),
        ] {
            assert_eq!(clip_row_to_width(run, w), run.to_vec());
        }
    }

    /// `CHA` (`ESC[…G`) is also absolute — the same desync guard, for the
    /// column-only absolute move.
    #[test]
    fn cha_is_absolute() {
        let w = 12u16;
        let run = b"\x1b[7mXXXXX\x1b[4G\x1b[K"; // jump to col 4 (0-based 3)
        let got = clip_row_to_width(run, w);
        let fill = w - 3;
        let mut want = Vec::new();
        want.extend_from_slice(b"\x1b[7mXXXXX\x1b[4G");
        want.extend(std::iter::repeat_n(b' ', usize::from(fill)));
        want.extend(cub(fill));
        assert_eq!(got, want);
    }

    /// `MoveRight` advances the column: `ESC[5C` skips five columns before the
    /// erase, so the fill is `W - 5`.
    #[test]
    fn move_right_advances_column() {
        let w = 10u16;
        let run = b"\x1b[7m\x1b[5C\x1b[K";
        let got = clip_row_to_width(run, w);
        let fill = w - 5;
        let mut want = Vec::new();
        want.extend_from_slice(b"\x1b[7m\x1b[5C");
        want.extend(std::iter::repeat_n(b' ', usize::from(fill)));
        want.extend(cub(fill));
        assert_eq!(got, want);
    }

    /// Backspace retreats the column: three glyphs then a backspace leaves the
    /// cursor at column 2, so the fill is `W - 2`.
    #[test]
    fn backspace_retreats_column() {
        let w = 10u16;
        let run = b"\x1b[7mABC\x08\x1b[K";
        let got = clip_row_to_width(run, w);
        let fill = w - 2;
        let mut want = Vec::new();
        want.extend_from_slice(b"\x1b[7mABC\x08");
        want.extend(std::iter::repeat_n(b' ', usize::from(fill)));
        want.extend(cub(fill));
        assert_eq!(got, want);
    }

    /// The erase at the band edge clips to nothing: with the cursor already at
    /// column `W` the fill is empty and no `CUB` is emitted, so the `ESC[K` vanishes.
    #[test]
    fn erase_at_edge_emits_no_fill() {
        let w = 3u16;
        let run = b"\x1b[7mABC\x1b[K"; // col 3 == W
        let got = clip_row_to_width(run, w);
        assert_eq!(got, b"\x1b[7mABC".to_vec());
    }

    /// `EraseChar` moves no column: an interior `ESC[3X` between glyphs and the
    /// trailing erase does not shift the fill length.
    #[test]
    fn erase_char_is_no_move() {
        let w = 10u16;
        let run = b"\x1b[7mAB\x1b[3X\x1b[K"; // col still 2 after the in-place erase
        let got = clip_row_to_width(run, w);
        let fill = w - 2;
        let mut want = Vec::new();
        want.extend_from_slice(b"\x1b[7mAB\x1b[3X");
        want.extend(std::iter::repeat_n(b' ', usize::from(fill)));
        want.extend(cub(fill));
        assert_eq!(got, want);
    }
}
