//! Bounded time plots over caller-owned observations, with stable semantic hits.
use crate::raster::{
    text_run, Draw, GlyphStyle, Primitive, Rect, Surface, BORDER, CHROME, INK, PAPER, SELECTED,
};
use crate::{CELL_HEIGHT, CELL_WIDTH};
pub const SAMPLES: usize = 1024;
pub const SERIES: usize = 16;
pub const LABEL_BYTES: usize = 128;
pub const DRAW_LIMIT: usize = 155_648;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    Lines,
    Stacked,
}
#[derive(Clone, Copy, Debug)]
pub struct Time<'a> {
    pub at: u64,
    pub label: &'a str,
}
#[derive(Clone, Copy, Debug)]
pub struct Series<'a, I> {
    pub id: I,
    pub label: &'a str,
    pub color: u32,
    pub values: &'a [Option<u64>],
}
/// Raw values and maximum share units; divisor changes displayed text only.
#[derive(Clone, Copy, Debug)]
pub struct Axis<'a> {
    pub maximum: u64,
    pub divisor: u64,
    pub decimal_places: u8,
    pub maximum_label: &'a str,
    pub unit: &'a str,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Limit,
    InvalidData,
    InvalidText,
    InvalidSurface,
    NoRoom,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Limit => "chart input limit exceeded",
            Self::InvalidData => "invalid chart observations",
            Self::InvalidText => "invalid chart label",
            Self::InvalidSurface => "invalid chart surface",
            Self::NoRoom => "surface cannot show the chart",
        })
    }
}
impl std::error::Error for Error {}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// A view with no observations cannot emit a time/series selection.
pub struct Selection<I> {
    pub at: u64,
    pub series: Option<I>,
}
#[derive(Debug)]
pub struct Chart<'a, I> {
    surface: Surface,
    rect: Rect,
    plot: Rect,
    mode: Mode,
    axis: Axis<'a>,
    times: &'a [Time<'a>],
    series: &'a [Series<'a, I>],
    legend_columns: usize,
    legend_y: i64,
    row: u32,
}
fn valid_text(text: &str) -> bool {
    !text.is_empty() && text.len() <= LABEL_BYTES && !text.chars().any(char::is_control)
}
impl<'a, I: Copy + Eq> Chart<'a, I> {
    pub fn new(
        surface: Surface,
        rect: Rect,
        mode: Mode,
        axis: Axis<'a>,
        times: &'a [Time<'a>],
        series: &'a [Series<'a, I>],
    ) -> Result<Self, Error> {
        surface.check().map_err(|_| Error::InvalidSurface)?;
        if rect.intersection(surface.bounds()) != Some(rect) {
            return Err(Error::NoRoom);
        }
        if times.len() > SAMPLES || series.len() > SERIES {
            return Err(Error::Limit);
        }
        if axis.maximum == 0
            || axis.divisor == 0
            || axis.decimal_places > 6
            || series.is_empty()
            || times.windows(2).any(|pair| {
                pair.first()
                    .zip(pair.get(1))
                    .is_some_and(|(a, b)| a.at >= b.at)
            })
        {
            return Err(Error::InvalidData);
        }
        if !valid_text(axis.unit)
            || !valid_text(axis.maximum_label)
            || times.iter().any(|time| !valid_text(time.label))
            || series.iter().any(|s| !valid_text(s.label))
        {
            return Err(Error::InvalidText);
        }
        for (index, entry) in series.iter().enumerate() {
            if entry.values.len() != times.len()
                || series.iter().take(index).any(|old| old.id == entry.id)
                || entry
                    .values
                    .iter()
                    .flatten()
                    .any(|value| *value > axis.maximum)
            {
                return Err(Error::InvalidData);
            }
        }
        if mode == Mode::Stacked {
            for index in 0..times.len() {
                let mut total = 0u64;
                for entry in series {
                    if let Some(Some(value)) = entry.values.get(index) {
                        total = total.checked_add(*value).ok_or(Error::InvalidData)?;
                    }
                }
                if total > axis.maximum {
                    return Err(Error::InvalidData);
                }
            }
        }
        let scale = surface.scale.value();
        let cell = CELL_WIDTH * scale;
        let row = (CELL_HEIGHT + 8) * scale;
        let legend_width = (series
            .iter()
            .map(|s| s.label.chars().count())
            .max()
            .unwrap_or(0)
            + 3)
            * cell;
        let legend_columns = (rect.width as usize / legend_width).min(series.len());
        if legend_columns == 0 {
            return Err(Error::NoRoom);
        }
        let legend_rows = series.len().div_ceil(legend_columns);
        let left = (axis.maximum_label.chars().count().max(1) + 1) * cell;
        let reserved = (legend_rows + 3) * row;
        let plot_width = (rect.width as usize)
            .checked_sub(left + cell)
            .ok_or(Error::NoRoom)?;
        let plot_height = (rect.height as usize)
            .checked_sub(reserved)
            .ok_or(Error::NoRoom)?;
        let time_width = times
            .iter()
            .map(|t| t.label.chars().count())
            .max()
            .unwrap_or(0)
            * cell;
        if plot_width < 32 * scale
            || plot_height < 2 * row
            || (if times.len() == 1 {
                time_width + 2 * cell
            } else {
                2 * (time_width + cell)
            }) > plot_width
            || (axis.unit.chars().count()
                + 22
                + usize::from(axis.decimal_places)
                + usize::from(axis.decimal_places > 0))
                * cell
                > rect.width as usize
        {
            return Err(Error::NoRoom);
        }
        let plot = Rect {
            x: rect.x + left as i64,
            y: rect.y + row as i64,
            width: plot_width as u32,
            height: plot_height as u32,
        };
        let legend_y = plot.y + i64::from(plot.height) + row as i64;
        Ok(Self {
            surface,
            rect,
            plot,
            mode,
            axis,
            times,
            series,
            legend_columns,
            legend_y,
            row: row as u32,
        })
    }
    pub fn plot(&self) -> Rect {
        self.plot
    }
    pub fn rect(&self) -> Rect {
        self.rect
    }
    pub fn legend(&self, index: usize) -> Option<Rect> {
        self.series.get(index)?;
        let width = self.rect.width / self.legend_columns as u32;
        Some(Rect {
            x: self.rect.x + (index % self.legend_columns) as i64 * i64::from(width),
            y: self.legend_y + (index / self.legend_columns) as i64 * i64::from(self.row),
            width,
            height: self.row,
        })
    }
    fn x(&self, at: u64) -> Option<i64> {
        let first = self.times.first()?.at;
        let last = self.times.last()?.at;
        if at < first || at > last {
            return None;
        }
        if first == last {
            return Some(self.plot.x + i64::from(self.plot.width / 2));
        }
        Some(
            self.plot.x
                + (u128::from(at - first) * u128::from(self.plot.width - 1)
                    / u128::from(last - first)) as i64,
        )
    }
    fn y(&self, value: u64) -> i64 {
        self.plot.y + i64::from(self.plot.height - 1)
            - (u128::from(value) * u128::from(self.plot.height - 1) / u128::from(self.axis.maximum))
                as i64
    }
    fn nearest(&self, x: i64) -> Option<usize> {
        let first = self.times.first()?.at;
        let last = self.times.last()?.at;
        let offset = x
            .saturating_sub(self.plot.x)
            .clamp(0, i64::from(self.plot.width - 1)) as u64;
        // Compare in the original time domain, retaining subpixel timestamps.
        let target = u128::from(first) * u128::from(self.plot.width - 1)
            + u128::from(last - first) * u128::from(offset);
        self.times
            .iter()
            .enumerate()
            .min_by_key(|(_, time)| {
                (u128::from(time.at) * u128::from(self.plot.width - 1)).abs_diff(target)
            })
            .map(|(index, _)| index)
    }
    fn bracket(&self, x: i64) -> Option<(usize, usize, u128, u128)> {
        if x < self.plot.x || x >= self.plot.x + i64::from(self.plot.width) {
            return None;
        }
        let first = self.times.first()?.at;
        let last = self.times.last()?.at;
        if first == last {
            return (Some(x) == self.x(first)).then_some((0, 0, 0, 1));
        }
        let width = u128::from(self.plot.width - 1);
        let target = u128::from(first) * width
            + u128::from(last - first) * x.saturating_sub(self.plot.x) as u128;
        let right = self
            .times
            .partition_point(|time| u128::from(time.at) * width < target);
        let right_time = self.times.get(right)?;
        if u128::from(right_time.at) * width == target {
            return Some((right, right, 0, 1));
        }
        let left = right.checked_sub(1)?;
        let left_time = self.times.get(left)?;
        let delta = target - u128::from(left_time.at) * width;
        let span = u128::from(right_time.at - left_time.at) * width;
        Some((left, right, delta, span))
    }

    fn value(&self, series: usize, bracket: (usize, usize, u128, u128)) -> Option<u64> {
        let values = self.series.get(series)?.values;
        let (left, right, fraction, span) = bracket;
        Some(interpolate(
            (*values.get(left)?)?,
            (*values.get(right)?)?,
            fraction,
            span,
        ))
    }
    fn columns(&self, x: i64) -> [Option<(i64, i64)>; SERIES] {
        let mut columns = [None; SERIES];
        let Some(bracket) = self.bracket(x) else {
            return columns;
        };
        let previous = (x > self.plot.x).then(|| self.bracket(x - 1)).flatten();
        if self.mode == Mode::Lines {
            for (index, entry) in self.series.iter().enumerate() {
                let Some(value) = self.value(index, bracket) else {
                    continue;
                };
                let y = self.y(value);
                let prior = previous
                    .filter(|before| {
                        entry
                            .values
                            .get(before.0.min(bracket.0)..=before.1.max(bracket.1))
                            .is_some_and(|values| values.iter().all(Option::is_some))
                    })
                    .and_then(|before| self.value(index, before))
                    .map_or(y, |value| self.y(value));
                if let Some(slot) = columns.get_mut(index) {
                    *slot = Some((
                        y.min(prior),
                        (y.max(prior) + self.surface.scale.value() as i64)
                            .min(self.plot.y + i64::from(self.plot.height)),
                    ));
                }
            }
            return columns;
        }
        let (left, right, fraction, span) = bracket;
        let mut a = 0u64;
        let mut b = 0u64;
        let mut bottom = self.y(0);
        for (index, entry) in self.series.iter().enumerate() {
            let Some((Some(first), Some(last))) =
                entry.values.get(left).zip(entry.values.get(right))
            else {
                return [None; SERIES];
            };
            let Some(next_a) = a.checked_add(*first) else {
                return [None; SERIES];
            };
            let Some(next_b) = b.checked_add(*last) else {
                return [None; SERIES];
            };
            a = next_a;
            b = next_b;
            let top = self.y(interpolate(a, b, fraction, span));
            if let Some(slot) = columns.get_mut(index) {
                *slot = Some((top, bottom));
            }
            bottom = top;
        }
        columns
    }
    fn sample_markers(&self, sample: usize) -> [Option<Rect>; SERIES] {
        let mut markers = [None; SERIES];
        let Some(x) = self.times.get(sample).and_then(|time| self.x(time.at)) else {
            return markers;
        };
        let scale = self.surface.scale.value() as u32;
        let mut lower = 0u64;
        for (index, entry) in self.series.iter().enumerate() {
            let value = entry.values.get(sample).copied().flatten();
            let marker = match self.mode {
                Mode::Lines => value.and_then(|value| {
                    (Rect {
                        x: x - i64::from(scale),
                        y: self.y(value) - i64::from(scale),
                        width: 3 * scale,
                        height: 3 * scale,
                    })
                    .intersection(self.plot)
                }),
                Mode::Stacked => {
                    let Some(upper) = value.and_then(|value| lower.checked_add(value)) else {
                        return [None; SERIES];
                    };
                    let top = self.y(upper);
                    let height = self.y(lower).saturating_sub(top) as u32;
                    lower = upper;
                    (height > 0).then_some(Rect {
                        x,
                        y: top,
                        width: 1,
                        height,
                    })
                }
            };
            if let Some(slot) = markers.get_mut(index) {
                *slot = marker;
            }
        }
        markers
    }
    pub fn hit(&self, x: i64, y: i64, selected: Option<Selection<I>>) -> Option<Selection<I>> {
        if !self.rect.contains(x, y) {
            return None;
        }
        if self.plot.contains(x, y) {
            // Markers paint after interpolated columns; later observations
            // and later series paint last when several share a pixel.
            for (sample, time) in self.times.iter().enumerate().rev() {
                let Some(at) = self.x(time.at) else {
                    continue;
                };
                let scale = self.surface.scale.value() as i64;
                let near = match self.mode {
                    Mode::Lines => x >= at - scale && x < at + 2 * scale,
                    Mode::Stacked => x == at,
                };
                if !near {
                    continue;
                }
                let markers = self.sample_markers(sample);
                for (index, entry) in self.series.iter().enumerate().rev() {
                    if markers
                        .get(index)
                        .copied()
                        .flatten()
                        .is_some_and(|rect| rect.contains(x, y))
                    {
                        return Some(Selection {
                            at: time.at,
                            series: Some(entry.id),
                        });
                    }
                }
            }
            let time = self.times.get(self.nearest(x)?)?;
            let columns = self.columns(x);
            let mut picked = None;
            let mut distance = u64::MAX;
            for (index, series) in self.series.iter().enumerate() {
                if let Some((top, bottom)) = columns.get(index).copied().flatten() {
                    let delta = if y < top {
                        top.abs_diff(y)
                    } else if y >= bottom {
                        (bottom - 1).abs_diff(y)
                    } else {
                        0
                    };
                    let inside = match self.mode {
                        Mode::Lines => {
                            delta <= (3 * self.surface.scale.value()) as u64 && delta <= distance
                        }
                        Mode::Stacked => top < bottom && y >= top && y < bottom,
                    };
                    if inside {
                        picked = Some(series.id);
                        distance = delta;
                    }
                }
            }
            return Some(Selection {
                at: time.at,
                series: picked,
            });
        }
        for (index, series) in self.series.iter().enumerate() {
            if self.legend(index).is_some_and(|rect| rect.contains(x, y)) {
                let at = selected
                    .map(|s| s.at)
                    .filter(|at| self.times.iter().any(|t| t.at == *at))
                    .or_else(|| self.times.last().map(|t| t.at))?;
                return Some(Selection {
                    at,
                    series: Some(series.id),
                });
            }
        }
        None
    }
}

// The slow path is only needed for near-u64 observations and time spans.
// Its remainder is below 3 * divisor, at most 3 * u64::MAX * MAX_AXIS.
fn multiply_divide(value: u64, fraction: u128, divisor: u128) -> (u128, u128) {
    if let Some(product) = u128::from(value).checked_mul(fraction) {
        return (product / divisor, product % divisor);
    }
    let mut quotient = 0u128;
    let mut remainder = 0u128;
    for bit in (0..64).rev() {
        remainder = 2 * remainder
            + if value & (1u64 << bit) != 0 {
                fraction
            } else {
                0
            };
        quotient = 2 * quotient + remainder / divisor;
        remainder %= divisor;
    }
    (quotient, remainder)
}
fn interpolate(a: u64, b: u64, fraction: u128, span: u128) -> u64 {
    let (whole, remainder) = multiply_divide(a.abs_diff(b), fraction, span);
    if b >= a {
        a + whole as u64
    } else {
        a - whole as u64 - u64::from(remainder != 0)
    }
}

fn fill(rect: Rect, color: u32, damage: Rect, sink: &mut dyn FnMut(Draw)) {
    if let Some(clip) = rect.intersection(damage) {
        sink(Draw {
            clip,
            primitive: Primitive::Fill { rect, color },
        });
    }
}
impl<I: Copy + Eq> Chart<'_, I> {
    fn text(
        &self,
        text: impl Iterator<Item = char>,
        rect: Rect,
        selected: bool,
        damage: Rect,
        sink: &mut dyn FnMut(Draw),
    ) {
        let background = if selected { SELECTED } else { CHROME };
        fill(rect, background, damage, sink);
        text_run(
            self.surface.scale,
            text,
            (
                rect.x + (CELL_WIDTH * self.surface.scale.value()) as i64,
                rect.y + (4 * self.surface.scale.value()) as i64,
            ),
            rect,
            GlyphStyle::medium(if selected { PAPER } else { INK }, background),
            damage,
            sink,
        );
    }
    pub fn emit(
        &self,
        selection: Option<Selection<I>>,
        focused: bool,
        damage: Rect,
        sink: &mut dyn FnMut(Draw),
    ) {
        let Some(damage) = damage.intersection(self.rect) else {
            return;
        };
        fill(self.rect, CHROME, damage, sink);
        fill(self.plot, PAPER, damage, sink);
        let cell = (CELL_WIDTH * self.surface.scale.value()) as u32;
        let thickness = self.surface.scale.value() as u32;
        for y in [self.plot.y, self.plot.y + i64::from(self.plot.height - 1)] {
            fill(
                Rect {
                    x: self.plot.x - 1,
                    y,
                    width: self.plot.width + 1,
                    height: 1,
                },
                BORDER,
                damage,
                sink,
            );
        }
        fill(
            Rect {
                x: self.plot.x - 1,
                width: 1,
                ..self.plot
            },
            BORDER,
            damage,
            sink,
        );
        self.text(
            self.axis.maximum_label.chars(),
            Rect {
                x: self.rect.x,
                y: self.plot.y,
                width: (self.plot.x - self.rect.x) as u32,
                height: self.row,
            },
            false,
            damage,
            sink,
        );
        self.text(
            "0".chars(),
            Rect {
                x: self.rect.x,
                y: self.plot.y + i64::from(self.plot.height) - i64::from(self.row),
                width: (self.plot.x - self.rect.x) as u32,
                height: self.row,
            },
            false,
            damage,
            sink,
        );
        let selected_index =
            selection.and_then(|s| self.times.iter().position(|time| time.at == s.at));
        let selected_series = selection
            .and_then(|s| s.series)
            .and_then(|id| self.series.iter().position(|entry| entry.id == id));
        let mut digits = [0u8; 32];
        let numeric = selected_index
            .zip(selected_series)
            .and_then(|(time, series)| {
                self.series.get(series)?.values.get(time).copied().flatten()
            });
        let value = match numeric {
            Some(number) => {
                let places = self.axis.decimal_places;
                let factor = 10u128.pow(u32::from(places));
                let mut whole = number / self.axis.divisor;
                let mut fraction = (u128::from(number % self.axis.divisor) * factor
                    + u128::from(self.axis.divisor / 2))
                    / u128::from(self.axis.divisor);
                if fraction == factor {
                    // Carry requires divisor > 1, so the integer part has room.
                    whole += 1;
                    fraction = 0;
                }
                let mut start = digits.len();
                if places > 0 {
                    for _ in 0..places {
                        start = start.saturating_sub(1);
                        if let Some(slot) = digits.get_mut(start) {
                            *slot = b'0' + (fraction % 10) as u8;
                        }
                        fraction /= 10;
                    }
                    start = start.saturating_sub(1);
                    if let Some(slot) = digits.get_mut(start) {
                        *slot = b'.';
                    }
                }
                loop {
                    start = start.saturating_sub(1);
                    if let Some(slot) = digits.get_mut(start) {
                        *slot = b'0' + (whole % 10) as u8;
                    }
                    whole /= 10;
                    if whole == 0 {
                        break;
                    }
                }
                digits
                    .get(start..)
                    .and_then(|digits| std::str::from_utf8(digits).ok())
                    .unwrap_or("")
            }
            None => "Unavailable",
        };
        self.text(
            value
                .chars()
                .chain(" ".chars())
                .chain(self.axis.unit.chars()),
            Rect {
                height: self.row,
                ..self.rect
            },
            focused,
            damage,
            sink,
        );
        for offset in 0..self.plot.width {
            let x = self.plot.x + i64::from(offset);
            let columns = self.columns(x);
            for (index, entry) in self.series.iter().enumerate() {
                let Some((y, bottom)) = columns.get(index).copied().flatten() else {
                    continue;
                };
                let height = bottom.saturating_sub(y) as u32;
                if height == 0 {
                    continue;
                }
                let column = Rect {
                    x,
                    y,
                    width: 1,
                    height,
                };
                if let Some(column) = column.intersection(self.plot) {
                    fill(column, entry.color, damage, sink);
                }
            }
        }
        for sample in 0..self.times.len() {
            let markers = self.sample_markers(sample);
            for (index, entry) in self.series.iter().enumerate() {
                if let Some(marker) = markers.get(index).copied().flatten() {
                    fill(marker, entry.color, damage, sink);
                }
            }
        }
        if let Some(x) = selected_index
            .and_then(|index| self.times.get(index))
            .and_then(|time| self.x(time.at))
        {
            for (x, width, color) in [
                (x - i64::from(thickness), 3 * thickness, PAPER),
                (x, thickness, INK),
            ] {
                if let Some(marker) = (Rect {
                    x,
                    width,
                    ..self.plot
                })
                .intersection(self.plot)
                {
                    fill(marker, color, damage, sink);
                }
            }
        }
        let time_y = self.plot.y + i64::from(self.plot.height);
        if self.times.len() == 1 {
            if let Some(time) = self.times.first() {
                let width = (time.label.chars().count() as u32 + 2) * cell;
                self.text(
                    time.label.chars(),
                    Rect {
                        x: self.plot.x + i64::from((self.plot.width - width) / 2),
                        y: time_y,
                        width,
                        height: self.row,
                    },
                    false,
                    damage,
                    sink,
                );
            }
        } else {
            if let Some(first) = self.times.first() {
                self.text(
                    first.label.chars(),
                    Rect {
                        x: self.plot.x - i64::from(cell),
                        y: time_y,
                        width: self.plot.width / 2,
                        height: self.row,
                    },
                    false,
                    damage,
                    sink,
                );
            }
            if let Some(last) = self.times.last() {
                let width = (last.label.chars().count() as u32 + 1) * cell;
                self.text(
                    last.label.chars(),
                    Rect {
                        x: self.plot.x + i64::from(self.plot.width - width),
                        y: time_y,
                        width,
                        height: self.row,
                    },
                    false,
                    damage,
                    sink,
                );
            }
        }
        for (index, entry) in self.series.iter().enumerate() {
            if let Some(rect) = self.legend(index) {
                self.text(
                    entry.label.chars(),
                    rect,
                    selected_series == Some(index),
                    damage,
                    sink,
                );
                fill(
                    Rect {
                        x: rect.x,
                        y: rect.y + 4 * i64::from(thickness),
                        width: thickness * 4,
                        height: self.row - thickness * 8,
                    },
                    entry.color,
                    damage,
                    sink,
                );
            }
        }
        if let Some(time) = selected_index.and_then(|index| self.times.get(index)) {
            self.text(
                time.label.chars(),
                Rect {
                    y: self.rect.y + i64::from(self.rect.height - self.row),
                    height: self.row,
                    ..self.rect
                },
                false,
                damage,
                sink,
            );
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Key {
    PreviousTime,
    NextTime,
    FirstTime,
    LastTime,
    PreviousSeries,
    NextSeries,
    ClearSeries,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Event {
    Key { key: Key, repeated: bool },
    Press { x: i64, y: i64 },
    Move { x: i64, y: i64 },
    Release { x: i64, y: i64 },
    FocusLost,
    Resize,
    Other,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome<I> {
    Ignored,
    Consumed,
    Stale,
    Selected(Selection<I>),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LayoutKey {
    surface: Surface,
    rect: Rect,
    plot: Rect,
    mode: Mode,
    columns: usize,
}
impl<I> Chart<'_, I> {
    fn layout_key(&self) -> LayoutKey {
        LayoutKey {
            surface: self.surface,
            rect: self.rect,
            plot: self.plot,
            mode: self.mode,
            columns: self.legend_columns,
        }
    }
}
#[derive(Clone, Copy, Debug)]
struct Gesture<I, R> {
    revision: R,
    selection: Selection<I>,
    layout: LayoutKey,
}
#[derive(Debug)]
pub struct State<I, R> {
    selection: Option<Selection<I>>,
    armed: Option<Gesture<I, R>>,
}
impl<I: Copy + Eq, R: Copy + Eq> Default for State<I, R> {
    fn default() -> Self {
        Self {
            selection: None,
            armed: None,
        }
    }
}
impl<I: Copy + Eq, R: Copy + Eq> State<I, R> {
    pub fn selection(&self) -> Option<Selection<I>> {
        self.selection
    }
    /// Retire input before a relayout that may refuse to construct a view.
    pub fn cancel_gesture(&mut self) {
        self.armed = None;
    }
    pub fn select(&mut self, selection: Option<Selection<I>>) {
        self.selection = selection;
        self.armed = None;
    }
    pub fn event(&mut self, chart: &Chart<'_, I>, revision: R, event: Event) -> Outcome<I> {
        if self.armed.is_some_and(|gesture| {
            gesture.revision != revision || gesture.layout != chart.layout_key()
        }) {
            self.armed = None;
            if matches!(event, Event::Move { .. } | Event::Release { .. }) {
                return Outcome::Stale;
            }
        }
        match event {
            Event::FocusLost | Event::Resize | Event::Other => {
                if self.armed.take().is_some() {
                    Outcome::Consumed
                } else {
                    Outcome::Ignored
                }
            }
            Event::Key {
                key,
                repeated: true,
            } if !matches!(key, Key::PreviousTime | Key::NextTime) => {
                self.armed = None;
                Outcome::Consumed
            }
            Event::Key { key, .. } => {
                self.armed = None;
                let Some(last) = chart.times.len().checked_sub(1) else {
                    return Outcome::Consumed;
                };
                let mut time = self
                    .selection
                    .and_then(|s| chart.times.iter().position(|t| t.at == s.at))
                    .unwrap_or(last);
                let mut series = self
                    .selection
                    .and_then(|s| s.series)
                    .and_then(|id| chart.series.iter().position(|s| s.id == id));
                match key {
                    Key::PreviousTime => time = time.saturating_sub(1),
                    Key::NextTime => time = (time + 1).min(last),
                    Key::FirstTime => time = 0,
                    Key::LastTime => time = last,
                    Key::ClearSeries => series = None,
                    Key::PreviousSeries => {
                        series = Some(
                            series
                                .filter(|index| *index > 0)
                                .map_or(chart.series.len() - 1, |index| index - 1),
                        )
                    }
                    Key::NextSeries => {
                        series = Some(series.map_or(0, |index| (index + 1) % chart.series.len()))
                    }
                }
                let Some(time) = chart.times.get(time) else {
                    return Outcome::Consumed;
                };
                let selection = Selection {
                    at: time.at,
                    series: series
                        .and_then(|index| chart.series.get(index))
                        .map(|s| s.id),
                };
                self.selection = Some(selection);
                Outcome::Selected(selection)
            }
            Event::Press { x, y } => {
                self.armed = chart.hit(x, y, self.selection).map(|selection| Gesture {
                    revision,
                    selection,
                    layout: chart.layout_key(),
                });
                if self.armed.is_some() {
                    Outcome::Consumed
                } else {
                    Outcome::Ignored
                }
            }
            Event::Move { x, y } => {
                if self.armed.is_none() {
                    return Outcome::Ignored;
                }
                if self.armed.is_some_and(|gesture| {
                    chart.hit(x, y, self.selection) != Some(gesture.selection)
                }) {
                    self.armed = None;
                }
                Outcome::Consumed
            }
            Event::Release { x, y } => {
                let Some(Gesture { selection, .. }) = self.armed.take() else {
                    return Outcome::Ignored;
                };
                if chart.hit(x, y, self.selection) != Some(selection) {
                    return Outcome::Consumed;
                }
                self.selection = Some(selection);
                Outcome::Selected(selection)
            }
        }
    }
}
