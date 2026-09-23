//! The AV1 deblocking filter (spec 7.14) over a frame the encoder
//! reconstructed, as a decoder runs it once the frame is decoded: every
//! transform edge inside the picture, a plane's vertical edges and then
//! its horizontal ones, the kernels libaom 3.9.1's `aom_dsp/loopfilter.c`
//! and dav1d apply. Intra prediction reads the frame before this runs, so
//! the encoder's decisions never see it; only what the decoder shows
//! does.
//!
//! The blocks here are square, one transform the block's size, intra
//! (never skipped for the filter), without segmentation, loop filter
//! deltas or sharpness, so an edge's strength is the plane's level and
//! its length the smaller transform's: 4, 8 or 14 taps for luma, 4 or 6
//! for chroma.

/// A frame's deblocking levels (spec `loop_filter_params`): luma's for
/// vertical and for horizontal edges, then U's and V's. Both luma levels
/// zero turn the whole filter off.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Levels {
    pub luma: [u8; 2],
    pub u: u8,
    pub v: u8,
}

/// One plane in place: rows `stride` apart covering every whole block,
/// of which `width` by `height` is the picture.
pub struct Plane<'a> {
    pub data: &'a mut [u8],
    pub stride: usize,
    pub width: usize,
    pub height: usize,
}

/// Filters the three planes (4:2:0) in place. `block_log2(mi_row,
/// mi_col)` is the log2 of the luma side of the block over a luma 4x4.
pub fn filter(planes: [Plane<'_>; 3], levels: Levels, block_log2: impl Fn(usize, usize) -> u32) {
    if levels.luma == [0, 0] {
        return;
    }
    for (plane, p) in planes.into_iter().enumerate() {
        let sub = usize::from(plane > 0);
        for vertical in [true, false] {
            let level = match (plane, vertical) {
                (0, true) => levels.luma[0],
                (0, false) => levels.luma[1],
                (1, _) => levels.u,
                _ => levels.v,
            };
            if level == 0 {
                continue;
            }
            let limits = Limits::new(level);
            // The log2 of the transform's side in pixels over a plane 4x4,
            // read from the luma 4x4 at its bottom right as libaom does.
            let tx_log2 = |x: usize, y: usize| {
                block_log2(((y << sub) >> 2) | sub, ((x << sub) >> 2) | sub)
                    .min(6)
                    .saturating_sub(sub as u32)
            };
            for y in (0..p.height).step_by(4) {
                for x in (0..p.width).step_by(4) {
                    let coord = if vertical { x } else { y };
                    let current = tx_log2(x, y);
                    if coord == 0 || coord & ((1 << current) - 1) != 0 {
                        continue;
                    }
                    let previous = if vertical {
                        tx_log2(x - 4, y)
                    } else {
                        tx_log2(x, y - 4)
                    };
                    let length = match (plane, current.min(previous)) {
                        (_, 0..=2) => 4,
                        (0, 3) => 8,
                        (0, _) => 14,
                        (_, _) => 6,
                    };
                    for i in 0..4 {
                        let (at, step) = if vertical {
                            ((y + i) * p.stride + x, 1)
                        } else {
                            (y * p.stride + x + i, p.stride)
                        };
                        edge(p.data, at, step, length, limits);
                    }
                }
            }
        }
    }
}

/// An edge's thresholds from its level (libaom `update_sharpness` at
/// sharpness 0, and the high edge variance threshold).
#[derive(Clone, Copy)]
struct Limits {
    limit: i32,
    blimit: i32,
    thresh: i32,
}

impl Limits {
    fn new(level: u8) -> Limits {
        let level = i32::from(level);
        let limit = level.max(1);
        Limits {
            limit,
            blimit: 2 * (level + 2) + limit,
            thresh: level >> 4,
        }
    }
}

/// Filters one line across an edge: `data[at]` is the first pixel past
/// it (`q0`), `step` the distance to the next, `length` taps. A line any
/// of whose taps falls outside `data` is left alone.
fn edge(data: &mut [u8], at: usize, step: usize, length: usize, limits: Limits) {
    let half = length.div_ceil(2).min(7);
    let Some(first) = at.checked_sub(half * step) else {
        return;
    };
    // `px[6 - k]` is `p_k` and `px[7 + k]` is `q_k`; the line's taps are
    // `px[7 - half..7 + half]`, `first` the pixel under the first.
    let mut px = [0i32; 14];
    let Some(taps) = px.get_mut(7 - half..7 + half) else {
        return;
    };
    for (i, slot) in taps.iter_mut().enumerate() {
        match data.get(first + i * step) {
            Some(&v) => *slot = i32::from(v),
            None => return,
        }
    }
    match length {
        4 => filter4_line(&mut px, limits),
        6 => filter6_line(&mut px, limits),
        8 => filter8_line(&mut px, limits),
        _ => filter14_line(&mut px, limits),
    }
    for (i, &v) in px.iter().skip(7 - half).take(2 * half).enumerate() {
        if let Some(slot) = data.get_mut(first + i * step) {
            *slot = v.clamp(0, 255) as u8;
        }
    }
}

/// `p_k` and `q_k` of a line.
fn p(px: &[i32; 14], k: usize) -> i32 {
    px.get(6 - k).copied().unwrap_or(0)
}

fn q(px: &[i32; 14], k: usize) -> i32 {
    px.get(7 + k).copied().unwrap_or(0)
}

fn set_p(px: &mut [i32; 14], k: usize, v: i32) {
    if let Some(slot) = px.get_mut(6 - k) {
        *slot = v;
    }
}

fn set_q(px: &mut [i32; 14], k: usize, v: i32) {
    if let Some(slot) = px.get_mut(7 + k) {
        *slot = v;
    }
}

/// `ROUND_POWER_OF_TWO`.
fn round(v: i32, n: u32) -> i32 {
    (v + (1 << (n - 1))) >> n
}

/// Whether every difference is within the threshold.
fn within(diffs: &[i32], limit: i32) -> bool {
    diffs.iter().all(|d| d.abs() <= limit)
}

/// libaom's `filter_mask` over `p_(taps-1)..q_(taps-1)`: the steps
/// within `limit`, the edge within `blimit`.
fn mask(px: &[i32; 14], taps: usize, limits: Limits) -> bool {
    let steps = (1..taps).all(|k| {
        within(
            &[p(px, k) - p(px, k - 1), q(px, k) - q(px, k - 1)],
            limits.limit,
        )
    });
    steps && (p(px, 0) - q(px, 0)).abs() * 2 + (p(px, 1) - q(px, 1)).abs() / 2 <= limits.blimit
}

/// libaom's flat masks: each of the taps within 1 of the edge pixels.
fn flat(px: &[i32; 14], taps: &[usize]) -> bool {
    taps.iter()
        .all(|&k| within(&[p(px, k) - p(px, 0), q(px, k) - q(px, 0)], 1))
}

/// `filter4`: the narrow filter, in libaom's signed-byte arithmetic.
fn filter4(px: &mut [i32; 14], limits: Limits) {
    let clamp = |t: i32| t.clamp(-128, 127);
    let (ps1, ps0) = (p(px, 1) - 128, p(px, 0) - 128);
    let (qs0, qs1) = (q(px, 0) - 128, q(px, 1) - 128);
    let hev = !within(&[p(px, 1) - p(px, 0), q(px, 1) - q(px, 0)], limits.thresh);
    let outer = if hev { clamp(ps1 - qs1) } else { 0 };
    let filter = clamp(outer + 3 * (qs0 - ps0));
    let filter1 = clamp(filter + 4) >> 3;
    let filter2 = clamp(filter + 3) >> 3;
    set_q(px, 0, clamp(qs0 - filter1) + 128);
    set_p(px, 0, clamp(ps0 + filter2) + 128);
    if !hev {
        let filter = round(filter1, 1);
        set_q(px, 1, clamp(qs1 - filter) + 128);
        set_p(px, 1, clamp(ps1 + filter) + 128);
    }
}

fn filter4_line(px: &mut [i32; 14], limits: Limits) {
    if mask(px, 2, limits) {
        filter4(px, limits);
    }
}

fn filter6_line(px: &mut [i32; 14], limits: Limits) {
    if !mask(px, 3, limits) {
        return;
    }
    if !flat(px, &[1, 2]) {
        filter4(px, limits);
        return;
    }
    let (p2, p1, p0) = (p(px, 2), p(px, 1), p(px, 0));
    let (q0, q1, q2) = (q(px, 0), q(px, 1), q(px, 2));
    set_p(px, 1, round(p2 * 3 + p1 * 2 + p0 * 2 + q0, 3));
    set_p(px, 0, round(p2 + p1 * 2 + p0 * 2 + q0 * 2 + q1, 3));
    set_q(px, 0, round(p1 + p0 * 2 + q0 * 2 + q1 * 2 + q2, 3));
    set_q(px, 1, round(p0 + q0 * 2 + q1 * 2 + q2 * 3, 3));
}

/// `filter8` once its mask holds.
fn filter8(px: &mut [i32; 14], limits: Limits) {
    if !flat(px, &[1, 2, 3]) {
        filter4(px, limits);
        return;
    }
    let (p3, p2, p1, p0) = (p(px, 3), p(px, 2), p(px, 1), p(px, 0));
    let (q0, q1, q2, q3) = (q(px, 0), q(px, 1), q(px, 2), q(px, 3));
    set_p(px, 2, round(p3 * 3 + 2 * p2 + p1 + p0 + q0, 3));
    set_p(px, 1, round(p3 * 2 + p2 + 2 * p1 + p0 + q0 + q1, 3));
    set_p(px, 0, round(p3 + p2 + p1 + 2 * p0 + q0 + q1 + q2, 3));
    set_q(px, 0, round(p2 + p1 + p0 + 2 * q0 + q1 + q2 + q3, 3));
    set_q(px, 1, round(p1 + p0 + q0 + 2 * q1 + q2 + q3 * 2, 3));
    set_q(px, 2, round(p0 + q0 + q1 + 2 * q2 + q3 * 3, 3));
}

fn filter8_line(px: &mut [i32; 14], limits: Limits) {
    if mask(px, 4, limits) {
        filter8(px, limits);
    }
}

fn filter14_line(px: &mut [i32; 14], limits: Limits) {
    if !mask(px, 4, limits) {
        return;
    }
    if !(flat(px, &[1, 2, 3]) && flat(px, &[4, 5, 6])) {
        filter8(px, limits);
        return;
    }
    let [p6, p5, p4, p3, p2, p1, p0, q0, q1, q2, q3, q4, q5, q6] = *px;
    set_p(
        px,
        5,
        round(p6 * 7 + p5 * 2 + p4 * 2 + p3 + p2 + p1 + p0 + q0, 4),
    );
    set_p(
        px,
        4,
        round(
            p6 * 5 + p5 * 2 + p4 * 2 + p3 * 2 + p2 + p1 + p0 + q0 + q1,
            4,
        ),
    );
    set_p(
        px,
        3,
        round(
            p6 * 4 + p5 + p4 * 2 + p3 * 2 + p2 * 2 + p1 + p0 + q0 + q1 + q2,
            4,
        ),
    );
    set_p(
        px,
        2,
        round(
            p6 * 3 + p5 + p4 + p3 * 2 + p2 * 2 + p1 * 2 + p0 + q0 + q1 + q2 + q3,
            4,
        ),
    );
    set_p(
        px,
        1,
        round(
            p6 * 2 + p5 + p4 + p3 + p2 * 2 + p1 * 2 + p0 * 2 + q0 + q1 + q2 + q3 + q4,
            4,
        ),
    );
    set_p(
        px,
        0,
        round(
            p6 + p5 + p4 + p3 + p2 + p1 * 2 + p0 * 2 + q0 * 2 + q1 + q2 + q3 + q4 + q5,
            4,
        ),
    );
    set_q(
        px,
        0,
        round(
            p5 + p4 + p3 + p2 + p1 + p0 * 2 + q0 * 2 + q1 * 2 + q2 + q3 + q4 + q5 + q6,
            4,
        ),
    );
    set_q(
        px,
        1,
        round(
            p4 + p3 + p2 + p1 + p0 + q0 * 2 + q1 * 2 + q2 * 2 + q3 + q4 + q5 + q6 * 2,
            4,
        ),
    );
    set_q(
        px,
        2,
        round(
            p3 + p2 + p1 + p0 + q0 + q1 * 2 + q2 * 2 + q3 * 2 + q4 + q5 + q6 * 3,
            4,
        ),
    );
    set_q(
        px,
        3,
        round(
            p2 + p1 + p0 + q0 + q1 + q2 * 2 + q3 * 2 + q4 * 2 + q5 + q6 * 4,
            4,
        ),
    );
    set_q(
        px,
        4,
        round(
            p1 + p0 + q0 + q1 + q2 + q3 * 2 + q4 * 2 + q5 * 2 + q6 * 5,
            4,
        ),
    );
    set_q(
        px,
        5,
        round(p0 + q0 + q1 + q2 + q3 + q4 * 2 + q5 * 2 + q6 * 7, 4),
    );
}
