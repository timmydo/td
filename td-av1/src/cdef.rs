//! The AV1 constrained directional enhancement filter (spec 7.15) over a
//! deblocked frame, as a decoder runs it after deblocking: each 8x8 of
//! luma with coefficients is filtered along the direction its luma
//! shows, and its 4x4s of chroma with it, every tap reading the frame as
//! deblocking left it. One strength set serves the whole frame
//! (`cdef_bits` 0), so the tiles code no `cdef_idx`. The kernels are
//! dav1d 1.5.1's `cdef_tmpl.c` and `cdef_apply_tmpl.c`; like deblocking,
//! the filter changes only what the decoder shows, never what the
//! encoder predicts from.

use crate::deblock::Plane;

/// A frame's CDEF strengths as `cdef_params` codes them with one preset:
/// the damping (3 to 6), then luma's and chroma's primary strength (0 to
/// 15) and secondary strength (0 to 3, 3 meaning 4).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Strengths {
    pub damping: u8,
    pub y_pri: u8,
    pub y_sec: u8,
    pub uv_pri: u8,
    pub uv_sec: u8,
}

impl Strengths {
    /// Whether the filter leaves every pixel as it is.
    pub fn is_off(self) -> bool {
        self.y_pri == 0 && self.y_sec == 0 && self.uv_pri == 0 && self.uv_sec == 0
    }
}

/// `Cdef_Directions`: each direction's two tap offsets, row then column.
const DIRECTIONS: [[(isize, isize); 2]; 8] = [
    [(-1, 1), (-2, 2)],
    [(0, 1), (-1, 2)],
    [(0, 1), (0, 2)],
    [(0, 1), (1, 2)],
    [(1, 1), (2, 2)],
    [(1, 0), (2, 1)],
    [(1, 0), (2, 0)],
    [(1, 0), (2, -1)],
];

/// A plane as the filter reads it: the frame before the filter, with
/// only the pixels of the frame's 8x8 grid available.
struct View<'a> {
    data: &'a [u8],
    stride: usize,
    rows: usize,
    cols: usize,
}

impl View<'_> {
    fn at(&self, row: isize, col: isize) -> Option<i32> {
        let (row, col) = (usize::try_from(row).ok()?, usize::try_from(col).ok()?);
        if row >= self.rows || col >= self.cols {
            return None;
        }
        self.data
            .get(row * self.stride + col)
            .map(|&p| i32::from(p))
    }
}

/// Filters the three planes (4:2:0) in place. `mi_rows` and `mi_cols`
/// are the frame's 4x4 counts, both even: the filter covers their 8x8
/// grid and reads no pixel outside it. `skip(row, col)` says the 8x8 of
/// luma at that place in the grid had no coefficients: all four of its
/// 4x4s' blocks skipped.
pub fn filter(
    planes: [Plane<'_>; 3],
    strengths: Strengths,
    mi_rows: usize,
    mi_cols: usize,
    skip: impl Fn(usize, usize) -> bool,
) {
    if strengths.is_off() {
        return;
    }
    let sources: Vec<Vec<u8>> = planes.iter().map(|p| p.data.to_vec()).collect();
    let [mut y, mut u, mut v] = planes;
    let view = |plane: usize, p: &Plane<'_>| View {
        data: sources.get(plane).map_or(&[], |s| s.as_slice()),
        stride: p.stride,
        rows: (mi_rows * 4) >> usize::from(plane > 0),
        cols: (mi_cols * 4) >> usize::from(plane > 0),
    };
    let (vy, vu, vv) = (view(0, &y), view(1, &u), view(2, &v));
    let damping = i32::from(strengths.damping);
    let y_pri = i32::from(strengths.y_pri);
    let y_sec = secondary(strengths.y_sec);
    let uv_pri = i32::from(strengths.uv_pri);
    let uv_sec = secondary(strengths.uv_sec);
    for row in 0..mi_rows / 2 {
        for col in 0..mi_cols / 2 {
            if skip(row, col) {
                continue;
            }
            let (dir, var) = if y_pri > 0 || uv_pri > 0 {
                direction(&vy, row * 8, col * 8)
            } else {
                (0, 0)
            };
            if y_pri > 0 {
                let adjusted = adjust(y_pri, var);
                if adjusted > 0 || y_sec > 0 {
                    block(
                        &vy,
                        &mut y,
                        (row * 8, col * 8, 8),
                        (adjusted, y_sec),
                        dir,
                        damping,
                    );
                }
            } else if y_sec > 0 {
                block(&vy, &mut y, (row * 8, col * 8, 8), (0, y_sec), 0, damping);
            }
            if uv_pri > 0 || uv_sec > 0 {
                let dir = if uv_pri > 0 { dir } else { 0 };
                let at = (row * 4, col * 4, 4);
                block(&vu, &mut u, at, (uv_pri, uv_sec), dir, damping - 1);
                block(&vv, &mut v, at, (uv_pri, uv_sec), dir, damping - 1);
            }
        }
    }
}

/// A coded secondary strength's value: 3 means 4.
fn secondary(coded: u8) -> i32 {
    let s = i32::from(coded);
    s + i32::from(s == 3)
}

/// The luma primary strength by the block's variance (dav1d's
/// `adjust_strength`): none on a flat block, more on a busy one.
fn adjust(strength: i32, var: u32) -> i32 {
    if var == 0 {
        return 0;
    }
    let i = if var >> 6 != 0 {
        (var >> 6).ilog2().min(12)
    } else {
        0
    };
    (strength * (4 + i as i32) + 8) >> 4
}

/// The spec's `constrain`: a tap's difference, cut back as it passes the
/// threshold, `shift` the damping less the threshold's log.
fn constrain(diff: i32, threshold: i32, shift: u32) -> i32 {
    let magnitude = diff.abs();
    let kept = magnitude.min((threshold - (magnitude >> shift)).max(0));
    if diff < 0 {
        -kept
    } else {
        kept
    }
}

/// Floor of log2 of a positive strength.
fn log2(v: i32) -> i32 {
    31 - v.max(1).leading_zeros() as i32
}

/// Filters one block of side `n` at `(row, col, n)` from the frame
/// before the filter into `dst` (spec `cdef_filter`): the primary taps
/// along `dir`, the secondary ones two directions either side, each tap
/// outside the frame's grid left out, the sum rounded to nearest with
/// ties away from zero and kept within the taps' range.
fn block(
    src: &View<'_>,
    dst: &mut Plane<'_>,
    (row, col, n): (usize, usize, usize),
    (pri, sec): (i32, i32),
    dir: usize,
    damping: i32,
) {
    let pri_taps = if pri & 1 == 0 { [4, 2] } else { [3, 3] };
    // The strengths and damping are the header's (damping 2 to 6 here),
    // but a shift stays within the type whatever a caller passes.
    let pri_shift = (damping - log2(pri)).clamp(0, 31) as u32;
    let sec_shift = (damping - log2(sec)).clamp(0, 31) as u32;
    let primary = DIRECTIONS.get(dir).copied().unwrap_or_default();
    let secondaries = [
        DIRECTIONS.get((dir + 2) & 7).copied().unwrap_or_default(),
        DIRECTIONS.get((dir + 6) & 7).copied().unwrap_or_default(),
    ];
    for i in 0..n {
        for j in 0..n {
            let (y, x) = ((row + i) as isize, (col + j) as isize);
            let Some(px) = src.at(y, x) else {
                continue;
            };
            let (mut sum, mut min, mut max) = (0, px, px);
            let mut tap =
                |offset: (isize, isize), sign: isize, weight: i32, strength: i32, shift: u32| {
                    if let Some(p) = src.at(y + sign * offset.0, x + sign * offset.1) {
                        sum += weight * constrain(p - px, strength, shift);
                        min = min.min(p);
                        max = max.max(p);
                    }
                };
            for (k, &offset) in primary.iter().enumerate() {
                if pri > 0 {
                    let pri_weight = pri_taps.get(k).copied().unwrap_or(0);
                    tap(offset, 1, pri_weight, pri, pri_shift);
                    tap(offset, -1, pri_weight, pri, pri_shift);
                }
                if sec > 0 {
                    let sec_weight = 2 - k as i32;
                    for &offset in secondaries.iter().filter_map(|d| d.get(k)) {
                        tap(offset, 1, sec_weight, sec, sec_shift);
                        tap(offset, -1, sec_weight, sec, sec_shift);
                    }
                }
            }
            let out = (px + ((8 + sum - i32::from(sum < 0)) >> 4)).clamp(min, max);
            if let Some(slot) = dst.data.get_mut((row + i) * dst.stride + col + j) {
                *slot = out as u8;
            }
        }
    }
}

/// The direction of an 8x8 of luma and how strongly it holds (spec
/// `cdef_find_dir`, dav1d's `cdef_find_dir_c`): the line sums along each
/// of the eight directions, weighted by their lengths, and the best
/// cost's lead over the orthogonal direction's.
fn direction(src: &View<'_>, row: usize, col: usize) -> (usize, u32) {
    let mut hv = [[0i32; 8]; 2];
    let mut diag = [[0i32; 15]; 2];
    let mut alt = [[0i32; 11]; 4];
    for y in 0..8 {
        for x in 0..8 {
            let px = src
                .at((row + y) as isize, (col + x) as isize)
                .unwrap_or(128)
                - 128;
            let add = |line: &mut [i32], k: usize| {
                if let Some(slot) = line.get_mut(k) {
                    *slot += px;
                }
            };
            let [diag0, diag1] = &mut diag;
            add(diag0, y + x);
            add(diag1, 7 + y - x);
            let [alt0, alt1, alt2, alt3] = &mut alt;
            add(alt0, y + (x >> 1));
            add(alt1, 3 + y - (x >> 1));
            add(alt2, 3 - (y >> 1) + x);
            add(alt3, (y >> 1) + x);
            let [hv0, hv1] = &mut hv;
            add(hv0, y);
            add(hv1, x);
        }
    }
    // A line's sums squared, each divided by its length: the full ones
    // at 105 (840 / 8), the shorter ones from both ends inward.
    const DIV: [u32; 7] = [840, 420, 280, 210, 168, 140, 120];
    let sq = |v: &i32| (v * v) as u32;
    let full = |line: &[i32]| line.iter().map(sq).sum::<u32>() * 105;
    let ends = |line: &[i32], divs: &mut dyn Iterator<Item = &u32>| -> u32 {
        line.iter()
            .zip(line.iter().rev())
            .zip(divs)
            .map(|((a, b), d)| (sq(a) + sq(b)) * d)
            .sum()
    };
    let diagonal =
        |line: &[i32; 15]| ends(line, &mut DIV.iter()) + line.get(7).map_or(0, |v| sq(v) * 105);
    let alternate = |line: &[i32; 11]| {
        full(line.get(3..8).unwrap_or(&[])) + ends(line, &mut DIV.iter().skip(1).step_by(2))
    };
    let [hv0, hv1] = &hv;
    let [diag0, diag1] = &diag;
    let [alt0, alt1, alt2, alt3] = &alt;
    let cost = [
        diagonal(diag0),
        alternate(alt0),
        full(hv0),
        alternate(alt1),
        diagonal(diag1),
        alternate(alt2),
        full(hv1),
        alternate(alt3),
    ];
    let mut best = (0, 0u32);
    for (n, &c) in cost.iter().enumerate() {
        if n == 0 || c > best.1 {
            best = (n, c);
        }
    }
    let orthogonal = cost.get(best.0 ^ 4).copied().unwrap_or(0);
    (best.0, (best.1 - orthogonal) >> 10)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(data: &[u8]) -> View<'_> {
        View {
            data,
            stride: 8,
            rows: 8,
            cols: 8,
        }
    }

    #[test]
    fn every_direction_is_found_along_its_lines() {
        // A ramp across the lines of one direction's partial sums, flat
        // along them, makes that direction the best (dav1d's line
        // indices: 0 and 4 the diagonals, 2 rows, 6 columns, the odd
        // ones the half-slopes between).
        let lines: [fn(usize, usize) -> usize; 8] = [
            |y, x| y + x,
            |y, x| y + (x >> 1),
            |y, _| y,
            |y, x| 3 + y - (x >> 1),
            |y, x| 7 + y - x,
            |y, x| 3 - (y >> 1) + x,
            |_, x| x,
            |y, x| (y >> 1) + x,
        ];
        for (dir, line) in lines.iter().enumerate() {
            let data: Vec<u8> = (0..64)
                .map(|i| (20 + 12 * line(i / 8, i % 8)) as u8)
                .collect();
            let (found, var) = direction(&view(&data), 0, 0);
            assert_eq!(found, dir);
            assert!(var > 0, "{dir}");
        }
    }

    #[test]
    fn lines_give_their_direction() {
        // Stripes along a direction make it the best: horizontal stripes
        // (constant rows) are direction 2, vertical ones direction 6.
        let rows: Vec<u8> = (0..64)
            .map(|i| if (i / 8) % 2 == 0 { 40 } else { 200 })
            .collect();
        assert_eq!(direction(&view(&rows), 0, 0).0, 2);
        let cols: Vec<u8> = (0..64)
            .map(|i| if (i % 8) % 2 == 0 { 40 } else { 200 })
            .collect();
        assert_eq!(direction(&view(&cols), 0, 0).0, 6);
        let flat = [90u8; 64];
        assert_eq!(direction(&view(&flat), 0, 0), (0, 0));
    }

    #[test]
    fn constrain_cuts_back_large_differences() {
        // Within the threshold a difference passes; past it, it shrinks
        // by the damped magnitude and vanishes at threshold << shift.
        assert_eq!(constrain(3, 4, 2), 3);
        assert_eq!(constrain(-3, 4, 2), -3);
        assert_eq!(constrain(8, 4, 2), 2);
        assert_eq!(constrain(-16, 4, 2), 0);
        assert_eq!(adjust(8, 0), 0);
        assert_eq!(adjust(8, 63), 2);
        assert_eq!(adjust(8, 64 << 12), 8);
        assert_eq!(secondary(3), 4);
    }
}
