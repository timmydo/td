//! CSS Grid 2 layout: grid items (§6), placement (§8, sparse
//! auto-placement in rows or columns), the explicit and implicit grids
//! (§7, auto-fill and auto-fit repeats), track sizing (§12: columns
//! first, then rows from the items laid at their columns' widths) and
//! self- and content alignment (Box Alignment 3). See DESIGN.md §5,
//! "Grid".
use super::flex::{
    holding, index, justify, percent_height, FlexItem, Source, HEIGHT, MAX_RELAY, PERCENT,
};
use super::table::{down, px, units};
use super::*;
use crate::{grid_container, MAX_COORDINATE, MAX_GRID_CELLS};
use td_css::values::{Breadth, GridAreas, Repeat, TrackItem, TrackList, TrackSize, MAX_GRID_LINE};

/// A flex factor's fixed-point unit, as flex's: multiples of 2^-20.
const ONE: i64 = 1 << 20;
/// No limit, in 1/64 px: an infinite growth limit.
const NONE: i64 = 64 * MAX_COORDINATE as i64;
/// The placements a grid keeps of the styles its items last had, each
/// replaced in turn: the length is the entry count.
const PLACEMENT_CACHE: usize = 8;
#[cfg(test)]
thread_local! {
    /// Tests lay with no contributions kept, to pin that keeping them
    /// changes nothing laid.
    pub(crate) static MEMO_OFF: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
#[cfg(test)]
fn memo_off() -> bool {
    MEMO_OFF.with(std::cell::Cell::get)
}
#[cfg(not(test))]
fn memo_off() -> bool {
    false
}
/// A grid `grid_known` has seen measured not at all, or once.
const UNMEASURED: u32 = u32::MAX;
const MEASURED_ONCE: u32 = u32::MAX - 1;
/// No geometry: an anonymous item's.
const NO_GEOMETRY: u32 = u32::MAX;

/// What a track sizing function is (§7.2).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Fixed,
    Auto,
    MinContent,
    MaxContent,
    /// `fr`, in 1/ONE.
    Flex,
    /// `fit-content()` of a length, as a maximum.
    Fit,
}

/// A track sizing function and its value: a fixed size or a
/// `fit-content()` argument in 1/64 px, a flex factor in 1/ONE.
#[derive(Clone, Copy)]
struct Sizing {
    kind: Kind,
    value: i64,
}

impl Sizing {
    const AUTO: Self = Self {
        kind: Kind::Auto,
        value: 0,
    };
    fn intrinsic(self) -> bool {
        matches!(
            self.kind,
            Kind::Auto | Kind::MinContent | Kind::MaxContent | Kind::Fit
        )
    }
}

/// A grid track: its sizing functions, its base size and growth limit
/// (NONE for infinite), the scratch §12.5.1 distributes with, and its
/// offset from the content edge once sized.
#[derive(Clone, Copy)]
pub(crate) struct Track {
    min: Sizing,
    max: Sizing,
    base: i64,
    limit: i64,
    planned: i64,
    incurred: i64,
    start: i64,
    /// A lower bound of its final size while its rows are laid.
    floor: i64,
    growable: bool,
    frozen: bool,
    touched: bool,
    collapsed: bool,
}

impl Track {
    fn new(min: Sizing, max: Sizing) -> Self {
        Self {
            min,
            max,
            base: 0,
            limit: NONE,
            planned: 0,
            incurred: 0,
            start: 0,
            floor: 0,
            growable: false,
            frozen: false,
            touched: false,
            collapsed: false,
        }
    }
    fn flexible(&self) -> bool {
        self.max.kind == Kind::Flex
    }
    /// Its base size's cap while space is distributed (§12.5.1).
    fn cap(&self) -> i64 {
        match self.max.kind {
            Kind::Fit => self.limit.min(self.max.value.max(self.base)),
            _ => self.limit,
        }
    }
    /// Its size before the rows above are final: what it holds of what
    /// was laid in it, or its fixed size (§12.4, §12.5 step 2).
    fn initial_floor(&self, free: bool) -> i64 {
        let base = if self.min.kind == Kind::Fixed {
            self.min.value
        } else {
            0
        };
        match self.max.kind {
            Kind::Fixed if free => base.max(self.max.value),
            _ => base,
        }
    }
}

/// An item's measured contributions across its columns, kept while the
/// layout lasts.
#[derive(Clone, Copy)]
pub(crate) struct Measured {
    least: i64,
    most: i64,
    floor: i64,
    bare: i64,
    measured: bool,
}

/// How an item is aligned in its area along one axis.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Align {
    Start,
    Center,
    End,
    Stretch,
}

/// A grid item: its source, placement, measured contributions in the
/// axis being sized, and once laid what it laid.
#[derive(Clone, Copy)]
pub(crate) struct GridItem {
    source: Source,
    /// Zero-based lines from the implicit grid's first once placed, end
    /// exclusive; before, an auto axis holds its span from 0.
    rows: (i32, i32),
    cols: (i32, i32),
    /// Whether its rows and columns are definite before auto-placement.
    fixed: (bool, bool),
    /// Its min-content, max-content and minimum contributions (§6.6),
    /// the last where an automatic minimum applies (`floor`) and where
    /// none does (`bare`), in the axis being sized.
    least: i64,
    most: i64,
    floor: i64,
    bare: i64,
    measured: bool,
    justify: Align,
    align: Align,
    /// Auto margins: top, right, bottom, left.
    autos: [bool; 4],
    /// Stretched in the block axis: a block of auto height.
    stretch: bool,
    /// Its own height or its bounds a percentage of its area's.
    percent: bool,
    /// Its margin box's top as laid, from the content edge, and whether
    /// it was laid at its area's height.
    top: i64,
    at_height: bool,
    outer: f64,
    min_height: f64,
    max_height: f64,
    lines: (u32, u32),
    boxes: (u32, u32),
    markers: (u32, u32),
    geometry: u32,
    bounds: Option<Rect>,
    wants: u8,
    /// The least row start of the items after it in tree order.
    later: i32,
}

impl GridItem {
    fn new(source: Source) -> Self {
        Self {
            source,
            rows: (0, 1),
            cols: (0, 1),
            fixed: (false, false),
            least: 0,
            most: 0,
            floor: 0,
            bare: 0,
            measured: false,
            justify: Align::Stretch,
            align: Align::Stretch,
            autos: [false; 4],
            stretch: false,
            percent: false,
            top: 0,
            at_height: false,
            outer: 0.0,
            min_height: 0.0,
            max_height: f64::INFINITY,
            lines: (0, 0),
            boxes: (0, 0),
            markers: (0, 0),
            geometry: NO_GEOMETRY,
            bounds: None,
            wants: 0,
            later: i32::MAX,
        }
    }
    fn span(&self, rows: bool) -> (usize, usize) {
        let (start, end) = if rows { self.rows } else { self.cols };
        (start.max(0) as usize, end.max(0) as usize)
    }
    fn geometry(&self) -> Option<usize> {
        (self.geometry != NO_GEOMETRY).then_some(self.geometry as usize)
    }
    /// Its minimum contribution, its automatic minimum where it applies.
    fn minimum(&self, automatic: bool) -> i64 {
        if automatic {
            self.floor
        } else {
            self.bare
        }
    }
}

/// How the free space of an axis is known while its tracks are sized.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Space {
    Definite(i64),
    /// An auto height: rows at their growth limits, `fr` from contents,
    /// each redone against a least and a greatest height it passes
    /// (§12.6, §12.7.1).
    Indefinite(Option<i64>, Option<i64>),
    /// Under a min-content or max-content constraint (§12.3).
    Min,
    Max,
}

/// How many times an automatic repeat repeats (§7.2.3.2): as many as fit
/// a definite or greatest size, as few as fill a least one, else once.
#[derive(Clone, Copy)]
enum Fill {
    Once,
    Most(i64),
    Least(i64),
}

/// One axis's explicit grid: its template, its areas, and its repeat.
#[derive(Clone, Copy)]
struct Axis<'a> {
    list: Option<&'a TrackList>,
    areas: Option<&'a GridAreas>,
    rows: bool,
    /// Explicit tracks: the template's, repeats expanded, or the areas',
    /// the more.
    count: i32,
    repeats: u32,
}

impl<'a> Axis<'a> {
    fn of(
        list: Option<&'a TrackList>,
        areas: Option<&'a GridAreas>,
        rows: bool,
        (fill, gap, basis): (Fill, i64, Option<f64>),
    ) -> Self {
        let repeats = list.map_or(1, |list| repeat_count(list, fill, gap, basis));
        let template = list.map_or(0, |list| track_count(list, repeats).0);
        let spanned = areas.map_or(0, |a| if rows { a.rows() } else { a.columns() }) as i64;
        Self {
            list,
            areas,
            rows,
            count: template.max(spanned).min(i64::from(MAX_GRID_LINE)) as i32,
            repeats,
        }
    }
}

/// Lines a name holds in a repeat: `start` plus each of its residues
/// (offsets under `period`, ascending) in each of `times` repetitions.
#[derive(Clone, Copy)]
struct Block {
    start: i32,
    period: i32,
    times: i32,
    residues: (u32, u32),
}

/// Where a name of a track list stands: at a line, or in a repeat's body
/// at `offset` tracks from each repetition's start.
#[derive(Clone, Copy)]
enum Spot {
    Line(i32),
    Repeat {
        start: i32,
        period: i32,
        times: i32,
        offset: i32,
    },
}

/// A name, the range of its blocks and the range of its other lines.
type Entry<'a> = (&'a str, (u32, u32), (u32, u32));

/// An axis's line names, indexed once a grid, as an item first names
/// one, from the track list as written, not as its repeats expand: each
/// distinct name, sorted, with the blocks of lines it holds in repeats
/// and its lines outside them, so a lookup is a binary search of the
/// names and arithmetic over its blocks (§8.3).
pub(super) struct Names<'a> {
    entries: BudgetVec<Entry<'a>>,
    blocks: BudgetVec<Block>,
    residues: BudgetVec<i32>,
    points: BudgetVec<i32>,
    /// Scratch: each name as written, its list item and where it stands.
    found: BudgetVec<(&'a str, u32, Spot)>,
    built: bool,
}

impl Names<'_> {
    /// Empties the index for another grid, keeping its storage.
    fn reset(&mut self) {
        self.entries.truncate(0);
        self.blocks.truncate(0);
        self.residues.truncate(0);
        self.points.truncate(0);
        self.found.truncate(0);
        self.built = false;
    }
    pub(super) fn new(memory: &Budget) -> Result<Self, Error> {
        Ok(Self {
            entries: BudgetVec::new(memory)?,
            blocks: BudgetVec::new(memory)?,
            residues: BudgetVec::new(memory)?,
            points: BudgetVec::new(memory)?,
            found: BudgetVec::new(memory)?,
            built: false,
        })
    }
}

/// The lines below `v` that a name's `blocks` and `points` hold.
fn named_below(blocks: &[Block], residues: &[i32], points: &[i32], v: i32) -> usize {
    let mut count = points.partition_point(|p| *p < v);
    for block in blocks {
        let held = residues
            .get(block.residues.0 as usize..block.residues.1 as usize)
            .unwrap_or(&[]);
        let x = i64::from(v) - i64::from(block.start);
        if x <= 0 {
            continue;
        }
        let whole = i64::from(block.times) * i64::from(block.period);
        let per = held.len();
        count += if x >= whole {
            block.times as usize * per
        } else {
            let (r, rem) = (x / i64::from(block.period), x % i64::from(block.period));
            r as usize * per + held.partition_point(|o| i64::from(*o) < rem)
        };
    }
    count
}

/// Whether the name's `blocks` or `points` hold line `v`.
fn named_holds(blocks: &[Block], residues: &[i32], points: &[i32], v: i32) -> bool {
    points.binary_search(&v).is_ok()
        || blocks.iter().any(|block| {
            let held = residues
                .get(block.residues.0 as usize..block.residues.1 as usize)
                .unwrap_or(&[]);
            let x = i64::from(v) - i64::from(block.start);
            let whole = i64::from(block.times) * i64::from(block.period);
            x >= 0
                && x < whole
                && held
                    .binary_search(&((x % i64::from(block.period)) as i32))
                    .is_ok()
        })
}

/// `n` against `base` followed by `suffix`, as byte slices.
pub(super) fn name_order(n: &str, (base, suffix): (&str, &str)) -> std::cmp::Ordering {
    let (n, base, suffix) = (n.as_bytes(), base.as_bytes(), suffix.as_bytes());
    let at = n.len().min(base.len());
    let (head, rest) = n.split_at_checked(at).unwrap_or((n, &[]));
    let stem = base.get(..at).unwrap_or(base);
    head.cmp(stem).then_with(|| {
        if base.len() > at {
            std::cmp::Ordering::Less
        } else {
            rest.cmp(suffix)
        }
    })
}

/// A placement longhand: auto, a line (a lone name has integer 0) or a
/// span, with its name.
#[derive(Clone, Copy)]
enum Edge<'a> {
    Auto,
    Line(i32, Option<&'a str>),
    Span(i32, Option<&'a str>),
}

fn edge(style: &ComputedStyle, p: Property) -> Edge<'_> {
    match style.get(p) {
        Some(ComputedValue::GridLine(line)) => {
            let name = line.name.as_deref();
            if line.span {
                Edge::Span(line.integer.max(1), name)
            } else {
                Edge::Line(line.integer, name)
            }
        }
        _ => Edge::Auto,
    }
}

/// One axis's placement: definite lines, or a span to auto-place.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Placed {
    Lines(i32, i32),
    Auto(i32),
}

/// Walks `list`'s sizes, its automatic repeat `repeats` times, each with
/// its track, until `each` says to stop; returns the items it visited.
fn walk<'a>(
    list: &'a TrackList,
    repeats: u32,
    each: &mut dyn FnMut(u32, &'a TrackSize) -> bool,
) -> usize {
    let items = list.items();
    let (mut line, mut at, mut visited) = (0u32, 0usize, 0usize);
    let mut visit = |item: &'a TrackItem, line: &mut u32, visited: &mut usize| -> bool {
        *visited += 1;
        match item {
            TrackItem::Names { .. } => true,
            TrackItem::Size(size) => {
                let go = each(*line, size);
                *line = line.saturating_add(1);
                go
            }
            TrackItem::Repeat { .. } => true,
        }
    };
    while let Some(item) = items.get(at) {
        if let TrackItem::Repeat { repeat, len } = item {
            let times = match repeat {
                Repeat::Count(n) => *n,
                Repeat::AutoFill | Repeat::AutoFit => repeats,
            };
            let body = items.get(at + 1..at + 1 + len).unwrap_or(&[]);
            for _ in 0..times {
                for item in body {
                    if !visit(item, &mut line, &mut visited) {
                        return visited;
                    }
                }
            }
            at += 1 + len;
        } else {
            if !visit(item, &mut line, &mut visited) {
                return visited;
            }
            at += 1;
        }
    }
    visited
}

/// The tracks `list` holds, its automatic repeat `repeats` times, and
/// where that repeat's tracks start and how many it has.
fn track_count(list: &TrackList, repeats: u32) -> (i64, Option<(i64, i64)>) {
    let items = list.items();
    let (mut count, mut at, mut auto) = (0i64, 0usize, None);
    while let Some(item) = items.get(at) {
        match item {
            TrackItem::Size(_) => count += 1,
            TrackItem::Repeat { repeat, len } => {
                let body = items.get(at + 1..at + 1 + len).unwrap_or(&[]);
                let sizes = body
                    .iter()
                    .filter(|i| matches!(i, TrackItem::Size(_)))
                    .count() as i64;
                let times = match repeat {
                    Repeat::Count(n) => i64::from(*n),
                    Repeat::AutoFill | Repeat::AutoFit => {
                        auto = Some((count, sizes * i64::from(repeats)));
                        i64::from(repeats)
                    }
                };
                count += sizes * times;
                at += *len;
            }
            TrackItem::Names { .. } => {}
        }
        at += 1;
    }
    (count, auto)
}

/// A breadth as a sizing function, a percentage auto where `basis` is
/// indefinite (§7.2.1).
fn breadth(b: &Breadth, basis: Option<f64>) -> Sizing {
    let fixed = |value: f64| Sizing {
        kind: Kind::Fixed,
        value: units(value.max(0.0)).min(NONE),
    };
    match b {
        Breadth::Length(Length::Px(n)) if n.is_finite() => fixed(*n),
        Breadth::Length(l) => match basis.and_then(|basis| l.resolve(basis)) {
            Some(value) => fixed(value),
            None => Sizing::AUTO,
        },
        Breadth::Flex(f) => Sizing {
            kind: Kind::Flex,
            value: if f.is_finite() && *f > 0.0 {
                (f * ONE as f64).round().min((1u64 << 40) as f64) as i64
            } else {
                0
            },
        },
        Breadth::Auto => Sizing::AUTO,
        Breadth::MinContent => Sizing {
            kind: Kind::MinContent,
            value: 0,
        },
        Breadth::MaxContent => Sizing {
            kind: Kind::MaxContent,
            value: 0,
        },
    }
}

/// A track size's minimum and maximum sizing functions (§7.2.3): `1fr`
/// is `minmax(auto, 1fr)`, `fit-content(L)` an auto minimum and L capping
/// a max-content maximum.
fn sizing(size: &TrackSize, basis: Option<f64>) -> (Sizing, Sizing) {
    match size {
        TrackSize::Breadth(b) => {
            let s = breadth(b, basis);
            if s.kind == Kind::Flex {
                (Sizing::AUTO, s)
            } else {
                (s, s)
            }
        }
        TrackSize::MinMax(a, b) => {
            let min = breadth(a, basis);
            let min = if min.kind == Kind::Flex {
                Sizing::AUTO
            } else {
                min
            };
            (min, breadth(b, basis))
        }
        TrackSize::FitContent(l) => {
            let max = match breadth(&Breadth::Length(*l), basis) {
                Sizing {
                    kind: Kind::Fixed,
                    value,
                } => Sizing {
                    kind: Kind::Fit,
                    value,
                },
                _ => Sizing {
                    kind: Kind::MaxContent,
                    value: 0,
                },
            };
            (Sizing::AUTO, max)
        }
    }
}

/// A fixed size of `size` for counting repeats: its maximum where fixed,
/// floored by its minimum where that is too, else its minimum, at least
/// 1 px (§7.2.3.2).
fn repeat_size(size: &TrackSize, basis: Option<f64>) -> i64 {
    let (min, max) = sizing(size, basis);
    let least = if min.kind == Kind::Fixed {
        min.value
    } else {
        0
    };
    let value = if max.kind == Kind::Fixed {
        max.value.max(least)
    } else {
        least
    };
    value.max(64)
}

/// The times `list`'s automatic repeat repeats in `fill`, `gap` between
/// tracks; at least once, and its tracks within MAX_GRID_LINE.
fn repeat_count(list: &TrackList, fill: Fill, gap: i64, basis: Option<f64>) -> u32 {
    let items = list.items();
    let (mut outside, mut tracks, mut body, mut each) = (0i64, 0i64, 0i64, 0i64);
    let mut at = 0;
    while let Some(item) = items.get(at) {
        match item {
            TrackItem::Size(size) => {
                outside = outside.saturating_add(repeat_size(size, basis));
                tracks += 1;
            }
            TrackItem::Repeat { repeat, len } => {
                let part = items.get(at + 1..at + 1 + len).unwrap_or(&[]);
                let (mut sum, mut count) = (0i64, 0i64);
                for item in part {
                    if let TrackItem::Size(size) = item {
                        sum = sum.saturating_add(repeat_size(size, basis));
                        count += 1;
                    }
                }
                match repeat {
                    Repeat::Count(n) => {
                        outside = outside.saturating_add(sum.saturating_mul(i64::from(*n)));
                        tracks += count * i64::from(*n);
                    }
                    Repeat::AutoFill | Repeat::AutoFit => (body, each) = (sum, count),
                }
                at += *len;
            }
            TrackItem::Names { .. } => {}
        }
        at += 1;
    }
    if each == 0 {
        return 1;
    }
    let most = ((i64::from(MAX_GRID_LINE) - tracks) / each).max(1);
    // n repeats take outside + n body + (tracks + n each - 1) gaps.
    let step = body.saturating_add(each.saturating_mul(gap)).max(1);
    let fixed = outside.saturating_add((tracks - 1).saturating_mul(gap));
    let n = match fill {
        Fill::Once => 1,
        Fill::Most(room) => room.saturating_sub(fixed).div_euclid(step),
        Fill::Least(least) => {
            let need = least.saturating_sub(fixed);
            need.saturating_add(step - 1).div_euclid(step)
        }
    };
    n.clamp(1, most) as u32
}

/// `value` of `property`, its `auto` the container's `items`, `legacy`
/// its value (Box Alignment 3 §6): `normal` stretches but a picture,
/// which starts.
fn self_align(own: &str, items: &str, inline: bool, picture: bool) -> Align {
    let value = if own == "auto" || own.is_empty() {
        items
    } else {
        own
    };
    let value = value.strip_prefix("legacy ").unwrap_or(value);
    match value {
        "normal" | "" if picture => Align::Start,
        "normal" | "stretch" | "" => Align::Stretch,
        "center" => Align::Center,
        "end" | "self-end" | "flex-end" => Align::End,
        "right" if inline => Align::End,
        _ => Align::Start,
    }
}

/// A grid laid or measured: its items' and tracks' ranges of the stacks.
#[derive(Clone, Copy)]
struct Grid {
    items: (usize, usize),
    cols: (usize, usize),
    rows: (usize, usize),
}

/// What a band of rows is laid with.
#[derive(Clone, Copy)]
struct Rows<'a> {
    /// `align-content`.
    content: &'a str,
    gap: i64,
    /// Sized before any item is laid.
    known: bool,
    /// One band: their sizes couple across the grid.
    coupled: bool,
    /// An auto height without bounds: rows reach their growth limits.
    free: bool,
    space: Space,
    stretch: Option<i64>,
    align: Option<i64>,
}

impl<M: FontMetrics> Engine<'_, '_, '_, '_, M> {
    /// Whether `b` is a grid container, on a page with any.
    pub(super) fn grid_container(&self, b: &BoxNode) -> Result<bool, Error> {
        if !self.styled.has_grid() {
            return Ok(false);
        }
        let style = self.styled.style(b.style).ok_or(Error::Invalid)?;
        Ok(grid_container(style, b.kind))
    }
    /// Whether `b`'s parent is a grid container, which makes `b` a grid
    /// item and a formatting context's root.
    pub(super) fn grid_item(&self, b: &BoxNode) -> Result<bool, Error> {
        match b.parent {
            Some(parent) if self.styled.has_grid() => {
                let parent = self.styled.box_node(parent).ok_or(Error::Invalid)?;
                self.grid_container(parent)
            }
            _ => Ok(false),
        }
    }
    /// The min-content and max-content widths of the grid container
    /// `b`'s content: its columns sized under each constraint, a
    /// percentage auto, each item measured once (§12.3).
    #[cold]
    #[inline(never)]
    pub(super) fn grid_intrinsic(
        &mut self,
        b: &BoxNode,
        depth: usize,
    ) -> Result<(i64, i64), Error> {
        if depth > self.counters.limits.depth {
            return Err(Error::Limit("MAX_DEPTH"));
        }
        let saved = (self.grid.len(), self.grid_tracks.len());
        let measured = self.measure_grid(b, depth);
        self.grid.truncate(saved.0);
        self.grid_tracks.truncate(saved.1);
        measured
    }
    fn measure_grid(&mut self, b: &BoxNode, depth: usize) -> Result<(i64, i64), Error> {
        let styled = self.styled;
        let style = styled.style(b.style).ok_or(Error::Invalid)?;
        let gap = length(style, Property::ColumnGap, None).map_or(0, |g| units(g.max(0.0)));
        let px_of = |p| length(style, p, None).map(|v| units(v.max(0.0)));
        let fill = match (px_of(Property::MaxWidth), px_of(Property::MinWidth)) {
            (Some(most), _) => Fill::Most(most),
            (None, Some(least)) if least > 0 => Fill::Least(least),
            _ => Fill::Once,
        };
        let rows = match px_of(Property::Height) {
            Some(height) => Fill::Most(height),
            None => Fill::Once,
        };
        let grid = self.build(b, (fill, rows), (None, None), (gap, 0))?;
        let mut sized = (0i64, 0i64);
        for (space, size) in [(Space::Min, &mut sized.0), (Space::Max, &mut sized.1)] {
            self.measure_items(grid, b.style, space, depth)?;
            self.size_tracks(grid, false, gap, space, None)?;
            *size = self.span_extent(grid.cols, gap)?;
        }
        Ok((sized.0.max(0), sized.1.max(sized.0).max(0)))
    }
    /// The size of the tracks `range`, `gap` between those not collapsed.
    fn span_extent(&mut self, (start, end): (usize, usize), gap: i64) -> Result<i64, Error> {
        self.counters.step(end - start)?;
        let tracks = self.grid_tracks.get(start..end).ok_or(Error::Invalid)?;
        let open = tracks.iter().filter(|t| !t.collapsed).count() as i64;
        let sizes = tracks
            .iter()
            .fold(0i64, |sum, t| sum.saturating_add(t.base));
        Ok(sizes.saturating_add(gap.saturating_mul((open - 1).max(0))))
    }
    /// Gathers `b`'s items, places them and makes its tracks: the explicit
    /// grid's, each automatic repeat repeated by its `fill`, and the
    /// implicit ones, percentages of `basis` (§7, §8).
    fn build(
        &mut self,
        b: &BoxNode,
        fill: (Fill, Fill),
        basis: (Option<f64>, Option<f64>),
        gaps: (i64, i64),
    ) -> Result<Grid, Error> {
        let styled = self.styled;
        let style = styled.style(b.style).ok_or(Error::Invalid)?;
        let tracks = |p| match style.get(p) {
            Some(ComputedValue::GridTracks(list)) => Some(&**list),
            _ => None,
        };
        let areas = match style.get(Property::GridTemplateAreas) {
            Some(ComputedValue::GridAreas(areas)) => Some(&**areas),
            _ => None,
        };
        let columns = Axis::of(
            tracks(Property::GridTemplateColumns),
            areas,
            false,
            (fill.0, gaps.0, basis.0),
        );
        let rows = Axis::of(
            tracks(Property::GridTemplateRows),
            areas,
            true,
            (fill.1, gaps.1, basis.1),
        );
        let start = self.grid.len();
        let mut next = b.first_child;
        let align = (
            keyword(style, Property::JustifyItems),
            keyword(style, Property::AlignItems),
        );
        // Placements are read once per style, of the last eight met:
        // items sharing one place alike.
        let mut cache: [Option<(StyleId, Placed, Placed)>; PLACEMENT_CACHE] =
            [None; PLACEMENT_CACHE];
        let mut replace = 0usize;
        // The engine's indexes, out of it while its items are placed: no
        // grid is built while another's items are, and an error ends the
        // layout.
        let empty = (Names::new(self.memory)?, Names::new(self.memory)?);
        let mut names = std::mem::replace(&mut self.grid_names, empty);
        names.0.reset();
        names.1.reset();
        while let Some(FlexItem { source, .. }) = self.next_flex_item(&mut next, "auto")? {
            let mut item = GridItem::new(source);
            let (row, col) = match source {
                Source::Element(child) => {
                    let c = styled.box_node(child).ok_or(Error::Invalid)?;
                    let own = styled.style(c.style).ok_or(Error::Invalid)?;
                    let picture = c.kind == BoxKind::Image;
                    let auto = |p| length(own, p, Some(0.0)).is_none();
                    item.autos = [
                        auto(Property::MarginTop),
                        auto(Property::MarginRight),
                        auto(Property::MarginBottom),
                        auto(Property::MarginLeft),
                    ];
                    let own_align = |p| keyword(own, p);
                    item.justify =
                        self_align(own_align(Property::JustifySelf), align.0, true, picture);
                    item.align =
                        self_align(own_align(Property::AlignSelf), align.1, false, picture);
                    let cached = cache.iter().flatten().find(|(id, ..)| *id == c.style);
                    match cached.copied() {
                        Some((_, row, col)) => (row, col),
                        None => {
                            let row = self.place_axis(
                                (&rows, &mut names.0),
                                edge(own, Property::GridRowStart),
                                edge(own, Property::GridRowEnd),
                            )?;
                            let col = self.place_axis(
                                (&columns, &mut names.1),
                                edge(own, Property::GridColumnStart),
                                edge(own, Property::GridColumnEnd),
                            )?;
                            if let Some(entry) = cache.get_mut(replace % PLACEMENT_CACHE) {
                                *entry = Some((c.style, row, col));
                            }
                            replace = replace.wrapping_add(1);
                            (row, col)
                        }
                    }
                }
                Source::Run(..) => {
                    item.justify = self_align("auto", align.0, true, false);
                    item.align = self_align("auto", align.1, false, false);
                    (Placed::Auto(1), Placed::Auto(1))
                }
            };
            let split = |p: Placed| match p {
                Placed::Lines(s, e) => ((s, e), true),
                Placed::Auto(n) => ((0, n), false),
            };
            (item.rows, item.fixed.0) = split(row);
            (item.cols, item.fixed.1) = split(col);
            self.counters.admit_growth(&mut self.grid)?;
            self.grid.try_push(item)?;
        }
        self.grid_names = names;
        let end = self.grid.len();
        let flow = keyword(style, Property::GridAutoFlow);
        let column_flow = flow.starts_with("column");
        let (lanes, slots, offset) =
            self.auto_place((start, end), (rows.count, columns.count), column_flow)?;
        let (row_count, col_count, (row_lo, col_lo)) = if column_flow {
            (slots, lanes, (offset.1, offset.0))
        } else {
            (lanes, slots, offset)
        };
        let cells = (row_count as usize).saturating_mul(col_count as usize);
        if cells > MAX_GRID_CELLS {
            return Err(Error::Limit("MAX_GRID_CELLS"));
        }
        let fit = |list: Option<&TrackList>| {
            list.is_some_and(|l| {
                l.items().iter().any(|i| {
                    matches!(
                        i,
                        TrackItem::Repeat {
                            repeat: Repeat::AutoFit,
                            ..
                        }
                    )
                })
            })
        };
        let cols = self.make_tracks(
            &columns,
            tracks(Property::GridAutoColumns),
            (col_lo, col_count),
            basis.0,
        )?;
        if fit(columns.list) {
            self.collapse(cols, (start, end), &columns, col_lo, false)?;
        }
        let rows_range = self.make_tracks(
            &rows,
            tracks(Property::GridAutoRows),
            (row_lo, row_count),
            basis.1,
        )?;
        Ok(Grid {
            items: (start, end),
            cols,
            rows: rows_range,
        })
    }
    /// Pushes `count` tracks for `axis`, the first `lo` from the explicit
    /// grid's: template sizes, and the auto ones cycling around them
    /// (§7.6).
    fn make_tracks(
        &mut self,
        axis: &Axis<'_>,
        auto: Option<&TrackList>,
        (lo, count): (i32, u32),
        basis: Option<f64>,
    ) -> Result<(usize, usize), Error> {
        let first = self.grid_tracks.len();
        self.counters.step(count as usize)?;
        let sizes = auto.map_or(&[][..], |list| list.items());
        let template = axis
            .list
            .map_or(0, |list| track_count(list, axis.repeats).0) as i32;
        let k = sizes.len() as i32;
        for at in 0..count as i32 {
            let explicit = at + lo;
            let cycled = if k == 0 {
                None
            } else if explicit >= template {
                sizes.get(((explicit - template) % k) as usize)
            } else {
                sizes.get((k - 1 - ((-explicit - 1) % k)) as usize)
            };
            let (min, max) = match cycled {
                Some(TrackItem::Size(size)) => sizing(size, basis),
                _ => (Sizing::AUTO, Sizing::AUTO),
            };
            self.counters.admit_growth(&mut self.grid_tracks)?;
            self.grid_tracks.try_push(Track::new(min, max))?;
        }
        if let Some(list) = axis.list {
            let tracks = &mut self.grid_tracks;
            let visited = walk(list, axis.repeats, &mut |track, size| {
                let at = i64::from(track) - i64::from(lo);
                if at >= 0 && at < i64::from(count) {
                    if let Some(t) = tracks.get_mut(first + at as usize) {
                        let (min, max) = sizing(size, basis);
                        *t = Track::new(min, max);
                    }
                }
                true
            });
            self.counters.step(visited)?;
        }
        Ok((first, self.grid_tracks.len()))
    }
    /// Collapses the empty tracks of an `auto-fit` repeat (§7.2.3.2):
    /// fixed at 0, their gaps with them.
    fn collapse(
        &mut self,
        (first, end): (usize, usize),
        items: (usize, usize),
        axis: &Axis<'_>,
        lo: i32,
        rows: bool,
    ) -> Result<(), Error> {
        let Some((at, len)) = axis.list.and_then(|l| track_count(l, axis.repeats).1) else {
            return Ok(());
        };
        for track in self.grid_tracks.get_mut(first..end).ok_or(Error::Invalid)? {
            track.touched = false;
        }
        for k in items.0..items.1 {
            let (s, e) = self.grid.get(k).ok_or(Error::Invalid)?.span(rows);
            self.counters.step(e.saturating_sub(s) + 1)?;
            for track in self
                .grid_tracks
                .get_mut(first + s..(first + e).min(end))
                .ok_or(Error::Invalid)?
            {
                track.touched = true;
            }
        }
        let start = (at - i64::from(lo)).max(0) as usize;
        let stop = ((at + len - i64::from(lo)).max(0) as usize).min(end - first);
        for track in self
            .grid_tracks
            .get_mut(first + start.min(stop)..first + stop)
            .ok_or(Error::Invalid)?
        {
            if !track.touched {
                let zero = Sizing {
                    kind: Kind::Fixed,
                    value: 0,
                };
                *track = Track::new(zero, zero);
                track.collapsed = true;
            }
            track.touched = false;
        }
        Ok(())
    }
    /// `axis`'s line names, indexed from the list as written: a unit of
    /// work an item of it and a unit a name; the sort's bound, a unit a
    /// comparison and one for each 64 bytes of the longest name, charged
    /// before it; and a unit a line or block each name is given.
    fn index_names<'a>(&mut self, axis: &Axis<'a>, names: &mut Names<'a>) -> Result<(), Error> {
        names.built = true;
        let Some(list) = axis.list else {
            return Ok(());
        };
        let items = list.items();
        let (mut line, mut at, mut longest) = (0i32, 0usize, 0usize);
        while let Some(item) = items.get(at) {
            self.counters.step(1)?;
            match item {
                TrackItem::Names { start, len } => {
                    for n in list.line_names(*start, *len).unwrap_or(&[]) {
                        self.counters.step(1)?;
                        longest = longest.max(n.len());
                        self.counters.admit_growth(&mut names.found)?;
                        names.found.try_push((&**n, index(at)?, Spot::Line(line)))?;
                    }
                    at += 1;
                }
                TrackItem::Size(_) => {
                    line = line.saturating_add(1);
                    at += 1;
                }
                TrackItem::Repeat { repeat, len } => {
                    let times = match repeat {
                        Repeat::Count(n) => *n,
                        Repeat::AutoFill | Repeat::AutoFit => axis.repeats,
                    };
                    let times = i32::try_from(times).unwrap_or(i32::MAX).max(1);
                    let body = items.get(at + 1..at + 1 + len).unwrap_or(&[]);
                    self.counters.step(body.len())?;
                    let period = body
                        .iter()
                        .filter(|i| matches!(i, TrackItem::Size(_)))
                        .count() as i32;
                    let mut offset = 0i32;
                    for (j, inner) in body.iter().enumerate() {
                        match inner {
                            TrackItem::Names { start, len } if period > 0 => {
                                for n in list.line_names(*start, *len).unwrap_or(&[]) {
                                    self.counters.step(1)?;
                                    longest = longest.max(n.len());
                                    let spot = Spot::Repeat {
                                        start: line,
                                        period,
                                        times,
                                        offset,
                                    };
                                    self.counters.admit_growth(&mut names.found)?;
                                    names.found.try_push((&**n, index(at + 1 + j)?, spot))?;
                                }
                            }
                            TrackItem::Size(_) => offset += 1,
                            _ => {}
                        }
                    }
                    line = line.saturating_add(times.saturating_mul(period));
                    at += 1 + len;
                }
            }
        }
        let n = names.found.len();
        let bits = (usize::BITS - n.leading_zeros()) as usize;
        self.counters
            .step(n.saturating_mul(bits + 1).saturating_mul(1 + longest / 64))?;
        names
            .found
            .sort_unstable_by(|a, b| a.0.cmp(b.0).then(a.1.cmp(&b.1)));
        let mut run = 0usize;
        while let Some(&(name, ..)) = names.found.get(run) {
            let end = run
                + names
                    .found
                    .get(run..)
                    .unwrap_or(&[])
                    .partition_point(|f| f.0 == name);
            let (blocks, points) = (names.blocks.len(), names.points.len());
            let mut at = run;
            while let Some(&(_, _, spot)) = names.found.get(at).filter(|_| at < end) {
                self.counters.step(1)?;
                match spot {
                    Spot::Line(l) => {
                        self.counters.admit_growth(&mut names.points)?;
                        names.points.try_push(l)?;
                        at += 1;
                    }
                    Spot::Repeat {
                        start,
                        period,
                        times,
                        ..
                    } => {
                        // This repeat's names of `name`, offsets ascending.
                        let first = names.residues.len();
                        let mut tail = false;
                        while let Some(&(
                            _,
                            _,
                            Spot::Repeat {
                                start: s, offset, ..
                            },
                        )) = names.found.get(at).filter(|_| at < end)
                        {
                            if s != start {
                                break;
                            }
                            if offset >= period {
                                tail = true;
                            } else if names.residues.last() != Some(&offset)
                                || names.residues.len() == first
                            {
                                self.counters.admit_growth(&mut names.residues)?;
                                names.residues.try_push(offset)?;
                            }
                            at += 1;
                        }
                        let held = (index(first)?, index(names.residues.len())?);
                        if held.1 > held.0 {
                            self.counters.admit_growth(&mut names.blocks)?;
                            names.blocks.try_push(Block {
                                start,
                                period,
                                times,
                                residues: held,
                            })?;
                        }
                        if tail {
                            // Names after the body's last track: each
                            // repetition's end, the next one's start.
                            let zero = names.residues.get(first) == Some(&0);
                            if !zero && times > 1 {
                                let at0 = index(names.residues.len())?;
                                self.counters.admit_growth(&mut names.residues)?;
                                names.residues.try_push(0)?;
                                self.counters.admit_growth(&mut names.blocks)?;
                                names.blocks.try_push(Block {
                                    start: start.saturating_add(period),
                                    period,
                                    times: times - 1,
                                    residues: (at0, at0 + 1),
                                })?;
                            }
                            self.counters.admit_growth(&mut names.points)?;
                            names
                                .points
                                .try_push(start.saturating_add(times.saturating_mul(period)))?;
                        }
                    }
                }
            }
            // Its other lines ascending, once, and none a block holds.
            let last = names.points.len();
            let mine = names.points.get_mut(points..last).ok_or(Error::Invalid)?;
            self.counters.step(mine.len().saturating_mul(bits + 1))?;
            mine.sort_unstable();
            let mut write = points;
            for read in points..last {
                let v = *names.points.get(read).ok_or(Error::Invalid)?;
                let held = {
                    let own = names.blocks.get(blocks..).unwrap_or(&[]);
                    self.counters.step(own.len() + 1)?;
                    named_holds(own, &names.residues, &[], v)
                };
                let seen = write > points && names.points.get(write - 1) == Some(&v);
                if !held && !seen {
                    *names.points.get_mut(write).ok_or(Error::Invalid)? = v;
                    write += 1;
                }
            }
            names.points.truncate(write);
            let entry = (
                name,
                (index(blocks)?, index(names.blocks.len())?),
                (index(points)?, index(names.points.len())?),
            );
            self.counters.admit_growth(&mut names.entries)?;
            names.entries.try_push(entry)?;
            run = end;
        }
        names.found.truncate(0);
        Ok(())
    }
    /// The lines named `name` in `axis`, ascending, the area edge `name`
    /// gives among them (§7.3.2): how many there are, how many lie before
    /// `below`, and the `k`th; from the index, a unit of work a probe of
    /// the names and one for each 64 bytes of the name, and a unit a block
    /// and a probe of its lines each time they are counted.
    fn named_lines<'a>(
        &mut self,
        (axis, names): (&Axis<'a>, &mut Names<'a>),
        name: (&str, &str),
        k: usize,
        below: i32,
    ) -> Result<(usize, usize, Option<i32>), Error> {
        let full = name.0.len() + name.1.len();
        let area = |side: &str| -> Option<i32> {
            let base = if name.1.is_empty() {
                name.0.strip_suffix(side)?
            } else if name.1 == side {
                name.0
            } else {
                return None;
            };
            let areas = axis.areas?.areas();
            let at = areas.binary_search_by(|a| (*a.name).cmp(base)).ok()?;
            let a = areas.get(at)?;
            let line = match (axis.rows, side == "-start") {
                (true, true) => a.row_start,
                (true, false) => a.row_end,
                (false, true) => a.column_start,
                (false, false) => a.column_end,
            };
            i32::try_from(line).ok()
        };
        let pending = if full > 0 {
            area("-start").or_else(|| area("-end"))
        } else {
            None
        };
        if !names.built {
            self.index_names(axis, names)?;
        }
        let entries = &names.entries;
        let probes = (usize::BITS - entries.len().leading_zeros()) as usize + 1;
        self.counters.step(probes * (1 + full / 64))?;
        let found = entries
            .binary_search_by(|e| name_order(e.0, name))
            .ok()
            .and_then(|at| entries.get(at));
        let (blocks, points) = match found {
            Some(&(_, b, p)) => (
                names.blocks.get(b.0 as usize..b.1 as usize).unwrap_or(&[]),
                names.points.get(p.0 as usize..p.1 as usize).unwrap_or(&[]),
            ),
            None => (&[][..], &[][..]),
        };
        let residues: &[i32] = &names.residues;
        // The area's edge, merged in where no named line is it.
        let extra = pending.filter(|a| !named_holds(blocks, residues, points, *a));
        let probe = blocks.len() + 2 * (usize::BITS - points.len().leading_zeros()) as usize + 1;
        let below_v = |v: i32| {
            named_below(blocks, residues, points, v) + usize::from(extra.is_some_and(|a| a < v))
        };
        let total = below_v(i32::MAX);
        let before = below_v(below);
        let mut counted = 2usize;
        let kth = if k < total {
            // The least line with more than `k` named lines up to it.
            let lows = blocks
                .iter()
                .map(|b| b.start)
                .chain(points.first().copied());
            let highs = blocks
                .iter()
                .map(|b| b.start.saturating_add(b.times.saturating_mul(b.period)))
                .chain(points.last().copied());
            let (mut lo, mut hi) = (
                lows.chain(extra).min().unwrap_or(0),
                highs.chain(extra).max().unwrap_or(0),
            );
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                counted += 1;
                if below_v(mid.saturating_add(1)) > k {
                    hi = mid;
                } else {
                    lo = mid + 1;
                }
            }
            Some(lo)
        } else {
            None
        };
        self.counters.step(counted.saturating_mul(probe))?;
        Ok((total, before, kth))
    }
    /// The `n`th line named `name` from the start, or from the end where
    /// `n` is negative; past those, implicit lines count as named (§8.3).
    fn nth_named<'a>(
        &mut self,
        (axis, names): (&Axis<'a>, &mut Names<'a>),
        name: &str,
        n: i32,
    ) -> Result<i32, Error> {
        if n > 0 {
            let k = (n - 1) as usize;
            let (total, _, kth) = self.named_lines((axis, &mut *names), (name, ""), k, 0)?;
            Ok(kth.unwrap_or(axis.count + (n - total as i32)))
        } else {
            let m = n.unsigned_abs() as usize;
            let (total, ..) = self.named_lines((axis, &mut *names), (name, ""), usize::MAX, 0)?;
            if m <= total {
                let (_, _, kth) =
                    self.named_lines((axis, &mut *names), (name, ""), total - m, 0)?;
                kth.ok_or(Error::Invalid)
            } else {
                Ok(-((m - total) as i32))
            }
        }
    }
    /// A line an edge names, None for auto and spans (§8.3).
    fn line<'a>(
        &mut self,
        (axis, names): (&Axis<'a>, &mut Names<'a>),
        e: Edge<'_>,
        end: bool,
    ) -> Result<Option<i32>, Error> {
        Ok(match e {
            Edge::Line(n, None) if n > 0 => Some(n - 1),
            Edge::Line(n, None) => Some(axis.count + 1 + n),
            Edge::Line(0, Some(name)) => {
                let side = if end { "-end" } else { "-start" };
                let (total, _, first) =
                    self.named_lines((axis, &mut *names), (name, side), 0, 0)?;
                match first.filter(|_| total > 0) {
                    Some(line) => Some(line),
                    None => Some(self.nth_named((axis, &mut *names), name, 1)?),
                }
            }
            Edge::Line(n, Some(name)) => Some(self.nth_named((axis, &mut *names), name, n)?),
            Edge::Auto | Edge::Span(..) => None,
        })
    }
    /// The line `n` lines named `name` from `from`, forward or back.
    fn span_from<'a>(
        &mut self,
        (axis, names): (&Axis<'a>, &mut Names<'a>),
        from: i32,
        (n, name): (i32, Option<&str>),
        forward: bool,
    ) -> Result<i32, Error> {
        let Some(name) = name else {
            return Ok(if forward {
                from.saturating_add(n)
            } else {
                from.saturating_sub(n)
            });
        };
        let n = n.max(1) as usize;
        if forward {
            let (total, upto, _) =
                self.named_lines((axis, &mut *names), (name, ""), usize::MAX, from + 1)?;
            let after = total - upto;
            if n <= after {
                let (_, _, kth) =
                    self.named_lines((axis, &mut *names), (name, ""), upto + n - 1, 0)?;
                return kth.ok_or(Error::Invalid);
            }
            Ok(from.max(axis.count) + (n - after) as i32)
        } else {
            let (_, before, _) =
                self.named_lines((axis, &mut *names), (name, ""), usize::MAX, from)?;
            if n <= before {
                let (_, _, kth) =
                    self.named_lines((axis, &mut *names), (name, ""), before - n, 0)?;
                return kth.ok_or(Error::Invalid);
            }
            Ok(from.min(0) - (n - before) as i32)
        }
    }
    /// An item's placement in `axis` from its start and end longhands,
    /// conflicts resolved as §8.3.1 has them, lines within MAX_GRID_LINE.
    fn place_axis<'a>(
        &mut self,
        (axis, names): (&Axis<'a>, &mut Names<'a>),
        start: Edge<'_>,
        end: Edge<'_>,
    ) -> Result<Placed, Error> {
        let span = |e: Edge<'_>| match e {
            Edge::Span(n, None) => Some(n),
            Edge::Span(_, Some(_)) => Some(1),
            _ => None,
        };
        let s = self.line((axis, &mut *names), start, false)?;
        let e = self.line((axis, &mut *names), end, true)?;
        let (s, e) = match (s, e) {
            (Some(s), Some(e)) if s > e => (e, s),
            (Some(s), Some(e)) if s == e => (s, s + 1),
            (Some(s), Some(e)) => (s, e),
            (Some(s), None) => match end {
                Edge::Span(n, name) => {
                    (s, self.span_from((axis, &mut *names), s, (n, name), true)?)
                }
                _ => (s, s + 1),
            },
            (None, Some(e)) => match start {
                Edge::Span(n, name) => {
                    (self.span_from((axis, &mut *names), e, (n, name), false)?, e)
                }
                _ => (e - 1, e),
            },
            (None, None) => {
                let n = span(start).or(span(end)).unwrap_or(1);
                return Ok(Placed::Auto(n.clamp(1, MAX_GRID_LINE)));
            }
        };
        let s = s.clamp(-MAX_GRID_LINE, MAX_GRID_LINE - 1);
        let e = e.clamp(s + 1, MAX_GRID_LINE);
        Ok(Placed::Lines(s, e))
    }
    /// Places the items `range` (§8.5): those of definite lines, then those
    /// locked to a lane, then the rest by a sparse cursor, lanes the rows
    /// (the columns in a column flow) and slots across them; returns the
    /// lanes, the slots and how far the explicit grid's first line lies
    /// from the implicit grid's in each.
    fn auto_place(
        &mut self,
        (start, end): (usize, usize),
        (rows, cols): (i32, i32),
        column_flow: bool,
    ) -> Result<(u32, u32, (i32, i32)), Error> {
        self.counters.step(end - start)?;
        let (major, minor) = if column_flow {
            (cols, rows)
        } else {
            (rows, cols)
        };
        if column_flow {
            for item in self.grid.get_mut(start..end).ok_or(Error::Invalid)? {
                std::mem::swap(&mut item.rows, &mut item.cols);
                item.fixed = (item.fixed.1, item.fixed.0);
            }
        }
        let (mut lo, mut hi) = ((0i32, 0i32), (major, minor));
        let (mut widest, mut free) = (1i32, false);
        for item in self.grid.get(start..end).ok_or(Error::Invalid)? {
            if item.fixed.0 {
                lo.0 = lo.0.min(item.rows.0);
                hi.0 = hi.0.max(item.rows.1);
            }
            if item.fixed.1 {
                lo.1 = lo.1.min(item.cols.0);
                hi.1 = hi.1.max(item.cols.1);
            } else {
                widest = widest.max(item.cols.1);
            }
            free |= !item.fixed.0 || !item.fixed.1;
        }
        for item in self.grid.get_mut(start..end).ok_or(Error::Invalid)? {
            if item.fixed.0 {
                item.rows = (item.rows.0 - lo.0, item.rows.1 - lo.0);
            }
            if item.fixed.1 {
                item.cols = (item.cols.0 - lo.1, item.cols.1 - lo.1);
            }
        }
        let mut lanes = (hi.0 - lo.0) as u32;
        let mut slots = ((hi.1 - lo.1).max(widest)) as u32;
        if free {
            let cells = self.grid_cells.len();
            let placed = self.occupy((start, end), &mut lanes, &mut slots);
            self.grid_cells.truncate(cells);
            self.grid_lanes.clear();
            placed?;
        }
        if column_flow {
            for item in self.grid.get_mut(start..end).ok_or(Error::Invalid)? {
                std::mem::swap(&mut item.rows, &mut item.cols);
                item.fixed = (item.fixed.1, item.fixed.0);
            }
        }
        Ok((lanes, slots, lo))
    }
    /// Auto-places the items of `range` with an occupancy bitset of
    /// `lanes` by `slots` cells, which grows as they need.
    fn occupy(
        &mut self,
        (start, end): (usize, usize),
        lanes: &mut u32,
        slots: &mut u32,
    ) -> Result<(), Error> {
        let mut map = Occupancy {
            base: self.grid_cells.len(),
            words: 0,
            lanes: 0,
        };
        self.restride(&mut map, *slots)?;
        self.grow_lanes(&mut map, *lanes, *slots)?;
        // Step 1: the items of definite lines in both axes.
        for k in start..end {
            let item = *self.grid.get(k).ok_or(Error::Invalid)?;
            if item.fixed.0 && item.fixed.1 {
                self.mark_cells(&mut map, item.rows, item.cols, *slots)?;
            }
        }
        // Step 2: those locked to a lane, each past those placed in it.
        for k in start..end {
            let item = *self.grid.get(k).ok_or(Error::Invalid)?;
            if !item.fixed.0 || item.fixed.1 {
                continue;
            }
            if self.grid_lanes.len() < *lanes as usize {
                self.counters.step(*lanes as usize)?;
                while self.grid_lanes.len() < *lanes as usize {
                    self.counters.admit_growth(&mut self.grid_lanes)?;
                    self.grid_lanes.try_push(0)?;
                }
            }
            let lane = item.rows.0 as usize;
            let span = item.cols.1;
            let mut slot = *self.grid_lanes.get(lane).ok_or(Error::Invalid)? as i32;
            while self.overlaps(&map, item.rows, (slot, slot + span))? {
                slot += 1;
            }
            if slot + span > *slots as i32 {
                *slots = (slot + span) as u32;
                self.restride(&mut map, *slots)?;
            }
            *self.grid_lanes.get_mut(lane).ok_or(Error::Invalid)? = (slot + span) as u32;
            let cols = (slot, slot + span);
            self.mark_cells(&mut map, item.rows, cols, *slots)?;
            self.grid.get_mut(k).ok_or(Error::Invalid)?.cols = cols;
        }
        // Step 4: the rest, by a cursor moving across lanes then down.
        let (mut lane, mut slot) = (0i32, 0i32);
        for k in start..end {
            let item = *self.grid.get(k).ok_or(Error::Invalid)?;
            if item.fixed.0 {
                continue;
            }
            let height = item.rows.1;
            let cols = if item.fixed.1 {
                if item.cols.0 < slot {
                    lane += 1;
                }
                slot = item.cols.0;
                while self.overlaps(&map, (lane, lane + height), item.cols)? {
                    lane += 1;
                }
                item.cols
            } else {
                let width = item.cols.1;
                loop {
                    if slot + width > *slots as i32 {
                        lane += 1;
                        slot = 0;
                        continue;
                    }
                    if !self.overlaps(&map, (lane, lane + height), (slot, slot + width))? {
                        break;
                    }
                    slot += 1;
                }
                (slot, slot + width)
            };
            let rows = (lane, lane.saturating_add(height));
            *lanes = (*lanes).max(rows.1 as u32);
            self.grow_lanes(&mut map, *lanes, *slots)?;
            self.mark_cells(&mut map, rows, cols, *slots)?;
            let placed = self.grid.get_mut(k).ok_or(Error::Invalid)?;
            (placed.rows, placed.cols) = (rows, cols);
            if !item.fixed.1 {
                slot = cols.1;
            }
        }
        Ok(())
    }
    /// The bitset with room for `slots` slots a lane, refused past
    /// MAX_GRID_CELLS, copied across where it grows.
    fn restride(&mut self, map: &mut Occupancy, slots: u32) -> Result<(), Error> {
        let words = (slots as usize).div_ceil(64).max(1);
        if words <= map.words {
            return Ok(());
        }
        if map.lanes.saturating_mul(slots as usize) > MAX_GRID_CELLS {
            return Err(Error::Limit("MAX_GRID_CELLS"));
        }
        let cells = map.lanes.saturating_mul(words);
        self.counters.step(cells)?;
        let old = map.words;
        let mut fresh = BudgetVec::new(self.memory)?;
        for lane in 0..map.lanes {
            for word in 0..words {
                let value = if word < old {
                    *self
                        .grid_cells
                        .get(map.base + lane * old + word)
                        .ok_or(Error::Invalid)?
                } else {
                    0
                };
                self.counters.admit_growth(&mut fresh)?;
                fresh.try_push(value)?;
            }
        }
        self.grid_cells.truncate(map.base);
        for value in fresh.iter() {
            self.counters.admit_growth(&mut self.grid_cells)?;
            self.grid_cells.try_push(*value)?;
        }
        map.words = words;
        Ok(())
    }
    /// The bitset with `lanes` lanes, refused past MAX_GRID_CELLS.
    fn grow_lanes(&mut self, map: &mut Occupancy, lanes: u32, slots: u32) -> Result<(), Error> {
        let lanes = lanes as usize;
        if lanes <= map.lanes {
            return Ok(());
        }
        if lanes.saturating_mul(slots as usize) > MAX_GRID_CELLS {
            return Err(Error::Limit("MAX_GRID_CELLS"));
        }
        self.counters.step((lanes - map.lanes) * map.words)?;
        for _ in 0..(lanes - map.lanes) * map.words {
            self.counters.admit_growth(&mut self.grid_cells)?;
            self.grid_cells.try_push(0)?;
        }
        map.lanes = lanes;
        Ok(())
    }
    /// Whether any cell of `rows` by `cols` is occupied, a unit of work
    /// for each word probed.
    fn overlaps(
        &mut self,
        map: &Occupancy,
        rows: (i32, i32),
        cols: (i32, i32),
    ) -> Result<bool, Error> {
        let (first, last) = (
            rows.0.max(0) as usize,
            (rows.1.max(0) as usize).min(map.lanes),
        );
        let (from, to) = (cols.0.max(0) as usize, cols.1.max(0) as usize);
        for lane in first..last {
            let mut slot = from;
            while slot < to {
                self.counters.step(1)?;
                let word = slot / 64;
                if word >= map.words {
                    break;
                }
                let bits = self
                    .grid_cells
                    .get(map.base + lane * map.words + word)
                    .copied()
                    .ok_or(Error::Invalid)?;
                let upto = (to - word * 64).min(64);
                let low = slot - word * 64;
                let mask = mask(low, upto);
                if bits & mask != 0 {
                    return Ok(true);
                }
                slot = (word + 1) * 64;
            }
        }
        Ok(false)
    }
    /// Marks the cells of `rows` by `cols` occupied.
    fn mark_cells(
        &mut self,
        map: &mut Occupancy,
        rows: (i32, i32),
        cols: (i32, i32),
        slots: u32,
    ) -> Result<(), Error> {
        self.grow_lanes(map, rows.1.max(0) as u32, slots)?;
        let (from, to) = (cols.0.max(0) as usize, cols.1.max(0) as usize);
        for lane in rows.0.max(0) as usize..rows.1.max(0) as usize {
            let mut slot = from;
            while slot < to {
                self.counters.step(1)?;
                let word = slot / 64;
                if word >= map.words {
                    break;
                }
                let upto = (to - word * 64).min(64);
                let low = slot - word * 64;
                let bits = self
                    .grid_cells
                    .get_mut(map.base + lane * map.words + word)
                    .ok_or(Error::Invalid)?;
                *bits |= mask(low, upto);
                slot = (word + 1) * 64;
            }
        }
        Ok(())
    }
    /// Measures each item of `grid` its columns' sizing under `space` needs
    /// at most once: its min-content, max-content and minimum widths
    /// (§6.6), charged as measuring is.
    fn measure_items(
        &mut self,
        grid: Grid,
        strut: StyleId,
        space: Space,
        depth: usize,
    ) -> Result<(), Error> {
        let styled = self.styled;
        // What was kept of the grid, read once an item needs measuring;
        // kept again only where one was.
        let mut known: Option<Option<(usize, bool)>> = None;
        let mut fresh = false;
        for k in grid.items.0..grid.items.1 {
            let item = *self.grid.get(k).ok_or(Error::Invalid)?;
            if item.measured {
                continue;
            }
            let (s, e) = item.span(false);
            self.counters.step(e.saturating_sub(s))?;
            let tracks = self
                .grid_tracks
                .get(grid.cols.0 + s..grid.cols.0 + e)
                .ok_or(Error::Invalid)?;
            let needed = tracks.iter().any(|t| {
                t.min.kind != Kind::Fixed
                    || t.max.intrinsic()
                    || (t.flexible() && matches!(space, Space::Max | Space::Indefinite(..)))
            });
            if !needed {
                continue;
            }
            if known.is_none() {
                known = Some(self.known_measures(grid)?);
                if self.grid.get(k).is_some_and(|i| i.measured) {
                    continue;
                }
            }
            fresh = true;
            let (least, most, floor, bare) = match item.source {
                Source::Run(first, until) => {
                    let (l, m) = self.content_with(Some(first), until, strut, None, depth + 1)?;
                    (l, m, l, 0)
                }
                Source::Element(id) => {
                    let c = *styled.box_node(id).ok_or(Error::Invalid)?;
                    let role = self.role(&c)?;
                    let (l, m) = self.block_contribution(id, c, role, None, depth + 1)?;
                    let style = styled.style(c.style).ok_or(Error::Invalid)?;
                    let simple = c.kind == BoxKind::Image
                        || role == Some(TableRole::Table)
                        || length(style, Property::Width, None).is_some();
                    let dims = if simple {
                        None
                    } else {
                        Some(sized(styled, &c, style, None, None, None)?)
                    };
                    let decoration = dims.map_or(0.0, |d| d.decoration().horizontal());
                    let margin = edges(
                        style,
                        [
                            Property::MarginTop,
                            Property::MarginRight,
                            Property::MarginBottom,
                            Property::MarginLeft,
                        ],
                        0.0,
                        false,
                    );
                    let around =
                        super::table::up(decoration) + super::table::up(margin.horizontal());
                    let content = |v: f64| {
                        if keyword(style, Property::BoxSizing) == "border-box" {
                            (v - decoration).max(0.0)
                        } else {
                            v.max(0.0)
                        }
                    };
                    if simple {
                        (l, m, l, l)
                    } else if let Some(low) = length(style, Property::MinWidth, None) {
                        let low = (super::table::up(content(low)) + around).max(0);
                        (l, m, low, low)
                    } else if !matches!(keyword(style, Property::OverflowX), "visible" | "clip") {
                        (l, m, around.max(0), around.max(0))
                    } else {
                        (l, m, l, around.max(0))
                    }
                }
            };
            let item = self.grid.get_mut(k).ok_or(Error::Invalid)?;
            (item.least, item.most, item.floor, item.bare) = (least, most, floor, bare);
            item.measured = true;
        }
        match known {
            Some(Some((key, true))) if fresh => self.keep_measures(grid, key),
            _ => Ok(()),
        }
    }
    /// The grid's items' contributions as kept before in this layout,
    /// copied to them, a unit of work an item; its entry in `grid_known`,
    /// and whether what it measures now is to be kept: from its second
    /// measure on. The table is made, 4 bytes a box, as a grid page first
    /// measures an item.
    fn known_measures(&mut self, grid: Grid) -> Result<Option<(usize, bool)>, Error> {
        let Some(first) = self.grid.get(grid.items.0).filter(|_| !memo_off()) else {
            return Ok(None);
        };
        let key = match first.source {
            Source::Element(id) | Source::Run(id, _) => id.index(),
        };
        if self.grid_known.is_empty() {
            let boxes = self.styled.boxes().len();
            self.counters.step(boxes)?;
            self.counters
                .charge(boxes.saturating_mul(size_of::<u32>()).saturating_add(32))?;
            BudgetVec::try_reserve_exact(&mut self.grid_known, boxes)?;
            for _ in 0..boxes {
                self.grid_known.try_push(UNMEASURED)?;
            }
        }
        let entry = self.grid_known.get_mut(key).ok_or(Error::Invalid)?;
        match *entry {
            UNMEASURED => {
                *entry = MEASURED_ONCE;
                return Ok(Some((key, false)));
            }
            MEASURED_ONCE => return Ok(Some((key, true))),
            _ => {}
        }
        let start = *entry as usize;
        let count = grid.items.1 - grid.items.0;
        self.counters.step(count)?;
        let kept = self
            .grid_measured
            .get(start..start + count)
            .ok_or(Error::Invalid)?;
        let items = self
            .grid
            .get_mut(grid.items.0..grid.items.1)
            .ok_or(Error::Invalid)?;
        for (item, m) in items.iter_mut().zip(kept) {
            if m.measured && !item.measured {
                (item.least, item.most, item.floor, item.bare) = (m.least, m.most, m.floor, m.bare);
                item.measured = true;
            }
        }
        Ok(Some((key, true)))
    }
    /// Keeps the grid's items' contributions for a later lay or measure,
    /// 40 bytes and a unit of work an item.
    fn keep_measures(&mut self, grid: Grid, key: usize) -> Result<(), Error> {
        let count = grid.items.1 - grid.items.0;
        self.counters.step(count)?;
        let entry = *self.grid_known.get(key).ok_or(Error::Invalid)?;
        let start = if entry == MEASURED_ONCE {
            let at = self.grid_measured.len();
            for _ in 0..count {
                self.counters.admit_growth(&mut self.grid_measured)?;
                self.grid_measured.try_push(Measured {
                    least: 0,
                    most: 0,
                    floor: 0,
                    bare: 0,
                    measured: false,
                })?;
            }
            let entry = self.grid_known.get_mut(key).ok_or(Error::Invalid)?;
            *entry = u32::try_from(at)
                .ok()
                .filter(|at| *at < MEASURED_ONCE)
                .ok_or(Error::Invalid)?;
            at
        } else {
            entry as usize
        };
        let items = self
            .grid
            .get(grid.items.0..grid.items.1)
            .ok_or(Error::Invalid)?;
        let kept = self
            .grid_measured
            .get_mut(start..start + count)
            .ok_or(Error::Invalid)?;
        for (m, item) in kept.iter_mut().zip(items) {
            *m = Measured {
                least: item.least,
                most: item.most,
                floor: item.floor,
                bare: item.bare,
                measured: item.measured,
            };
        }
        Ok(())
    }
    /// Sizes `grid`'s columns, or its rows, by §12.4 to §12.8: base sizes
    /// and growth limits from fixed functions, then from items (§12.5),
    /// maximized (§12.6), flexible tracks expanded (§12.7) and auto tracks
    /// stretched into what `stretch` leaves (§12.8).
    fn size_tracks(
        &mut self,
        grid: Grid,
        rows: bool,
        gap: i64,
        space: Space,
        stretch: Option<i64>,
    ) -> Result<(), Error> {
        let tracks = if rows { grid.rows } else { grid.cols };
        self.size_range(tracks, grid.items, (rows, 0), gap, space, stretch)
    }
    /// `size_tracks` for the tracks `range`, the `offset`th of the axis
    /// first, and those of the items `items` within them.
    fn size_range(
        &mut self,
        (first, end): (usize, usize),
        items: (usize, usize),
        (rows, offset): (bool, usize),
        gap: i64,
        space: Space,
        stretch: Option<i64>,
    ) -> Result<(), Error> {
        let count = end - first;
        self.counters.step(count)?;
        for track in self.grid_tracks.get_mut(first..end).ok_or(Error::Invalid)? {
            track.base = if track.min.kind == Kind::Fixed {
                track.min.value
            } else {
                0
            };
            track.limit = if track.max.kind == Kind::Fixed {
                track.max.value.max(track.base)
            } else {
                NONE
            };
            (track.planned, track.incurred, track.growable) = (0, 0, false);
        }
        let constrained = matches!(space, Space::Min | Space::Max);
        // Step 2: items of one track not flexible.
        let order = self.grid_order.len();
        let sorted = self.sort_spans(items, (first, end), (rows, offset), constrained);
        let laid = sorted.and_then(|()| {
            self.span_items(order, (first, offset), rows, gap, space)?;
            self.flexible_items(order, (first, offset), rows, gap, space)
        });
        self.grid_order.truncate(order);
        laid?;
        for track in self.grid_tracks.get_mut(first..end).ok_or(Error::Invalid)? {
            if track.limit == NONE {
                track.limit = track.base;
            }
        }
        // §12.6: maximize.
        match space {
            Space::Definite(room) => {
                let free = room.saturating_sub(self.span_extent((first, end), gap)?);
                if free > 0 {
                    self.fill_bases((first, end), free)?;
                }
            }
            Space::Indefinite(_, Some(most)) if self.limits_extent((first, end), gap)? > most => {
                let free = most.saturating_sub(self.span_extent((first, end), gap)?);
                if free > 0 {
                    self.fill_bases((first, end), free)?;
                }
            }
            Space::Indefinite(..) | Space::Max => {
                for track in self.grid_tracks.get_mut(first..end).ok_or(Error::Invalid)? {
                    track.base = track.base.max(track.limit);
                }
            }
            Space::Min => {}
        }
        // §12.7: flexible tracks.
        if !matches!(space, Space::Min) {
            let any = self
                .grid_tracks
                .get(first..end)
                .ok_or(Error::Invalid)?
                .iter()
                .any(Track::flexible);
            if any {
                match space {
                    Space::Definite(room) => self.expand_definite((first, end), room, gap)?,
                    _ => {
                        let tracks = self.grid_tracks.get_mut(first..end).ok_or(Error::Invalid)?;
                        for track in tracks.iter_mut().filter(|t| t.flexible()) {
                            track.planned = track.base;
                        }
                        self.expand_indefinite((first, end), items, (rows, offset), gap)?;
                        if let Space::Indefinite(least, most) = space {
                            let extent = self.span_extent((first, end), gap)?;
                            let redo = match (least, most) {
                                (Some(least), _) if extent < least => Some(least),
                                (_, Some(most)) if extent > most => Some(most),
                                _ => None,
                            };
                            if let Some(room) = redo {
                                // The fr size the bound leaves (§12.7.1).
                                let tracks =
                                    self.grid_tracks.get_mut(first..end).ok_or(Error::Invalid)?;
                                for track in tracks.iter_mut().filter(|t| t.flexible()) {
                                    track.base = track.planned;
                                }
                                self.expand_definite((first, end), room, gap)?;
                            }
                        }
                    }
                }
            }
        }
        // §12.8: auto tracks stretched.
        if let Some(room) = stretch.filter(|_| !constrained) {
            let free = room.saturating_sub(self.span_extent((first, end), gap)?);
            let tracks = self.grid_tracks.get_mut(first..end).ok_or(Error::Invalid)?;
            let autos = tracks.iter().filter(|t| t.max.kind == Kind::Auto).count() as i64;
            if free > 0 && autos > 0 {
                let mut seen = 0i64;
                for track in tracks.iter_mut().filter(|t| t.max.kind == Kind::Auto) {
                    let before = free * seen / autos;
                    seen += 1;
                    track.base = track.base.saturating_add(free * seen / autos - before);
                }
            }
        }
        Ok(())
    }
    /// The extent of the tracks `range` were each at its growth limit.
    fn limits_extent(&mut self, (start, end): (usize, usize), gap: i64) -> Result<i64, Error> {
        self.counters.step(end - start)?;
        let tracks = self.grid_tracks.get(start..end).ok_or(Error::Invalid)?;
        let open = tracks.iter().filter(|t| !t.collapsed).count() as i64;
        let sizes = tracks
            .iter()
            .fold(0i64, |sum, t| sum.saturating_add(t.limit.max(t.base)));
        Ok(sizes.saturating_add(gap.saturating_mul((open - 1).max(0))))
    }
    /// Fills `grid_order` with the items of `items` in the tracks
    /// `range` (the `offset`th first), each keyed by its span, then
    /// index, those crossing a flexible track keyed past any span; and
    /// sizes those of one track not flexible (§12.5 step 2).
    fn sort_spans(
        &mut self,
        items: (usize, usize),
        (first, end): (usize, usize),
        (rows, offset): (bool, usize),
        constrained: bool,
    ) -> Result<(), Error> {
        let base = self.grid_order.len();
        for k in items.0..items.1 {
            let item = *self.grid.get(k).ok_or(Error::Invalid)?;
            let (s, e) = item.span(rows);
            if s < offset || e > offset + (end - first) {
                continue;
            }
            self.counters.step(e - s)?;
            let tracks = self
                .grid_tracks
                .get(first + s - offset..first + e - offset)
                .ok_or(Error::Invalid)?;
            let flexible = tracks.iter().any(Track::flexible);
            if e - s == 1 && !flexible {
                let track = self
                    .grid_tracks
                    .get_mut(first + s - offset)
                    .ok_or(Error::Invalid)?;
                single(track, &item, constrained);
                continue;
            }
            let key = if flexible { u32::MAX } else { (e - s) as u32 };
            self.counters.admit_growth(&mut self.grid_order)?;
            self.grid_order.try_push((key, index(k)?))?;
        }
        if let Some(order) = self.grid_order.get_mut(base..) {
            order.sort_unstable();
        }
        Ok(())
    }
    /// §12.5 step 3: spanning items crossing no flexible track, smallest
    /// span first, each span's items distributing together.
    fn span_items(
        &mut self,
        order: usize,
        at: (usize, usize),
        rows: bool,
        gap: i64,
        space: Space,
    ) -> Result<(), Error> {
        let mut group = order;
        while let Some(&(span, _)) = self.grid_order.get(group) {
            if span == u32::MAX {
                break;
            }
            let mut stop = group;
            while self.grid_order.get(stop).is_some_and(|(s, _)| *s == span) {
                stop += 1;
            }
            for phase in [Phase::MinBase, Phase::ContentBase, Phase::MaxBase] {
                self.phase((group, stop), at, rows, gap, (phase, space))?;
            }
            let (first, offset) = at;
            for k in group..stop {
                let (_, item) = *self.grid_order.get(k).ok_or(Error::Invalid)?;
                let (s, e) = self
                    .grid
                    .get(item as usize)
                    .ok_or(Error::Invalid)?
                    .span(rows);
                for track in self
                    .grid_tracks
                    .get_mut(first + s - offset..first + e - offset)
                    .ok_or(Error::Invalid)?
                {
                    track.limit = track.limit.max(track.base);
                }
            }
            for phase in [Phase::IntrinsicLimit, Phase::MaxLimit] {
                self.phase((group, stop), at, rows, gap, (phase, space))?;
            }
            for k in group..stop {
                let (_, item) = *self.grid_order.get(k).ok_or(Error::Invalid)?;
                let (s, e) = self
                    .grid
                    .get(item as usize)
                    .ok_or(Error::Invalid)?
                    .span(rows);
                for track in self
                    .grid_tracks
                    .get_mut(first + s - offset..first + e - offset)
                    .ok_or(Error::Invalid)?
                {
                    track.growable = false;
                }
            }
            group = stop;
        }
        Ok(())
    }
    /// §12.5 step 4: items crossing flexible tracks, together, their space
    /// going to those tracks by their factors.
    fn flexible_items(
        &mut self,
        order: usize,
        at: (usize, usize),
        rows: bool,
        gap: i64,
        space: Space,
    ) -> Result<(), Error> {
        let mut group = order;
        while self
            .grid_order
            .get(group)
            .is_some_and(|(s, _)| *s != u32::MAX)
        {
            group += 1;
        }
        let stop = self.grid_order.len();
        if group == stop {
            return Ok(());
        }
        for phase in [Phase::MinBase, Phase::ContentBase, Phase::MaxBase] {
            self.phase((group, stop), at, rows, gap, (phase, space))?;
        }
        Ok(())
    }
    /// One sub-step of §12.5 for the items `grid_order[group]`: each
    /// distributes the space its contribution needs (§12.5.1), then the
    /// planned increases apply.
    fn phase(
        &mut self,
        (group, stop): (usize, usize),
        (first, offset): (usize, usize),
        rows: bool,
        gap: i64,
        (phase, space): (Phase, Space),
    ) -> Result<(), Error> {
        let max = matches!(space, Space::Max);
        let constrained = matches!(space, Space::Min | Space::Max);
        for k in group..stop {
            let (key, item) = *self.grid_order.get(k).ok_or(Error::Invalid)?;
            let item = *self.grid.get(item as usize).ok_or(Error::Invalid)?;
            let (s, e) = item.span(rows);
            let flex = key == u32::MAX;
            let range = (first + s - offset, first + e - offset);
            let tracks = self
                .grid_tracks
                .get(range.0..range.1)
                .ok_or(Error::Invalid)?;
            let automatic =
                !flex && tracks.iter().any(|t| t.min.kind == Kind::Auto) || (flex && e - s == 1);
            let contribution = match phase {
                Phase::MinBase if constrained => item.least.max(item.minimum(automatic)),
                Phase::MinBase => item.minimum(automatic),
                Phase::ContentBase | Phase::IntrinsicLimit => item.least,
                Phase::MaxBase | Phase::MaxLimit => item.most,
            };
            let base = phase.base();
            let used = tracks.iter().fold(0i64, |sum, t| {
                let size = if base || t.limit == NONE {
                    t.base
                } else {
                    t.limit
                };
                sum.saturating_add(size)
            });
            let gaps = gap.saturating_mul((e - s).saturating_sub(1) as i64);
            let room = contribution.saturating_sub(used).saturating_sub(gaps);
            self.counters.step(3 * (e - s))?;
            for track in self
                .grid_tracks
                .get_mut(range.0..range.1)
                .ok_or(Error::Invalid)?
            {
                if phase.affects(track, max) && (!flex || track.flexible()) {
                    track.touched = true;
                }
            }
            if room > 0 {
                let tracks = self
                    .grid_tracks
                    .get_mut(range.0..range.1)
                    .ok_or(Error::Invalid)?;
                let passes = if flex {
                    by_factors(tracks, room, phase, max);
                    1
                } else {
                    distribute(tracks, room, phase, max)
                };
                self.counters.step(passes * (e - s))?;
            }
            for track in self
                .grid_tracks
                .get_mut(range.0..range.1)
                .ok_or(Error::Invalid)?
            {
                track.planned = track.planned.max(track.incurred);
                track.incurred = 0;
            }
        }
        // The planned increases apply once every item of the group has
        // distributed, so that the next sub-step sees them.
        for k in group..stop {
            let (_, item) = *self.grid_order.get(k).ok_or(Error::Invalid)?;
            let (s, e) = self
                .grid
                .get(item as usize)
                .ok_or(Error::Invalid)?
                .span(rows);
            for track in self
                .grid_tracks
                .get_mut(first + s - offset..first + e - offset)
                .ok_or(Error::Invalid)?
            {
                if !track.touched {
                    continue;
                }
                track.touched = false;
                if phase.base() {
                    track.base = track.base.saturating_add(track.planned).min(NONE);
                } else {
                    let infinite = track.limit == NONE;
                    let from = if infinite { track.base } else { track.limit };
                    track.limit = from.saturating_add(track.planned).min(NONE);
                    if infinite && matches!(phase, Phase::IntrinsicLimit) {
                        track.growable = true;
                    }
                }
                track.planned = 0;
            }
        }
        Ok(())
    }
    /// §12.6 with definite free space: `free` shared equally among the
    /// base sizes, each frozen at its growth limit.
    fn fill_bases(&mut self, (first, end): (usize, usize), free: i64) -> Result<(), Error> {
        let tracks = self.grid_tracks.get_mut(first..end).ok_or(Error::Invalid)?;
        let mut left = free;
        let mut passes = 0;
        loop {
            passes += 1;
            let unfrozen = tracks.iter().filter(|t| t.limit > t.base).count() as i64;
            if unfrozen == 0 || left <= 0 {
                break;
            }
            let least = tracks
                .iter()
                .filter(|t| t.limit > t.base)
                .map(|t| t.limit - t.base)
                .min()
                .unwrap_or(0);
            if least.saturating_mul(unfrozen) <= left {
                for track in tracks.iter_mut().filter(|t| t.limit > t.base) {
                    track.base += least;
                }
                left -= least * unfrozen;
            } else {
                let mut seen = 0i64;
                for track in tracks.iter_mut().filter(|t| t.limit > t.base) {
                    let before = left * seen / unfrozen;
                    seen += 1;
                    track.base += left * seen / unfrozen - before;
                }
                break;
            }
        }
        self.counters.step(passes * (end - first))
    }
    /// §12.7.1 with definite free space: the fr size leftover space gives,
    /// flexible tracks whose base exceeds their share inflexible, each
    /// share the step between floors of the cumulative ones.
    fn expand_definite(
        &mut self,
        (first, end): (usize, usize),
        room: i64,
        gap: i64,
    ) -> Result<(), Error> {
        let open = self
            .grid_tracks
            .get(first..end)
            .ok_or(Error::Invalid)?
            .iter()
            .filter(|t| !t.collapsed)
            .count() as i64;
        let gaps = gap.saturating_mul((open - 1).max(0));
        for track in self.grid_tracks.get_mut(first..end).ok_or(Error::Invalid)? {
            track.frozen = !track.flexible();
        }
        loop {
            self.counters.step(end - first)?;
            let tracks = self.grid_tracks.get_mut(first..end).ok_or(Error::Invalid)?;
            let fixed = tracks
                .iter()
                .filter(|t| t.frozen)
                .fold(0i64, |sum, t| sum.saturating_add(t.base));
            let leftover = room.saturating_sub(gaps).saturating_sub(fixed);
            let factors: i128 = tracks
                .iter()
                .filter(|t| !t.frozen)
                .map(|t| i128::from(t.max.value))
                .sum();
            if leftover <= 0 || factors == 0 {
                return Ok(());
            }
            let total = factors.max(i128::from(ONE));
            let mut changed = false;
            for track in tracks.iter_mut().filter(|t| !t.frozen) {
                if i128::from(track.base) * total
                    > i128::from(leftover) * i128::from(track.max.value)
                {
                    track.frozen = true;
                    changed = true;
                }
            }
            if changed {
                continue;
            }
            let mut before = 0i128;
            for track in tracks.iter_mut().filter(|t| !t.frozen) {
                let low = i128::from(leftover) * before / total;
                before += i128::from(track.max.value);
                let share = (i128::from(leftover) * before / total - low) as i64;
                track.base = track.base.max(share);
            }
            return Ok(());
        }
    }
    /// §12.7 with indefinite free space: the largest fr size any flexible
    /// track's base or any item crossing one asks for, as a ratio.
    fn expand_indefinite(
        &mut self,
        (first, end): (usize, usize),
        items: (usize, usize),
        (rows, offset): (bool, usize),
        gap: i64,
    ) -> Result<(), Error> {
        self.counters.step(end - first)?;
        // An fr size of p/q: 1/64 px per ONE of factor.
        let mut fr = (0i128, 1i128);
        let larger = |a: (i128, i128), b: (i128, i128)| a.0 * b.1 > b.0 * a.1;
        for track in self.grid_tracks.get(first..end).ok_or(Error::Invalid)? {
            if !track.flexible() {
                continue;
            }
            let f = i128::from(track.max.value);
            let candidate = if f > i128::from(ONE) {
                (i128::from(track.base) * i128::from(ONE), f)
            } else {
                (i128::from(track.base), 1)
            };
            if larger(candidate, fr) {
                fr = candidate;
            }
        }
        for k in items.0..items.1 {
            let item = *self.grid.get(k).ok_or(Error::Invalid)?;
            let (s, e) = item.span(rows);
            if s < offset || e > offset + (end - first) {
                continue;
            }
            let range = (first + s - offset, first + e - offset);
            if !self
                .grid_tracks
                .get(range.0..range.1)
                .ok_or(Error::Invalid)?
                .iter()
                .any(Track::flexible)
            {
                continue;
            }
            for track in self
                .grid_tracks
                .get_mut(range.0..range.1)
                .ok_or(Error::Invalid)?
            {
                track.frozen = !track.flexible();
            }
            let gaps = gap.saturating_mul((e - s).saturating_sub(1) as i64);
            loop {
                self.counters.step(e - s)?;
                let tracks = self
                    .grid_tracks
                    .get_mut(range.0..range.1)
                    .ok_or(Error::Invalid)?;
                let fixed = tracks
                    .iter()
                    .filter(|t| t.frozen)
                    .fold(0i64, |sum, t| sum.saturating_add(t.base));
                let leftover = item.most.saturating_sub(gaps).saturating_sub(fixed);
                let factors: i128 = tracks
                    .iter()
                    .filter(|t| !t.frozen)
                    .map(|t| i128::from(t.max.value))
                    .sum();
                if leftover <= 0 || factors == 0 {
                    break;
                }
                let total = factors.max(i128::from(ONE));
                let mut changed = false;
                for track in tracks.iter_mut().filter(|t| !t.frozen) {
                    if i128::from(track.base) * total
                        > i128::from(leftover) * i128::from(track.max.value)
                    {
                        track.frozen = true;
                        changed = true;
                    }
                }
                if !changed {
                    let candidate = (i128::from(leftover) * i128::from(ONE), total);
                    if larger(candidate, fr) {
                        fr = candidate;
                    }
                    break;
                }
            }
        }
        for track in self.grid_tracks.get_mut(first..end).ok_or(Error::Invalid)? {
            if track.flexible() && fr.1 > 0 {
                let size = fr.0 * i128::from(track.max.value) / (fr.1 * i128::from(ONE));
                track.base = track.base.max(size.min(i128::from(NONE)) as i64);
            }
        }
        Ok(())
    }
    /// Each track's offset from the content edge, `gap` between those not
    /// collapsed, `align` placing them in what `room` leaves (§10.5,
    /// Box Alignment 3 §5.3), never before the start where `safe`.
    fn offsets(
        &mut self,
        (first, end): (usize, usize),
        gap: i64,
        (room, value, safe): (Option<i64>, &str, bool),
    ) -> Result<(), Error> {
        let used = self.span_extent((first, end), gap)?;
        let tracks = self.grid_tracks.get_mut(first..end).ok_or(Error::Invalid)?;
        let open = tracks.iter().filter(|t| !t.collapsed).count() as i64;
        let free = room.map_or(0, |room| room.saturating_sub(used));
        let free = if safe { free.max(0) } else { free };
        let value = match value {
            "left" => "start",
            other => other,
        };
        let (mut cursor, mut k) = (0i64, 0i64);
        for track in tracks.iter_mut() {
            if track.collapsed {
                track.start = cursor;
                continue;
            }
            if k > 0 {
                cursor = cursor.saturating_add(gap);
            }
            track.start = cursor.saturating_add(justify(value, free, k, open));
            cursor = cursor.saturating_add(track.base);
            k += 1;
        }
        Ok(())
    }
    /// Lays the grid container `id`'s items in its content box, at `x`, `y`
    /// with `offset` and `dims` its own, as `block` lays a block's
    /// children; returns its content height, its rows and their gaps.
    /// A block's content on a page with grid containers: a grid's, its
    /// used and own heights, else its children's flow at `place`.
    #[inline(never)]
    pub(super) fn grid_or_children(
        &mut self,
        b: &BoxNode,
        dims: &Dimensions,
        place: Place,
        depth: usize,
        inside: Option<Marker>,
    ) -> Result<(f64, Option<f64>), Error> {
        let (Some(id), true) = (place.block, self.grid_container(b)?) else {
            return Ok((
                self.children(b.first_child, None, place, depth, inside)?,
                None,
            ));
        };
        let at = (place.container.x, place.container.y, place.offset);
        let (used, own) = self.grid_content(id, b, (dims, place.definite), at, depth)?;
        Ok((used, Some(own)))
    }
    #[cold]
    #[inline(never)]
    pub(super) fn grid_content(
        &mut self,
        id: BoxId,
        b: &BoxNode,
        (dims, basis): (&Dimensions, Option<f64>),
        (x, y, offset): (f64, f64, (f64, f64)),
        depth: usize,
    ) -> Result<(f64, f64), Error> {
        if depth > self.counters.limits.depth {
            return Err(Error::Limit("MAX_DEPTH"));
        }
        let laid = Dimensions {
            height: basis,
            ..*dims
        };
        let dims = &laid;
        let place = Place {
            container: Rect {
                x,
                y,
                width: dims.width,
                height: dims.height.unwrap_or(0.0),
            },
            definite: dims.height,
            offset,
            strut: b.style,
            block: Some(id),
        };
        let saved = (
            self.grid.len(),
            self.grid_tracks.len(),
            self.grid_tree.len(),
            self.flex_marks.len(),
        );
        let (last_baseline, retain) = (self.last_baseline, self.retain);
        // Its first baseline, its first item's in grid order (§10.8), is
        // wanted by an inline-grid's line, or as its first line by a cell
        // or a flex item sharing its line's baseline: that item's first
        // line is watched for and kept whatever the window, and no other
        // takes the first line.
        let wanted = std::mem::replace(&mut self.flex_wanted, false);
        let inherited = std::mem::replace(&mut self.first_line, false);
        let watch = self.flex_watch.take();
        let lead = self.flex_lead.take();
        let laid = self.lay_grid(id, b, dims, &place, wanted || inherited, depth);
        // A grid's lines, as a table's, are no inline-block's last.
        self.last_baseline = last_baseline;
        self.retain = retain;
        self.flex_watch = watch;
        self.flex_lead = lead;
        self.grid.truncate(saved.0);
        self.grid_tracks.truncate(saved.1);
        self.grid_tree.truncate(saved.2);
        self.flex_marks.truncate(saved.3);
        let (used, own, first, baseline) = laid?;
        // Its first item's first line, else its border box: a grid with
        // no item takes none, and the first line is what follows.
        match first {
            Some(first) if inherited => self.first_index = Some(first),
            _ => self.first_line = inherited,
        }
        if wanted {
            self.flex_first = baseline.map(|y| y - offset.1);
        }
        Ok((used, own))
    }
    fn lay_grid(
        &mut self,
        id: BoxId,
        b: &BoxNode,
        dims: &Dimensions,
        place: &Place,
        watching: bool,
        depth: usize,
    ) -> Result<(f64, f64, Option<First>, Option<f64>), Error> {
        let styled = self.styled;
        let style = styled.style(b.style).ok_or(Error::Invalid)?;
        let width = place.container.width;
        let room = down(width);
        let col_gap =
            length(style, Property::ColumnGap, Some(width)).map_or(0, |g| units(g.max(0.0)));
        let row_gap =
            length(style, Property::RowGap, place.definite).map_or(0, |g| units(g.max(0.0)));
        if super::flex::percentage(style, Property::RowGap) {
            self.flex_wants |= HEIGHT | PERCENT;
        }
        let bounded = |v: f64| (v.is_finite() && v > 0.0).then(|| units(v));
        let row_fill = match place.definite {
            Some(height) => Fill::Most(units(height)),
            None => match (bounded(dims.max_height), bounded(dims.min_height)) {
                (Some(most), _) => Fill::Most(most),
                (None, Some(least)) => Fill::Least(least),
                _ => Fill::Once,
            },
        };
        let grid = self.build(
            b,
            (Fill::Most(room), row_fill),
            (Some(width), place.definite),
            (col_gap, row_gap),
        )?;
        // Columns first: the container's width is definite.
        let justified = keyword(style, Property::JustifyContent);
        let stretch = matches!(justified, "normal" | "stretch").then_some(room);
        self.measure_items(grid, b.style, Space::Definite(room), depth)?;
        self.size_tracks(grid, false, col_gap, Space::Definite(room), stretch)?;
        self.offsets(grid.cols, col_gap, (Some(room), justified, false))?;
        // Rows: known before laying where every one is fixed; else laid
        // in bands, each sized once its items are laid.
        let content = keyword(style, Property::AlignContent);
        let tracks = self
            .grid_tracks
            .get(grid.rows.0..grid.rows.1)
            .ok_or(Error::Invalid)?;
        let definite = place.definite.map(units);
        let least = bounded(dims.min_height);
        let free = definite.is_none() && least.is_none() && !dims.max_height.is_finite();
        let known = tracks.iter().all(|t| {
            t.min.kind == Kind::Fixed
                && (t.max.kind == Kind::Fixed || (t.flexible() && definite.is_some()))
        });
        let flexible = tracks.iter().any(Track::flexible);
        let spread = !matches!(
            content,
            "normal" | "stretch" | "start" | "flex-start" | "baseline" | "last baseline"
        );
        let autos = tracks.iter().any(|t| t.max.kind == Kind::Auto);
        // Its content lays otherwise at another definite height where
        // that resizes or moves its rows (§12.7, §12.8, §10.5).
        let stretches = autos && matches!(content, "normal" | "stretch");
        if flexible || spread || stretches {
            self.flex_wants |= HEIGHT;
        }
        if style_percent_rows(style) {
            self.flex_wants |= HEIGHT | PERCENT;
        }
        let avail = definite.or(least);
        let stretch_rows = matches!(content, "normal" | "stretch")
            .then_some(avail)
            .flatten();
        let rows = Rows {
            content,
            gap: row_gap,
            known,
            coupled: !known && (!free || flexible),
            free,
            space: match definite {
                Some(height) => Space::Definite(height),
                // A least height above the greatest wins (CSS 2.2 §10.7).
                None => Space::Indefinite(
                    least,
                    bounded(dims.max_height).map(|most| least.map_or(most, |l| most.max(l))),
                ),
            },
            stretch: stretch_rows,
            align: avail,
        };
        let tree = if known {
            self.size_tracks(grid, true, row_gap, rows.space, rows.stretch)?;
            self.offsets(grid.rows, row_gap, (avail, content, true))?;
            0
        } else {
            self.floors(grid, free)?
        };
        let lead = if watching {
            self.grid_lead(grid)?
        } else {
            None
        };
        self.flex_watch = lead;
        let content_height = self.lay_bands(id, place, grid, rows, (tree, depth))?;
        let (first, baseline) = match lead.and_then(|k| self.grid.get(k)) {
            Some(item) => {
                let item = *item;
                let (_, area) = self.area_rows(grid, &item)?;
                let source = (item.source, item.geometry());
                let first = self.flex_lead.or(item.geometry().map(First::Border));
                (first, self.laid_baseline(source, first, Some(px(area)))?)
            }
            None => (None, None),
        };
        Ok((content_height, content_height, first, baseline))
    }
    /// The grid's first item in grid order, whose baseline is its own
    /// (§10.8): the first in row-major order of its areas' first cells,
    /// tree order among those sharing one; a unit of work an item.
    fn grid_lead(&mut self, grid: Grid) -> Result<Option<usize>, Error> {
        let (start, end) = grid.items;
        self.counters.step(end - start)?;
        let items = self.grid.get(start..end).ok_or(Error::Invalid)?;
        let mut best: Option<((usize, usize), usize)> = None;
        for (at, item) in items.iter().enumerate() {
            let cell = (item.span(true).0, item.span(false).0);
            if best.is_none_or(|(first, _)| cell < first) {
                best = Some((cell, start + at));
            }
        }
        Ok(best.map(|(_, k)| k))
    }
    /// Each row's floor as no item is laid yet, and a tree of their
    /// prefix sums, where it starts in the tree stack.
    fn floors(&mut self, grid: Grid, free: bool) -> Result<usize, Error> {
        let (first, end) = grid.rows;
        self.counters.step(2 * (end - first))?;
        let base = self.grid_tree.len();
        self.counters.admit_growth(&mut self.grid_tree)?;
        self.grid_tree.try_push(0)?;
        for at in first..end {
            let track = self.grid_tracks.get_mut(at).ok_or(Error::Invalid)?;
            track.floor = track.initial_floor(free);
            let floor = track.floor;
            self.counters.admit_growth(&mut self.grid_tree)?;
            self.grid_tree.try_push(floor)?;
        }
        // A Fenwick tree of the floors, built in place.
        let n = end - first;
        for i in 1..=n {
            let j = i + i.isolate_lowest_one();
            if j <= n {
                let value = *self.grid_tree.get(base + i).ok_or(Error::Invalid)?;
                *self.grid_tree.get_mut(base + j).ok_or(Error::Invalid)? += value;
            }
        }
        Ok(base)
    }
    /// The sum of the floors of the rows before `row`, from the tree at
    /// `base`.
    fn floor_sum(&mut self, base: usize, row: usize) -> Result<i64, Error> {
        let (mut i, mut sum, mut steps) = (row, 0i64, 0usize);
        while i > 0 {
            sum = sum.saturating_add(*self.grid_tree.get(base + i).ok_or(Error::Invalid)?);
            i &= i - 1;
            steps += 1;
        }
        self.counters.step(steps)?;
        Ok(sum)
    }
    fn floor_add(&mut self, base: usize, n: usize, row: usize, delta: i64) -> Result<(), Error> {
        let (mut i, mut steps) = (row + 1, 0usize);
        while i <= n {
            let node = self.grid_tree.get_mut(base + i).ok_or(Error::Invalid)?;
            *node = node.saturating_add(delta);
            i += i.isolate_lowest_one();
            steps += 1;
        }
        self.counters.step(steps)
    }
    /// Lays the items in tree order, each at its columns and the least
    /// top its rows may take, closing a band of rows where every item laid
    /// ends above the rows every later item starts in, and nothing couples
    /// the rows across it: the band's rows are sized from its items as
    /// laid, and its items moved down, stretched or laid again there.
    fn lay_bands(
        &mut self,
        id: BoxId,
        place: &Place,
        grid: Grid,
        rows: Rows<'_>,
        (tree, depth): (usize, usize),
    ) -> Result<f64, Error> {
        let (start, end) = grid.items;
        let row_count = grid.rows.1 - grid.rows.0;
        // The least row start after each item.
        let mut later = i32::MAX;
        self.counters.step(end - start)?;
        for k in (start..end).rev() {
            let item = self.grid.get_mut(k).ok_or(Error::Invalid)?;
            item.later = later;
            later = later.min(item.rows.0);
        }
        let moving = self
            .grid
            .get(start..end)
            .ok_or(Error::Invalid)?
            .iter()
            .any(|item| {
                let [top, _, bottom, _] = item.autos;
                matches!(item.align, Align::Center | Align::End) || top || bottom
            });
        let retained = !rows.known || moving;
        let outermost = retained && self.retain == 0;
        if retained {
            if outermost {
                self.filtered = (self.lines.len(), self.markers.len());
            }
            self.retain = self.retain.checked_add(1).ok_or(Error::Invalid)?;
        }
        let (mut band, mut band_row, mut reach) = (start, 0usize, 0usize);
        let mut marks = self.flex_marks.len();
        let mut settled = false;
        for k in start..end {
            let item = *self.grid.get(k).ok_or(Error::Invalid)?;
            let (r0, r1) = item.span(true);
            let (c0, c1) = item.span(false);
            let col = |at: usize| self.grid_tracks.get(grid.cols.0 + at).copied();
            let (first_col, last_col) = (
                col(c0).ok_or(Error::Invalid)?,
                col(c1 - 1).ok_or(Error::Invalid)?,
            );
            let x = first_col.start;
            let w = last_col
                .start
                .saturating_add(last_col.base)
                .saturating_sub(x)
                .max(0);
            let (top, height) = if rows.known {
                let first = self
                    .grid_tracks
                    .get(grid.rows.0 + r0)
                    .ok_or(Error::Invalid)?;
                let last = self
                    .grid_tracks
                    .get(grid.rows.0 + r1 - 1)
                    .ok_or(Error::Invalid)?;
                let area = last
                    .start
                    .saturating_add(last.base)
                    .saturating_sub(first.start);
                (first.start, Some(area.max(0)))
            } else {
                let above = self.floor_sum(tree, r0)?;
                (
                    above.saturating_add(rows.gap.saturating_mul(r0 as i64)),
                    None,
                )
            };
            let style_percent = self.item_percent(&item)?;
            let mark = if !rows.known
                && self.relaying < MAX_RELAY
                && !settled
                && (item.stretch_candidate() || style_percent)
            {
                self.mark(k)
            } else {
                None
            };
            self.lay_area(k, id, place, (x, w), (top, height), depth)?;
            let item = *self.grid.get(k).ok_or(Error::Invalid)?;
            if let Some(mark) = mark {
                let certain = item.percent || (item.stretch && item.wants & PERCENT != 0);
                if certain || (item.stretch && item.wants != 0) {
                    self.counters.admit_growth(&mut self.flex_marks)?;
                    self.flex_marks.try_push(mark)?;
                    settled = certain;
                }
            }
            if !rows.known {
                self.row_contribution(k)?;
                if r1 - r0 == 1 {
                    let item = *self.grid.get(k).ok_or(Error::Invalid)?;
                    let track = self
                        .grid_tracks
                        .get_mut(grid.rows.0 + r0)
                        .ok_or(Error::Invalid)?;
                    let raised = raised_floor(track, &item, rows.free);
                    let delta = raised - track.floor;
                    if delta > 0 {
                        track.floor = raised;
                        self.floor_add(tree, row_count, r0, delta)?;
                    }
                }
            }
            reach = reach.max(r1);
            if !rows.coupled && reach as i64 <= i64::from(item.later) {
                let span = ((band, k + 1), (band_row, reach));
                self.close_band(id, place, grid, rows, span, (marks, tree, depth))?;
                if outermost {
                    self.keep_window()?;
                }
                band = k + 1;
                band_row = reach;
                marks = self.flex_marks.len();
                settled = false;
            }
        }
        let span = ((band, end), (band_row, row_count));
        self.close_band(id, place, grid, rows, span, (marks, tree, depth))?;
        if retained {
            self.retain -= 1;
            if outermost {
                self.keep_window()?;
            }
        }
        let used = self.span_extent(grid.rows, rows.gap)?;
        let last = self
            .grid_tracks
            .get(grid.rows.0..grid.rows.1)
            .ok_or(Error::Invalid)?;
        let reach = last
            .iter()
            .filter(|t| !t.collapsed)
            .map(|t| t.start.saturating_add(t.base))
            .max()
            .unwrap_or(0);
        let height = px(used.max(reach).max(0));
        if !coordinate(height) {
            return Err(Error::Limit("MAX_COORDINATE"));
        }
        Ok(height)
    }
    /// Whether an item's own height or its bounds are a percentage, of
    /// its area's height once that is known.
    fn item_percent(&self, item: &GridItem) -> Result<bool, Error> {
        let Source::Element(child) = item.source else {
            return Ok(false);
        };
        let c = self.styled.box_node(child).ok_or(Error::Invalid)?;
        let style = self.styled.style(c.style).ok_or(Error::Invalid)?;
        Ok(percent_height(style))
    }
    /// The rows `rows` of a band of items `items` closed: sized from what
    /// those items laid, unless they were known, offset from the band's
    /// first, and each item moved down to its area, stretched or aligned
    /// there, or laid again from the first whose content its area's
    /// height changes.
    fn close_band(
        &mut self,
        id: BoxId,
        place: &Place,
        grid: Grid,
        rows: Rows<'_>,
        ((start, end), (r0, r1)): ((usize, usize), (usize, usize)),
        (marks, tree, depth): (usize, usize, usize),
    ) -> Result<(), Error> {
        let row_count = grid.rows.1 - grid.rows.0;
        if !rows.known && r1 > r0 {
            let (space, stretch) = if rows.coupled {
                (rows.space, rows.stretch)
            } else {
                (Space::Indefinite(None, None), None)
            };
            let range = (grid.rows.0 + r0, grid.rows.0 + r1);
            self.size_range(range, (start, end), (true, r0), rows.gap, space, stretch)?;
            if rows.coupled {
                self.offsets(range, rows.gap, (rows.align, rows.content, true))?;
            } else {
                let top = match r0.checked_sub(1) {
                    Some(above) => {
                        let t = self
                            .grid_tracks
                            .get(grid.rows.0 + above)
                            .ok_or(Error::Invalid)?;
                        t.start.saturating_add(t.base).saturating_add(rows.gap)
                    }
                    None => 0,
                };
                self.offsets(range, rows.gap, (None, "start", true))?;
                for track in self
                    .grid_tracks
                    .get_mut(range.0..range.1)
                    .ok_or(Error::Invalid)?
                {
                    track.start = track.start.saturating_add(top);
                }
            }
            for r in r0..r1 {
                let track = self
                    .grid_tracks
                    .get_mut(grid.rows.0 + r)
                    .ok_or(Error::Invalid)?;
                let delta = track.base.saturating_sub(track.floor);
                track.floor = track.base;
                if delta != 0 {
                    self.floor_add(tree, row_count, r, delta)?;
                }
            }
        }
        // The first item its area's height lays otherwise.
        let mut relaid = None;
        for at in marks..self.flex_marks.len() {
            let mark = *self.flex_marks.get(at).ok_or(Error::Invalid)?;
            let item = *self.grid.get(mark.at as usize).ok_or(Error::Invalid)?;
            let (_, area) = self.area_rows(grid, &item)?;
            if item.percent || (item.stretch && item.wants & PERCENT != 0) {
                relaid = Some(mark);
                break;
            }
            if let Some(geometry) = item.geometry().and_then(|g| self.boxes.get(g)) {
                let content = stretched(&item, geometry, px(area));
                if units(content) != units(geometry.content.height) {
                    relaid = Some(mark);
                    break;
                }
            }
        }
        let until = relaid.map_or(end, |mark| mark.at as usize);
        for k in start..until {
            self.settle_item(grid, k)?;
        }
        self.flex_marks.truncate(marks);
        if let Some(mark) = relaid {
            self.restore(&mark)?;
            self.relaying += 1;
            let laid = self.relay(id, place, grid, (until, end), depth);
            self.relaying -= 1;
            laid?;
        }
        Ok(())
    }
    /// Lays the items `range` again at their areas, as sized.
    fn relay(
        &mut self,
        id: BoxId,
        place: &Place,
        grid: Grid,
        (start, end): (usize, usize),
        depth: usize,
    ) -> Result<(), Error> {
        for k in start..end {
            let item = *self.grid.get(k).ok_or(Error::Invalid)?;
            let (top, area) = self.area_rows(grid, &item)?;
            let (c0, c1) = item.span(false);
            let first = *self
                .grid_tracks
                .get(grid.cols.0 + c0)
                .ok_or(Error::Invalid)?;
            let last = *self
                .grid_tracks
                .get(grid.cols.0 + c1 - 1)
                .ok_or(Error::Invalid)?;
            let w = last
                .start
                .saturating_add(last.base)
                .saturating_sub(first.start)
                .max(0);
            self.lay_area(k, id, place, (first.start, w), (top, Some(area)), depth)?;
            self.settle_item(grid, k)?;
        }
        Ok(())
    }
    /// The top of an item's area and its height, as its rows are sized.
    fn area_rows(&self, grid: Grid, item: &GridItem) -> Result<(i64, i64), Error> {
        let (r0, r1) = item.span(true);
        let first = self
            .grid_tracks
            .get(grid.rows.0 + r0)
            .ok_or(Error::Invalid)?;
        let last = self
            .grid_tracks
            .get(grid.rows.0 + r1 - 1)
            .ok_or(Error::Invalid)?;
        let area = last
            .start
            .saturating_add(last.base)
            .saturating_sub(first.start);
        Ok((first.start, area.max(0)))
    }
    /// Moves an item down from where it was laid to its area, stretching
    /// it to the area's height or aligning it there, never up (§11.5).
    fn settle_item(&mut self, grid: Grid, k: usize) -> Result<(), Error> {
        let item = *self.grid.get(k).ok_or(Error::Invalid)?;
        let (top, area) = self.area_rows(grid, &item)?;
        let down = top.saturating_sub(item.top).max(0);
        let area = px(area);
        let free = area - item.outer;
        let [auto_top, _, auto_bottom, _] = item.autos;
        let along = if item.stretch {
            0.0
        } else if auto_top || auto_bottom {
            let free = free.max(0.0);
            let above = match (auto_top, auto_bottom) {
                (true, true) => free / 2.0,
                (true, false) => free,
                _ => 0.0,
            };
            if let Some(at) = item.geometry() {
                let geometry = self.boxes.get_mut(at).ok_or(Error::Invalid)?;
                if auto_top {
                    geometry.margin.top = above;
                }
                if auto_bottom {
                    geometry.margin.bottom = free - above;
                }
            }
            above
        } else {
            match item.align {
                Align::Center => free / 2.0,
                Align::End => free,
                _ => 0.0,
            }
        };
        let shift = px(down.saturating_add(units(along.max(0.0))));
        if shift != 0.0 {
            let range = |(start, end): (u32, u32)| (start as usize, end as usize);
            let laid = (range(item.lines), range(item.boxes), range(item.markers));
            self.move_down(laid, shift)?;
        }
        if item.stretch && !item.at_height {
            if let Some(at) = item.geometry() {
                let geometry = self.boxes.get_mut(at).ok_or(Error::Invalid)?;
                let content = stretched(&item, geometry, area);
                let decoration = geometry.padding.plus(geometry.border_widths).vertical();
                geometry.content.height = content;
                geometry.border.height = content + decoration;
            }
        }
        if let Some(at) = item.geometry() {
            let border = self.boxes.get(at).ok_or(Error::Invalid)?.border;
            self.bounds(border)?;
        }
        if let Some(rect) = item.bounds {
            self.bounds(Rect {
                y: rect.y + shift,
                ..rect
            })?;
        }
        Ok(())
    }
    /// An item's contributions to its rows' sizes once laid (§6.6): its
    /// margin box's height, and its minimum contribution.
    fn row_contribution(&mut self, k: usize) -> Result<(), Error> {
        let item = *self.grid.get(k).ok_or(Error::Invalid)?;
        let outer = holding(item.outer.max(0.0));
        let edges = match item.geometry().and_then(|g| self.boxes.get(g)) {
            Some(g) => holding(g.padding.plus(g.border_widths).vertical() + g.margin.vertical()),
            None => 0,
        };
        let (floor, bare) = match item.source {
            Source::Run(..) => (outer, 0),
            Source::Element(child) => {
                let c = self.styled.box_node(child).ok_or(Error::Invalid)?;
                let style = self.styled.style(c.style).ok_or(Error::Invalid)?;
                let height = matches!(
                    style.get(Property::Height),
                    Some(ComputedValue::Length(Length::Px(_)))
                );
                let low = match style.get(Property::MinHeight) {
                    Some(ComputedValue::Length(Length::Px(_))) => Some(item.min_height),
                    _ => None,
                };
                if c.kind == BoxKind::Image || item.geometry().is_none() || height {
                    (outer, outer)
                } else if let Some(low) = low {
                    let low = holding(low.max(0.0)).saturating_add(edges);
                    (low, low)
                } else if !matches!(keyword(style, Property::OverflowY), "visible" | "clip") {
                    (edges, edges)
                } else {
                    (outer, edges)
                }
            }
        };
        let item = self.grid.get_mut(k).ok_or(Error::Invalid)?;
        (item.least, item.most, item.floor, item.bare) = (outer, outer, floor, bare);
        Ok(())
    }
    /// Lays the item `k` in its area: `x` and `width` its columns', in
    /// 1/64 px from the content edge, at `top`, `height` the area's where
    /// its rows are sized; keeps its own bounds and what it laid.
    fn lay_area(
        &mut self,
        k: usize,
        id: BoxId,
        place: &Place,
        (x, width): (i64, i64),
        (top, height): (i64, Option<i64>),
        depth: usize,
    ) -> Result<(), Error> {
        let mut item = *self.grid.get(k).ok_or(Error::Invalid)?;
        let (lines, markers, boxes) = (self.lines.len(), self.markers.len(), self.boxes.len());
        let outer = (self.left, self.top, self.right, self.bottom);
        (self.left, self.top, self.right, self.bottom) = (
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        );
        let area = Place {
            container: Rect {
                x: place.container.x + px(x),
                y: place.container.y + px(top),
                width: px(width),
                height: height.map_or(0.0, px),
            },
            definite: height.map(px),
            ..*place
        };
        if !coordinate(area.container.y) {
            return Err(Error::Limit("MAX_COORDINATE"));
        }
        item.top = top;
        // The item whose baseline is its grid's keeps its first line
        // whatever the window, as a cell's first is kept.
        let watched = self.flex_watch == Some(k);
        let watch = watched.then(|| {
            self.flex_watch = None;
            (
                std::mem::replace(&mut self.first_line, true),
                self.first_index.take(),
            )
        });
        let laid = self.lay_in_area(&mut item, id, &area, depth);
        if let Some((first_line, first_index)) = watch {
            self.flex_lead = std::mem::replace(&mut self.first_index, first_index);
            self.first_line = first_line;
        }
        let own = if item.stretch && !item.at_height && item.geometry().is_some() {
            // Its box is stretched once its rows are sized: what it holds.
            self.inner
        } else {
            (self.left, self.top, self.right, self.bottom)
        };
        (self.left, self.top, self.right, self.bottom) = outer;
        laid?;
        item.bounds = (own.0 <= own.2).then_some(Rect {
            x: own.0,
            y: own.1,
            width: own.2 - own.0,
            height: own.3 - own.1,
        });
        item.lines = (index(lines)?, index(self.lines.len())?);
        item.markers = (index(markers)?, index(self.markers.len())?);
        item.boxes = (index(boxes)?, index(self.boxes.len())?);
        if !coordinate(item.outer) {
            return Err(Error::Limit("MAX_COORDINATE"));
        }
        *self.grid.get_mut(k).ok_or(Error::Invalid)? = item;
        Ok(())
    }
    /// Lays an item in its area `area`: a stretched block its width less
    /// its margins, any other its own width or shrink-to-fit, placed
    /// across by its auto margins or `justify-self`; a stretched one at
    /// the area's height where that is known (§11.3, §11.4).
    fn lay_in_area(
        &mut self,
        item: &mut GridItem,
        id: BoxId,
        area: &Place,
        depth: usize,
    ) -> Result<(), Error> {
        let styled = self.styled;
        let width = area.container.width;
        let top = area.container.y;
        // Unsafe, as browsers align an item across (Box Alignment 3 §4.4):
        // one wider than its area is centred or ended past its start.
        let across = |align: Align, free: f64| match align {
            Align::Center => free / 2.0,
            Align::End => free,
            _ => 0.0,
        };
        match item.source {
            Source::Run(first, until) => {
                let room = if item.justify == Align::Stretch {
                    width
                } else {
                    let (least, most) =
                        self.content_with(Some(first), until, area.strut, None, depth + 1)?;
                    px(most).min(px(least).max(width))
                };
                let x = area.container.x + px(units(across(item.justify, width - room)));
                item.outer = self.children(
                    Some(first),
                    until,
                    Place {
                        container: Rect {
                            x,
                            y: top,
                            width: room,
                            height: 0.0,
                        },
                        definite: None,
                        block: Some(id),
                        ..*area
                    },
                    depth + 1,
                    None,
                )?;
                item.geometry = NO_GEOMETRY;
            }
            Source::Element(child) => {
                let c = *styled.box_node(child).ok_or(Error::Invalid)?;
                let style = styled.style(c.style).ok_or(Error::Invalid)?;
                let side = |p| length(style, p, Some(width)).is_none();
                item.autos = [
                    side(Property::MarginTop),
                    side(Property::MarginRight),
                    side(Property::MarginBottom),
                    side(Property::MarginLeft),
                ];
                item.percent = percent_height(style);
                let [auto_top, auto_right, auto_bottom, auto_left] = item.autos;
                let table = self.role(&c)? == Some(TableRole::Table);
                let picture = c.kind == BoxKind::Image;
                let auto_height = matches!(
                    style.get(Property::Height),
                    Some(ComputedValue::Length(Length::Auto)) | None
                );
                item.stretch = item.align == Align::Stretch
                    && !table
                    && !picture
                    && auto_height
                    && !auto_top
                    && !auto_bottom;
                if table {
                    let dims = self.block_dimensions(&c, width, area.definite)?;
                    let share = match item.justify {
                        Align::Center => 0.5,
                        Align::End => 1.0,
                        _ => 0.0,
                    };
                    let laid = (child, dims, share);
                    let height = self.table_item(laid, area, top + dims.margin.top, depth + 1)?;
                    item.outer = height + dims.margin.vertical();
                    item.geometry = self.own_box(&c)?;
                    item.at_height = false;
                    if percent_height(style) {
                        self.flex_wants |= HEIGHT | PERCENT;
                    }
                    return Ok(());
                }
                let wide = item.justify == Align::Stretch
                    && !picture
                    && !auto_right
                    && !auto_left
                    && length(style, Property::Width, Some(width)).is_none();
                let mut dims = if wide {
                    self.block_dimensions(&c, width, area.definite)?
                } else {
                    self.shrink_dimensions(child, &c, area, depth)?
                };
                if auto_top {
                    dims.margin.top = 0.0;
                }
                if auto_bottom {
                    dims.margin.bottom = 0.0;
                }
                let x = if wide {
                    area.container.x
                } else {
                    let outer = dims.width
                        + dims.decoration().horizontal()
                        + dims.margin.left
                        + dims.margin.right;
                    let free = width - outer;
                    match (auto_left, auto_right) {
                        (true, true) => {
                            dims.margin.left = (free / 2.0).max(0.0);
                            dims.margin.right = (free - dims.margin.left).max(0.0);
                            area.container.x
                        }
                        (true, false) => {
                            dims.margin.left = free.max(0.0);
                            area.container.x
                        }
                        (false, true) => {
                            dims.margin.right = free.max(0.0);
                            area.container.x
                        }
                        _ => area.container.x + px(units(across(item.justify, free))),
                    }
                };
                (item.min_height, item.max_height) = (dims.min_height, dims.max_height);
                item.at_height = false;
                if let Some(height) = area.definite.filter(|_| item.stretch) {
                    let content = (height - dims.margin.vertical() - dims.decoration().vertical())
                        .min(dims.max_height)
                        .max(dims.min_height)
                        .max(0.0);
                    dims.height = Some(content);
                    item.at_height = true;
                }
                let height = self.block(
                    child,
                    c,
                    dims,
                    (x, top + dims.margin.top),
                    area.offset,
                    depth + 1,
                )?;
                item.geometry = self.own_box(&c)?;
                item.outer = height + dims.margin.vertical();
                item.wants = self.flex_depends;
            }
        }
        Ok(())
    }
    /// The box `c` laid last, its own.
    fn own_box(&self, c: &BoxNode) -> Result<u32, Error> {
        let own = self
            .boxes
            .len()
            .checked_sub(1)
            .filter(|at| self.boxes.get(*at).is_some_and(|g| g.node == c.node));
        Ok(own.map(index).transpose()?.unwrap_or(NO_GEOMETRY))
    }
}

impl GridItem {
    /// Whether it may be stretched in the block axis once its area's
    /// height is known, read before it is laid.
    fn stretch_candidate(&self) -> bool {
        self.align == Align::Stretch && matches!(self.source, Source::Element(_))
    }
}

/// The bitset auto-placement probes: `lanes` lanes of `words` words at
/// `base` of the cells stack.
struct Occupancy {
    base: usize,
    words: usize,
    lanes: usize,
}

/// The bits `low..upto` of a word.
fn mask(low: usize, upto: usize) -> u64 {
    let high = if upto >= 64 {
        u64::MAX
    } else {
        (1u64 << upto) - 1
    };
    high & !((1u64 << low.min(63)) - 1)
}

/// Whether `style` sizes a row by a percentage.
fn style_percent_rows(style: &ComputedStyle) -> bool {
    let percent =
        |b: &Breadth| matches!(b, Breadth::Length(Length::Percent(_) | Length::Calc { .. }));
    [Property::GridTemplateRows, Property::GridAutoRows]
        .into_iter()
        .any(|p| match style.get(p) {
            Some(ComputedValue::GridTracks(list)) => list.items().iter().any(|i| match i {
                TrackItem::Size(TrackSize::Breadth(b)) => percent(b),
                TrackItem::Size(TrackSize::MinMax(a, b)) => percent(a) || percent(b),
                TrackItem::Size(TrackSize::FitContent(l)) => {
                    matches!(l, Length::Percent(_) | Length::Calc { .. })
                }
                _ => false,
            }),
            _ => false,
        })
}

/// §12.5 step 2 for an item of one track not flexible: its base size
/// and growth limit from its contributions, under `constraint` where the
/// grid is measured.
fn single(track: &mut Track, item: &GridItem, constrained: bool) {
    // Under a constraint an auto minimum takes the item's min-content
    // contribution limited by a fixed maximum, floored by its minimum.
    let limited = match track.max.kind {
        Kind::Fixed | Kind::Fit => item.least.min(track.max.value),
        _ => item.least,
    };
    match track.min.kind {
        Kind::MinContent => track.base = track.base.max(item.least),
        Kind::MaxContent => track.base = track.base.max(item.most),
        Kind::Auto if constrained => track.base = track.base.max(limited.max(item.floor)),
        Kind::Auto => track.base = track.base.max(item.floor),
        _ => {}
    }
    let grow = |limit: i64, c: i64| if limit == NONE { c } else { limit.max(c) };
    match track.max.kind {
        Kind::MinContent => track.limit = grow(track.limit, item.least),
        Kind::MaxContent | Kind::Auto => track.limit = grow(track.limit, item.most),
        Kind::Fit => track.limit = grow(track.limit, item.most.min(track.max.value)),
        _ => {}
    }
    if track.limit < track.base {
        track.limit = track.base;
    }
}

/// A row's floor once an item of it alone is laid: what §12.5 step 2
/// gives its base size, and, at an auto height without bounds, its
/// growth limit, which it reaches (§12.6).
fn raised_floor(track: &Track, item: &GridItem, free: bool) -> i64 {
    let mut floor = track.floor;
    floor = match track.min.kind {
        Kind::MinContent => floor.max(item.least),
        Kind::MaxContent => floor.max(item.most),
        Kind::Auto => floor.max(item.floor),
        _ => floor,
    };
    if free && !track.flexible() {
        floor = match track.max.kind {
            Kind::MinContent => floor.max(item.least),
            Kind::MaxContent | Kind::Auto => floor.max(item.most),
            Kind::Fit => floor.max(item.most.min(track.max.value)),
            _ => floor,
        };
    }
    floor
}

/// The content height of an item laid as `geometry` stretched in an area
/// `area` tall, within its own bounds.
fn stretched(item: &GridItem, geometry: &BoxGeometry, area: f64) -> f64 {
    let decoration = geometry.padding.plus(geometry.border_widths).vertical();
    (area - geometry.margin.vertical() - decoration)
        .min(item.max_height)
        .max(item.min_height)
        .max(0.0)
}

/// A sub-step of §12.5 step 3 or 4: which size of which tracks it
/// increases, by which contribution.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Intrinsic minimums, by minimum contributions.
    MinBase,
    /// Content-based minimums, by min-content contributions.
    ContentBase,
    /// Max-content minimums, by max-content contributions.
    MaxBase,
    /// Intrinsic maximums' growth limits, by min-content contributions.
    IntrinsicLimit,
    /// Max-content maximums' growth limits, by max-content ones.
    MaxLimit,
}

impl Phase {
    fn base(self) -> bool {
        matches!(self, Self::MinBase | Self::ContentBase | Self::MaxBase)
    }
    /// Whether it increases `track`'s size, the grid measured under a
    /// max-content constraint where `max`.
    fn affects(self, track: &Track, max: bool) -> bool {
        match self {
            Self::MinBase => matches!(
                track.min.kind,
                Kind::Auto | Kind::MinContent | Kind::MaxContent
            ),
            Self::ContentBase => matches!(track.min.kind, Kind::MinContent | Kind::MaxContent),
            Self::MaxBase => {
                track.min.kind == Kind::MaxContent || (max && track.min.kind == Kind::Auto)
            }
            Self::IntrinsicLimit => track.max.intrinsic(),
            Self::MaxLimit => matches!(track.max.kind, Kind::Auto | Kind::MaxContent | Kind::Fit),
        }
    }
    /// The tracks space past every limit goes to, of those it affects.
    fn beyond(self, track: &Track) -> bool {
        match self {
            Self::MinBase | Self::ContentBase => track.max.intrinsic(),
            Self::MaxBase => matches!(track.max.kind, Kind::Auto | Kind::MaxContent),
            Self::IntrinsicLimit | Self::MaxLimit => true,
        }
    }
    fn size(self, track: &Track) -> i64 {
        if self.base() || track.limit == NONE {
            track.base
        } else {
            track.limit
        }
    }
    fn limit(self, track: &Track) -> i64 {
        if self.base() {
            track.cap()
        } else if track.limit != NONE && !track.growable {
            track.limit
        } else if track.max.kind == Kind::Fit {
            track.max.value
        } else {
            NONE
        }
    }
}

/// §12.5.1: `space` shared equally among the tracks `phase` affects,
/// each frozen at its limit, then what is left among those past it;
/// each share a step between cumulative floors. Returns the passes.
fn distribute(tracks: &mut [Track], space: i64, phase: Phase, max: bool) -> usize {
    let mut left = space;
    let mut passes = 0;
    for track in tracks.iter_mut() {
        track.frozen = !phase.affects(track, max);
    }
    loop {
        passes += 1;
        let room = |t: &Track| {
            phase
                .limit(t)
                .saturating_sub(phase.size(t).saturating_add(t.incurred))
        };
        for track in tracks.iter_mut() {
            if !track.frozen && room(track) <= 0 {
                track.frozen = true;
            }
        }
        let open = tracks.iter().filter(|t| !t.frozen).count() as i64;
        if open == 0 || left <= 0 {
            break;
        }
        let least = tracks
            .iter()
            .filter(|t| !t.frozen)
            .map(room)
            .min()
            .unwrap_or(0);
        if least.saturating_mul(open) <= left {
            for track in tracks.iter_mut().filter(|t| !t.frozen) {
                track.incurred += least;
            }
            left -= least * open;
        } else {
            share(tracks.iter_mut().filter(|t| !t.frozen), left, open);
            left = 0;
            break;
        }
    }
    if left > 0 {
        let past = |t: &Track| phase.affects(t, max) && phase.beyond(t);
        let count = tracks.iter().filter(|t| past(t)).count() as i64;
        if count > 0 {
            share(tracks.iter_mut().filter(|t| past(t)), left, count);
        } else {
            let all = tracks.iter().filter(|t| phase.affects(t, max)).count() as i64;
            share(
                tracks.iter_mut().filter(|t| phase.affects(t, max)),
                left,
                all,
            );
        }
    }
    passes
}

/// `space` among `count` tracks equally, each share a step between
/// cumulative floors.
fn share<'t>(tracks: impl Iterator<Item = &'t mut Track>, space: i64, count: i64) {
    if count <= 0 {
        return;
    }
    let mut seen = 0i64;
    for track in tracks {
        let before = (i128::from(space) * i128::from(seen) / i128::from(count)) as i64;
        seen += 1;
        let after = (i128::from(space) * i128::from(seen) / i128::from(count)) as i64;
        track.incurred += after - before;
    }
}

/// §12.5 step 4's distribution: `space` to the flexible tracks `phase`
/// affects by their factors, the part a sum under one leaves equally.
fn by_factors(tracks: &mut [Track], space: i64, phase: Phase, max: bool) {
    let target = |t: &Track| t.flexible() && phase.affects(t, max);
    let count = tracks.iter().filter(|t| target(t)).count() as i64;
    if count == 0 {
        return;
    }
    let factors: i128 = tracks
        .iter()
        .filter(|t| target(t))
        .map(|t| i128::from(t.max.value))
        .sum();
    let by = if factors >= i128::from(ONE) {
        i128::from(space)
    } else {
        i128::from(space) * factors / i128::from(ONE)
    };
    if factors > 0 {
        let mut before = 0i128;
        for track in tracks.iter_mut().filter(|t| target(t)) {
            let low = by * before / factors;
            before += i128::from(track.max.value);
            track.incurred += (by * before / factors - low) as i64;
        }
    }
    let rest = space - by as i64;
    if rest > 0 {
        share(tracks.iter_mut().filter(|t| target(t)), rest, count);
    }
}
