//! Antialiased coverage of a quadratic outline (`sfnt::Outline`): each
//! pixel's alpha is the exact area the outline's edges enclose inside it,
//! accumulated per row from signed edge contributions, curves flattened to
//! within a thirty-second of a pixel. The fill is nonzero: a pixel inside
//! any winding is covered whole, whatever the count. An edge pixel shared
//! by two overlapping contours sums their areas and so reads darker than
//! their union, as FreeType's smooth rasterizer does; like it, an outline
//! the font flags as overlapping is covered on a grid four times finer
//! each way, where that sum resolves. No hinting, no gamma: alpha is
//! linear area. Nothing here reads the environment, a clock, a descriptor
//! or the filesystem.

use crate::sfnt::{Error, Outline, Point};

/// A mask's width and height; past it the outline is refused.
pub const MAX_MASK_AXIS: usize = 512;
/// A flagged outline's final mask axes at most, for the finer grid: its
/// accumulator is then at most 514 by 512.
pub const MAX_OVERSAMPLED_AXIS: usize = 128;
const OVERSAMPLE: usize = 4;
/// Line segments one quadratic curve is flattened into, at most: enough
/// for the thirty-second bound on any curve whose points fit a mask (the
/// largest second difference inside 512 pixels square is 1449 pixels).
const MAX_CURVE_SEGMENTS: usize = 128;

/// One glyph's coverage: `width * height` alpha bytes, row-major, top row
/// first, placed `left` pixels right of the pen and with its top row
/// `top` pixels above the baseline.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Mask {
    pub width: usize,
    pub height: usize,
    pub left: i32,
    pub top: i32,
    pub alpha: Vec<u8>,
}

impl Mask {
    /// Empty, keeping the allocation.
    pub fn clear(&mut self) {
        (self.width, self.height, self.left, self.top) = (0, 0, 0, 0);
        self.alpha.clear();
    }

    /// The alpha at a column and row, zero outside the mask.
    pub fn get(&self, x: usize, y: usize) -> u8 {
        if x >= self.width || y >= self.height {
            return 0;
        }
        y.checked_mul(self.width)
            .and_then(|row| row.checked_add(x))
            .and_then(|at| self.alpha.get(at))
            .copied()
            .unwrap_or(0)
    }
}

/// The area accumulator, kept between glyphs so a steady state allocates
/// nothing.
#[derive(Clone, Debug, Default)]
pub struct Rasterizer {
    area: Vec<f32>,
    columns: Vec<f32>,
    stride: usize,
    height: usize,
}

impl Rasterizer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Covers `outline` at `scale` pixels per font unit into `mask`,
    /// replacing what it held. An empty or zero-area outline is an empty
    /// mask; a scale that is not positive and finite is refused, and so is
    /// a mask past `MAX_MASK_AXIS`, before `mask` is touched.
    pub fn rasterize(
        &mut self,
        outline: &Outline,
        scale: f32,
        mask: &mut Mask,
    ) -> Result<(), Error> {
        if !(scale.is_finite() && scale > 0.0) {
            return Err(Error::Malformed("coverage scale"));
        }
        let mut bounds: Option<(f32, f32, f32, f32)> = None;
        for point in outline.points() {
            let (x, y) = (point.x * scale, point.y * scale);
            if !(x.is_finite() && y.is_finite()) {
                return Err(Error::Limit("coverage"));
            }
            bounds = Some(match bounds {
                None => (x, y, x, y),
                Some((left, bottom, right, top)) => {
                    (left.min(x), bottom.min(y), right.max(x), top.max(y))
                }
            });
        }
        let Some((left, bottom, right, top)) = bounds else {
            mask.clear();
            return Ok(());
        };
        let (left, bottom, right, top) = (left.floor(), bottom.floor(), right.ceil(), top.ceil());
        let axis = MAX_MASK_AXIS as f32;
        if right - left > axis || top - bottom > axis || left.abs() > 1e6 || top.abs() > 1e6 {
            return Err(Error::Limit("coverage"));
        }
        // Bounded above, so these conversions are exact.
        let width = (right - left) as usize;
        let height = (top - bottom) as usize;
        mask.clear();
        if width == 0 || height == 0 {
            return Ok(());
        }
        mask.width = width;
        mask.height = height;
        mask.left = left as i32;
        mask.top = top as i32;
        let factor =
            if outline.overlap() && width <= MAX_OVERSAMPLED_AXIS && height <= MAX_OVERSAMPLED_AXIS
            {
                OVERSAMPLE
            } else {
                1
            };
        let (columns, rows) = (width * factor, height * factor);
        // Two spare columns: an edge's remainder lands right of the last
        // pixel and is never read, so rows need no carry between them.
        self.stride = columns + 2;
        self.height = rows;
        self.area.clear();
        self.area.resize(self.stride * rows, 0.0);
        let (w, h, f) = (columns as f32, rows as f32, factor as f32);
        let place = |point: &Point| {
            (
                ((point.x * scale - left) * f).clamp(0.0, w),
                ((top - point.y * scale) * f).clamp(0.0, h),
            )
        };
        for contour in outline.contours() {
            self.contour(contour, &place);
        }
        // Each grid pixel's winding clamps before a mask pixel averages
        // its factor-by-factor block.
        mask.alpha.reserve(width * height);
        self.columns.clear();
        self.columns.resize(width, 0.0);
        let block = (factor * factor) as f32;
        for (index, row) in self.area.chunks(self.stride).enumerate() {
            let mut sum = 0.0f32;
            for (column, &delta) in row.iter().take(columns).enumerate() {
                sum += delta;
                if let Some(total) = self.columns.get_mut(column / factor) {
                    *total += sum.abs().min(1.0);
                }
            }
            if (index + 1) % factor == 0 {
                for total in &mut self.columns {
                    mask.alpha.push((*total / block * 255.0 + 0.5) as u8);
                    *total = 0.0;
                }
            }
        }
        Ok(())
    }

    /// TrueType's quadratic contour: consecutive off-curve points imply
    /// the on-curve midpoint between them, and the last point joins the
    /// first.
    fn contour(&mut self, contour: &[Point], place: &impl Fn(&Point) -> (f32, f32)) {
        let (Some((first, tail)), Some((last, head))) =
            (contour.split_first(), contour.split_last())
        else {
            return;
        };
        let (start, rest) = if first.on {
            (place(first), tail)
        } else if last.on {
            (place(last), head)
        } else {
            (midpoint(place(last), place(first)), contour)
        };
        let mut pen = start;
        let mut control: Option<(f32, f32)> = None;
        for point in rest {
            let here = place(point);
            if point.on {
                match control.take() {
                    Some(control) => self.curve(pen, control, here),
                    None => self.line(pen, here),
                }
                pen = here;
            } else {
                if let Some(control) = control {
                    let middle = midpoint(control, here);
                    self.curve(pen, control, middle);
                    pen = middle;
                }
                control = Some(here);
            }
        }
        match control {
            Some(control) => self.curve(pen, control, start),
            None => self.line(pen, start),
        }
    }

    /// Flattened so the chords stray at most a thirty-second of a pixel: a
    /// quadratic's chords after `n` equal steps stray `|p0 - 2c + p1| / 4n²`.
    fn curve(&mut self, from: (f32, f32), control: (f32, f32), to: (f32, f32)) {
        let dx = from.0 - 2.0 * control.0 + to.0;
        let dy = from.1 - 2.0 * control.1 + to.1;
        let difference = (dx * dx + dy * dy).sqrt();
        let steps = ((8.0 * difference).sqrt().ceil() as usize).clamp(1, MAX_CURVE_SEGMENTS);
        let mut pen = from;
        for step in 1..=steps {
            let t = step as f32 / steps as f32;
            let u = 1.0 - t;
            let next = if step == steps {
                to
            } else {
                (
                    u * u * from.0 + 2.0 * u * t * control.0 + t * t * to.0,
                    u * u * from.1 + 2.0 * u * t * control.1 + t * t * to.1,
                )
            };
            self.line(pen, next);
            pen = next;
        }
    }

    /// Adds one edge's signed area to every pixel row it crosses: the
    /// pixels it passes through get the part of their area left of it, and
    /// the pixel after it the remainder, so a row's running sum is the
    /// coverage. Endpoints are inside the mask.
    fn line(&mut self, from: (f32, f32), to: (f32, f32)) {
        if from.1 == to.1 {
            return;
        }
        let (direction, (x0, y0), (x1, y1)) = if from.1 < to.1 {
            (1.0, from, to)
        } else {
            (-1.0, to, from)
        };
        let slope = (x1 - x0) / (y1 - y0);
        let right = self.stride.saturating_sub(2) as f32;
        let mut x = x0;
        let last = (y1.ceil() as usize).min(self.height);
        for row in (y0 as usize)..last {
            let base = row * self.stride;
            let bottom = ((row + 1) as f32).min(y1);
            let dy = bottom - (row as f32).max(y0);
            // From the endpoint, not stepped, and held inside the mask: a
            // drift below zero would floor to the previous pixel.
            let next = if bottom >= y1 {
                x1
            } else {
                (x0 + slope * (bottom - y0)).clamp(0.0, right)
            };
            let d = dy * direction;
            let (low, high) = if x < next { (x, next) } else { (next, x) };
            let low_floor = low.floor();
            let first = low_floor as usize;
            let high_ceil = high.ceil();
            let end = high_ceil as usize;
            if end <= first + 1 {
                // Within one pixel: split by the edge's mean position.
                let inside = 0.5 * (x + next) - low_floor;
                self.add(base + first, d - d * inside);
                self.add(base + first + 1, d * inside);
            } else {
                let step = (high - low).recip();
                let low_fraction = low - low_floor;
                let head = 0.5 * step * (1.0 - low_fraction) * (1.0 - low_fraction);
                let high_fraction = high - high_ceil + 1.0;
                let tail = 0.5 * step * high_fraction * high_fraction;
                self.add(base + first, d * head);
                if end == first + 2 {
                    self.add(base + first + 1, d * (1.0 - head - tail));
                } else {
                    let second = step * (1.5 - low_fraction);
                    self.add(base + first + 1, d * (second - head));
                    for column in first + 2..end - 1 {
                        self.add(base + column, d * step);
                    }
                    let before_tail = second + (end - first - 3) as f32 * step;
                    self.add(base + end - 1, d * (1.0 - before_tail - tail));
                }
                self.add(base + end, d * tail);
            }
            x = next;
        }
    }

    fn add(&mut self, at: usize, value: f32) {
        if let Some(cell) = self.area.get_mut(at) {
            *cell += value;
        }
    }
}

fn midpoint(a: (f32, f32), b: (f32, f32)) -> (f32, f32) {
    (0.5 * (a.0 + b.0), 0.5 * (a.1 + b.1))
}
