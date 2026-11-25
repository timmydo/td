//! CSS Flexbox 1 layout: flex items (§4), their base and hypothetical
//! sizes and automatic minimum (§9.2, §4.5), lines (§9.3), resolving
//! flexible lengths (§9.7), main-axis alignment with auto margins and
//! `justify-content` (§9.5, §8.1, §8.2), and the lines' cross sizes,
//! `align-content`, stretch and cross-axis alignment (§9.4, §8.3, §8.4),
//! in rows, wrapping rows and columns. See DESIGN.md §5, "Flex".
use super::table::{down, px, units};
use super::*;
use crate::flex_container;

/// A flex factor's fixed-point unit: factors are kept as multiples of
/// 2^-20, so every share is integer arithmetic.
const ONE: i64 = 1 << 20;
/// No maximum, in 1/64 px: MAX_COORDINATE, past which no item is laid.
/// Every size is clamped to it, so a sum over MAX_BOXES items stays under
/// 2^58, and a share's product, under 2^58 times a weight scaled under
/// 2^68, within i128.
const NONE: i64 = 64 * crate::MAX_COORDINATE as i64;

/// What a flex item is laid from.
#[derive(Clone, Copy)]
enum Source {
    /// An element child: a block, a table or a picture laid as a block.
    Element(BoxId),
    /// A run of text and breaks no element holds, from its first up to
    /// `until`, wrapped in an anonymous block (Flexbox 1 §4).
    Run(BoxId, Option<BoxId>),
}

/// Which of an item's margins are auto.
#[derive(Clone, Copy, Default)]
struct Autos {
    left: bool,
    right: bool,
    top: bool,
    bottom: bool,
}

impl Autos {
    /// Its main-axis margins' autos, at the main start and end.
    fn main(self, column: bool) -> (bool, bool) {
        if column {
            (self.top, self.bottom)
        } else {
            (self.left, self.right)
        }
    }
}

/// How a flex container is laid: a column, or a row that may wrap and
/// may run from the right.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Lay {
    column: bool,
    wrap: bool,
    reverse: bool,
}

impl Lay {
    pub(super) fn of(style: &ComputedStyle) -> Self {
        let direction = keyword(style, Property::FlexDirection);
        let column = matches!(direction, "column" | "column-reverse");
        Self {
            column,
            // A column is one line, `wrap-reverse` laid as `wrap`.
            wrap: !column && matches!(keyword(style, Property::FlexWrap), "wrap" | "wrap-reverse"),
            reverse: direction == "row-reverse",
        }
    }
}

/// What a flex item's intrinsic contribution reads of it (§9.9.3).
#[derive(Clone, Copy)]
pub(super) struct Contribution {
    /// Its flex base size: a px `flex-basis`, else its width, else, for
    /// `content` or an auto width, its max-content width.
    pub(super) basis: Basis,
    pub(super) growable: bool,
    pub(super) shrinkable: bool,
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Basis {
    Width,
    Content,
    Length(f64),
}

impl Contribution {
    fn of(style: &ComputedStyle) -> Self {
        let basis = match style.get(Property::FlexBasis) {
            Some(ComputedValue::Keyword(_)) => Basis::Content,
            Some(ComputedValue::Length(Length::Auto)) | None => Basis::Width,
            // A percentage of the width being found is auto here.
            Some(_) => length(style, Property::FlexBasis, None).map_or(Basis::Width, Basis::Length),
        };
        Self {
            basis,
            growable: factor(style, Property::FlexGrow) > 0,
            shrinkable: factor(style, Property::FlexShrink) > 0,
        }
    }
}

/// How an item is aligned across the line (`align-self`, §8.3).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cross {
    Start,
    Center,
    End,
    Stretch,
}

/// A flex item: its main sizes in 1/64 px, of its content box but for a
/// table's, whose are its border box's, and, once laid, what it laid.
#[derive(Clone, Copy)]
pub(super) struct FlexItem {
    source: Source,
    /// Its flex base size, the bounds of its main size and its target.
    base: i64,
    min: i64,
    max: i64,
    target: i64,
    /// Its automatic minimum is not yet measured; its specified width,
    /// NONE for none, bounds it.
    pending: bool,
    specified: i64,
    /// Flex factors in 1/ONE.
    grow: i64,
    shrink: i64,
    /// Its padding, border and margins but auto ones, along the main axis.
    around: i64,
    /// Auto margins: left, right, top, bottom.
    autos: Autos,
    /// Its used main-axis margins, at its main start and end: left and
    /// right in a row, top and bottom in a column.
    before: i64,
    after: i64,
    frozen: bool,
    /// The sign of its last clamp: below its minimum, above its maximum.
    violation: i8,
    /// Its margin box's main offset from the container's content edge,
    /// and, in a column, the one it was laid at.
    x: i64,
    laid: i64,
    cross: Cross,
    /// Its own min and max content heights, which a stretch keeps to.
    min_height: f64,
    max_height: f64,
    /// What it laid, ranges of the layout's.
    lines: (u32, u32),
    boxes: (u32, u32),
    markers: (u32, u32),
    /// Its own geometry, None for an anonymous item.
    geometry: Option<u32>,
    /// Its margin box's height as laid.
    outer: f64,
    bounds: Option<Rect>,
}

impl FlexItem {
    fn new(source: Source, cross: Cross) -> Self {
        Self {
            source,
            base: 0,
            min: 0,
            max: NONE,
            target: 0,
            pending: false,
            specified: NONE,
            grow: 0,
            shrink: ONE,
            around: 0,
            autos: Autos::default(),
            before: 0,
            after: 0,
            frozen: false,
            violation: 0,
            x: 0,
            laid: 0,
            cross,
            min_height: 0.0,
            max_height: f64::INFINITY,
            lines: (0, 0),
            boxes: (0, 0),
            markers: (0, 0),
            geometry: None,
            outer: 0.0,
            bounds: None,
        }
    }
    /// Its sizes within 0 to NONE.
    fn bound(&mut self) {
        for size in [
            &mut self.base,
            &mut self.min,
            &mut self.max,
            &mut self.specified,
        ] {
            *size = (*size).clamp(0, NONE);
        }
    }
    /// Its main size, clamped by its bounds, a minimum winning (§9.7).
    fn clamp(&self, size: i64) -> i64 {
        size.min(self.max).max(self.min).max(0)
    }
    /// Its hypothetical outer main size (§9.2 step 3).
    fn outer_main(&self) -> i64 {
        self.clamp(self.base).saturating_add(self.around)
    }
}

/// A flex line: its items, a range of the item stack, and its cross
/// size once laid.
#[derive(Clone, Copy)]
pub(super) struct FlexLine {
    start: u32,
    end: u32,
    height: f64,
}

/// An index into the layout's output, which fits a u32 as `MAX_LINES`,
/// `MAX_BOXES` and `MAX_FRAGMENTS` do.
fn index(at: usize) -> Result<u32, Error> {
    u32::try_from(at).map_err(|_| Error::Invalid)
}

/// A flex factor in 1/ONE, from 0 to 2^40: td-css clamps it to
/// MAX_ABSOLUTE_VALUE, under 2^20.
fn factor(style: &ComputedStyle, p: Property) -> i64 {
    match style.get(p) {
        Some(ComputedValue::Number(n)) if n.is_finite() && *n > 0.0 => {
            (n * ONE as f64).round().min((1u64 << 40) as f64) as i64
        }
        _ => 0,
    }
}

/// An item's `align-self`, `auto` taking its container's `align-items`;
/// baselines are laid as start for now.
fn cross(own: &str, container: &str) -> Cross {
    let value = if own == "auto" || own.is_empty() {
        container
    } else {
        own
    };
    match value {
        "normal" | "stretch" | "" => Cross::Stretch,
        "center" => Cross::Center,
        "end" | "flex-end" | "self-end" => Cross::End,
        _ => Cross::Start,
    }
}

/// Where the item `at` of `count` starts past the space before it, of
/// `free` laid by `justify-content` (§8.2), or the line `at` by
/// `align-content` (§8.4): every share a floor of the cumulative one, so
/// the shares sum exactly.
fn justify(value: &str, free: i64, at: i64, count: i64) -> i64 {
    let share = |part: i64, whole: i64| -> i64 {
        if whole <= 0 {
            return 0;
        }
        (i128::from(free) * i128::from(part)).div_euclid(i128::from(whole)) as i64
    };
    let center = free.div_euclid(2);
    match value {
        "end" | "flex-end" | "right" => free,
        "center" => center,
        "space-between" if free > 0 => share(at, count - 1),
        "space-around" if free > 0 => share(2 * at + 1, 2 * count),
        "space-evenly" if free > 0 => share(at + 1, count + 1),
        "space-around" | "space-evenly" => center,
        _ => 0,
    }
}

/// `justify-content` as laid from the main start: in a row from the
/// right, `start`, `end`, `left` and `right`, which keep to the writing
/// mode, swap, as the positions are mirrored after; in a column `left`
/// and `right` are `start` (Box Alignment 3 §6.1).
fn main_justify(value: &str, lay: Lay) -> &str {
    match value {
        "left" | "right" if lay.column => "start",
        "start" | "left" if lay.reverse => "end",
        "end" | "right" if lay.reverse => "start",
        other => other,
    }
}

/// 1/64 px rounded up, but for what lies within 1e-9 of a unit: a line
/// or container that holds its tallest item.
fn holding(v: f64) -> i64 {
    let whole = units(v);
    if px(whole) + 1e-9 < v {
        whole + 1
    } else {
        whole
    }
}

impl<M: FontMetrics> Engine<'_, '_, '_, '_, M> {
    /// Whether `b` is a flex container, on a page with any.
    pub(super) fn flex_container(&self, b: &BoxNode) -> Result<bool, Error> {
        if !self.styled.has_flex() {
            return Ok(false);
        }
        let style = self.styled.style(b.style).ok_or(Error::Invalid)?;
        Ok(flex_container(style, b.kind))
    }
    /// Whether `b`'s parent is a flex container, which makes `b` a flex
    /// item and a formatting context's root.
    pub(super) fn flex_item(&self, b: &BoxNode) -> Result<bool, Error> {
        match b.parent {
            Some(parent) if self.styled.has_flex() => {
                let parent = self.styled.box_node(parent).ok_or(Error::Invalid)?;
                self.flex_container(parent)
            }
            _ => Ok(false),
        }
    }
    /// The intrinsic widths of `b`'s content in 1/64 px: a row's its
    /// items', else its children's as block flow, a column's the largest
    /// of its items' contributions among them (§9.9.1).
    pub(super) fn box_content(
        &mut self,
        b: &BoxNode,
        lead: Option<(i64, bool)>,
        depth: usize,
    ) -> Result<(i64, i64), Error> {
        if self.flex_container(b)? {
            let style = self.styled.style(b.style).ok_or(Error::Invalid)?;
            let lay = Lay::of(style);
            if !lay.column {
                return self.flex_intrinsic(b, lay.wrap, depth);
            }
        }
        self.content_with(b.first_child, None, b.style, lead, depth)
    }
    /// A row's intrinsic widths from its items' contributions (§9.9.3),
    /// each with its edges: the max-content width their sum and the gaps
    /// between them; the min-content width that sum too where no line
    /// wraps, else the largest, each item then on its own line (§9.9.1).
    #[cold]
    #[inline(never)]
    fn flex_intrinsic(
        &mut self,
        b: &BoxNode,
        wraps: bool,
        depth: usize,
    ) -> Result<(i64, i64), Error> {
        if depth > self.counters.limits.depth {
            return Err(Error::Limit("MAX_DEPTH"));
        }
        let style = self.styled.style(b.style).ok_or(Error::Invalid)?;
        // A percentage of the width being found is 0 here.
        let gap = length(style, Property::ColumnGap, None).map_or(0, |g| units(g.max(0.0)));
        let (mut least, mut most, mut count) = (0i64, 0i64, 0i64);
        let mut next = b.first_child;
        let mut run: Option<BoxId> = None;
        let mut solid = false;
        loop {
            let child = match next {
                Some(id) => {
                    self.counters.step(1)?;
                    Some((id, *self.styled.box_node(id).ok_or(Error::Invalid)?))
                }
                None => None,
            };
            let element =
                child.is_some_and(|(_, c)| matches!(c.kind, BoxKind::Block | BoxKind::Image));
            if child.is_none() || element {
                if let Some(first) = run.take().filter(|_| solid) {
                    let (l, m) = self.content_with(Some(first), next, b.style, None, depth + 1)?;
                    least = if wraps {
                        least.max(l)
                    } else {
                        least.saturating_add(l)
                    };
                    most = most.saturating_add(m);
                    count += 1;
                }
                solid = false;
            }
            let Some((id, c)) = child else {
                break;
            };
            if element {
                let role = self.role(&c)?;
                let item = self.styled.style(c.style).ok_or(Error::Invalid)?;
                let flex = Contribution::of(item);
                let (l, m) = self.block_contribution(id, c, role, Some(flex), depth + 1)?;
                least = if wraps {
                    least.max(l)
                } else {
                    least.saturating_add(l)
                };
                most = most.saturating_add(m);
                count += 1;
            } else {
                run = run.or(Some(id));
                if !solid {
                    solid = !self.blank(&c)?;
                }
            }
            next = c.next_sibling;
        }
        let gaps = gap.saturating_mul((count - 1).max(0));
        let least = if wraps {
            least
        } else {
            least.saturating_add(gaps)
        };
        Ok((least.max(0), most.saturating_add(gaps).max(0)))
    }
    /// Lays the flex container `id`'s items in its content box, at `x`,
    /// `y` with `offset` and `dims` its own, as `block` lays a block's
    /// children, and returns its content height: its lines' cross sizes
    /// and the gaps between them (§9.4), or a column's main size.
    #[cold]
    #[inline(never)]
    pub(super) fn flex_content(
        &mut self,
        id: BoxId,
        b: &BoxNode,
        dims: &Dimensions,
        (x, y, offset): (f64, f64, (f64, f64)),
        depth: usize,
    ) -> Result<(f64, f64), Error> {
        if depth > self.counters.limits.depth {
            return Err(Error::Limit("MAX_DEPTH"));
        }
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
        let style = self.styled.style(b.style).ok_or(Error::Invalid)?;
        let lay = Lay::of(style);
        let (base, lines, retain) = (self.flex.len(), self.flex_lines.len(), self.retain);
        let last_baseline = self.last_baseline;
        let laid = if lay.column {
            self.flex_column(id, b, dims, place, base, depth)
        } else {
            self.flex_rows(id, b, dims, place, (base, lines), depth)
        };
        // A container's lines, as a table's, are no inline-block's last.
        self.last_baseline = last_baseline;
        self.retain = retain;
        self.flex.truncate(base);
        self.flex_lines.truncate(lines);
        laid
    }
    /// Lays a row's items on its lines, one where it does not wrap, the
    /// lines down the page `row-gap` apart (§9.3, §9.4, §8.4).
    fn flex_rows(
        &mut self,
        id: BoxId,
        b: &BoxNode,
        dims: &Dimensions,
        place: Place,
        (base, first): (usize, usize),
        depth: usize,
    ) -> Result<(f64, f64), Error> {
        let style = self.styled.style(b.style).ok_or(Error::Invalid)?;
        let lay = Lay::of(style);
        let width = place.container.width;
        let room = down(width);
        let gap = length(style, Property::ColumnGap, Some(width)).map_or(0, |g| units(g.max(0.0)));
        let row_gap = length(style, Property::RowGap, place.definite).map_or(0.0, |g| g.max(0.0));
        let row_gap = px(units(row_gap));
        let align = keyword(style, Property::AlignItems);
        self.flex_items(b, align)?;
        let end = self.flex.len();
        for at in base..end {
            self.size_item(at, &place, depth)?;
        }
        self.break_lines(base, end, room, gap, lay.wrap)?;
        let justified = main_justify(keyword(style, Property::JustifyContent), lay);
        // Decided before anything is laid: whether any item will move
        // down once its line's cross size is known, and whether
        // `align-content` may move lines laid before others, which free
        // space in a container of definite or least height can, until
        // the lines laid reach that height.
        let content = keyword(style, Property::AlignContent);
        let floor = place
            .definite
            .or((dims.min_height > 0.0).then_some(dims.min_height));
        let mut spread = lay.wrap
            && floor.is_some()
            && !matches!(
                content,
                "start" | "flex-start" | "baseline" | "last baseline"
            );
        let aligned = self
            .flex
            .get(base..end)
            .ok_or(Error::Invalid)?
            .iter()
            .any(|item| {
                matches!(item.cross, Cross::Center | Cross::End)
                    || item.autos.top
                    || item.autos.bottom
            });
        let mut retained = spread || aligned;
        let outermost = retained && self.retain == 0;
        if retained {
            if outermost {
                self.filtered = (self.lines.len(), self.markers.len());
            }
            self.retain = self.retain.checked_add(1).ok_or(Error::Invalid)?;
        }
        let mut top = 0.0f64;
        // Its lines' height before its own height applies.
        let mut extent = 0.0f64;
        for at in first..self.flex_lines.len() {
            let line = *self.flex_lines.get(at).ok_or(Error::Invalid)?;
            let (start, end) = (line.start as usize, line.end as usize);
            let gaps = gap.saturating_mul(end.saturating_sub(start + 1) as i64);
            self.resolve(start, end, room.saturating_sub(gaps), depth)?;
            self.place_items(start, end, room, gap, justified, lay)?;
            let row = Place {
                container: Rect {
                    y: place.container.y + top,
                    ..place.container
                },
                ..place
            };
            let tallest = self.lay_items(id, b.style, &row, start, end, depth)?;
            extent += px(holding(tallest.max(0.0)));
            if at + 1 < self.flex_lines.len() {
                extent += row_gap;
            }
            // One line: the container's definite height, else its items'
            // within its own min and max heights (§9.4 step 8); one of
            // several, its tallest item's (step 8).
            let height = if lay.wrap {
                tallest.max(0.0)
            } else {
                match place.definite {
                    Some(height) => height,
                    None => tallest.max(0.0).min(dims.max_height).max(dims.min_height),
                }
            };
            let height = px(holding(height));
            if !coordinate(top + height) {
                return Err(Error::Limit("MAX_COORDINATE"));
            }
            self.flex_lines.get_mut(at).ok_or(Error::Invalid)?.height = height;
            if !spread {
                self.align_items(start, end, height)?;
                // A line's items are final once aligned, as a table's row.
                if outermost && retained {
                    self.keep_window()?;
                }
            }
            top += height;
            // Lines reaching the container's height leave no free space:
            // none spreads, and those laid are aligned as they lie.
            if spread && floor.is_some_and(|floor| holding(floor) <= units(top)) {
                spread = false;
                for pending in first..=at {
                    let line = *self.flex_lines.get(pending).ok_or(Error::Invalid)?;
                    self.align_items(line.start as usize, line.end as usize, line.height)?;
                }
                if outermost {
                    self.keep_window()?;
                }
                if !aligned {
                    self.retain -= 1;
                    retained = false;
                }
            }
            if at + 1 < self.flex_lines.len() {
                top += row_gap;
            }
        }
        if !lay.wrap {
            return Ok((top, extent));
        }
        let cross = match place.definite {
            Some(height) => height,
            None => top.min(dims.max_height).max(dims.min_height),
        };
        let cross = px(holding(cross));
        if spread {
            self.spread_lines(first, units(cross - top), content)?;
            if outermost {
                self.keep_window()?;
            }
        }
        if retained {
            self.retain -= 1;
        }
        Ok((cross, extent))
    }
    /// Breaks the items `start..end` into lines (§9.3 step 5): each as
    /// many as fit its room with the gaps between them, two units an item
    /// and one more of it being their sizes' rounding to 1/64 px, so
    /// percentages summing to 100% fit at every width; at least one.
    fn break_lines(
        &mut self,
        start: usize,
        end: usize,
        room: i64,
        gap: i64,
        wrap: bool,
    ) -> Result<(), Error> {
        self.counters.step(end - start)?;
        let mut first = start;
        let mut used = 0i64;
        for at in start..end {
            let size = self.flex.get(at).ok_or(Error::Invalid)?.outer_main();
            let count = (at - first) as i64;
            if count == 0 {
                used = size;
                continue;
            }
            let next = used.saturating_add(gap).saturating_add(size);
            let slack = 2i64.saturating_mul(count + 1).saturating_add(1);
            if wrap && next > room.saturating_add(slack) {
                self.flex_line(first, at)?;
                first = at;
                used = size;
            } else {
                used = next;
            }
        }
        if first < end || !wrap {
            self.flex_line(first, end)?;
        }
        Ok(())
    }
    fn flex_line(&mut self, start: usize, end: usize) -> Result<(), Error> {
        let line = FlexLine {
            start: index(start)?,
            end: index(end)?,
            height: 0.0,
        };
        self.counters.admit_growth(&mut self.flex_lines)?;
        self.flex_lines.try_push(line)?;
        Ok(())
    }
    /// Places the lines from `first` in `free` space, the container's
    /// cross size less theirs, by `align-content` (§8.4, §9.4 step 9,
    /// §9.6 step 16):
    /// `normal` and `stretch` grow each line by a share, the rest move
    /// them; never up, as `safe` alignment. Then aligns each line's items.
    fn spread_lines(&mut self, first: usize, free: i64, content: &str) -> Result<(), Error> {
        let count = (self.flex_lines.len() - first) as i64;
        let free = free.max(0);
        let stretch = matches!(content, "normal" | "stretch");
        let mut grown = 0i64;
        for at in first..self.flex_lines.len() {
            let n = (at - first) as i64;
            let mut line = *self.flex_lines.get(at).ok_or(Error::Invalid)?;
            let (shift, extra) = if stretch {
                let before = grown;
                grown = (i128::from(free) * i128::from(n + 1) / i128::from(count)) as i64;
                (before, grown - before)
            } else {
                (justify(content, free, n, count), 0)
            };
            let (start, end) = (line.start as usize, line.end as usize);
            if shift > 0 && start < end {
                let span = |a: (u32, u32), b: (u32, u32)| (a.0 as usize, b.1 as usize);
                let first = *self.flex.get(start).ok_or(Error::Invalid)?;
                let last = *self.flex.get(end - 1).ok_or(Error::Invalid)?;
                let laid = (
                    span(first.lines, last.lines),
                    span(first.boxes, last.boxes),
                    span(first.markers, last.markers),
                );
                self.move_down(laid, px(shift))?;
                for item in self.flex.get_mut(start..end).ok_or(Error::Invalid)? {
                    if let Some(rect) = item.bounds.as_mut() {
                        rect.y += px(shift);
                    }
                }
            }
            line.height += px(extra);
            self.align_items(start, end, line.height)?;
            *self.flex_lines.get_mut(at).ok_or(Error::Invalid)? = line;
        }
        Ok(())
    }
    /// The flex items of `b`, onto the stack: each element child, and
    /// each run of other children holding more than white space (§4).
    fn flex_items(&mut self, b: &BoxNode, align: &str) -> Result<(), Error> {
        let mut next = b.first_child;
        while let Some(item) = self.next_flex_item(&mut next, align)? {
            self.counters.admit_growth(&mut self.flex)?;
            self.flex.try_push(item)?;
        }
        Ok(())
    }
    /// The flex item from `next` on, which it moves past: an element
    /// child, or a run of other children up to the next element holding
    /// more than white space; each child looked at a unit of work.
    fn next_flex_item(
        &mut self,
        next: &mut Option<BoxId>,
        align: &str,
    ) -> Result<Option<FlexItem>, Error> {
        let element = |c: &BoxNode| matches!(c.kind, BoxKind::Block | BoxKind::Image);
        while let Some(first) = *next {
            self.counters.step(1)?;
            let c = *self.styled.box_node(first).ok_or(Error::Invalid)?;
            if element(&c) {
                *next = c.next_sibling;
                let style = self.styled.style(c.style).ok_or(Error::Invalid)?;
                let item = FlexItem::new(
                    Source::Element(first),
                    cross(keyword(style, Property::AlignSelf), align),
                );
                return Ok(Some(item));
            }
            let mut solid = !self.blank(&c)?;
            let mut cursor = c.next_sibling;
            while let Some(at) = cursor {
                let c = *self.styled.box_node(at).ok_or(Error::Invalid)?;
                if element(&c) {
                    break;
                }
                self.counters.step(1)?;
                if !solid {
                    solid = !self.blank(&c)?;
                }
                cursor = c.next_sibling;
            }
            *next = cursor;
            if solid {
                let item = FlexItem::new(Source::Run(first, cursor), cross("auto", align));
                return Ok(Some(item));
            }
        }
        Ok(None)
    }
    /// An item's flex base size, the bounds of its main size and its
    /// factors (§9.2 steps 3 and 4, §4.5), measuring its content at most
    /// once, and only where its base size or its automatic minimum needs
    /// it now; a minimum only shrinking needs waits (`resolve`).
    fn size_item(&mut self, at: usize, place: &Place, depth: usize) -> Result<(), Error> {
        let mut item = *self.flex.get(at).ok_or(Error::Invalid)?;
        let width = place.container.width;
        match item.source {
            Source::Run(first, until) => {
                let strut = place.strut;
                let (least, most) =
                    self.content_with(Some(first), until, strut, None, depth + 1)?;
                // Its style is its container's: `flex: 0 1 auto`, and an
                // automatic minimum, its min-content width.
                item.base = most;
                item.min = least;
            }
            Source::Element(id) => {
                let c = *self.styled.box_node(id).ok_or(Error::Invalid)?;
                let style = self.styled.style(c.style).ok_or(Error::Invalid)?;
                item.grow = factor(style, Property::FlexGrow);
                item.shrink = factor(style, Property::FlexShrink);
                let side = |p| length(style, p, Some(width)).is_none();
                item.autos = Autos {
                    left: side(Property::MarginLeft),
                    right: side(Property::MarginRight),
                    top: side(Property::MarginTop),
                    bottom: side(Property::MarginBottom),
                };
                let dims = self.block_dimensions(&c, width, place.definite)?;
                let margin = edges(
                    style,
                    [
                        Property::MarginTop,
                        Property::MarginRight,
                        Property::MarginBottom,
                        Property::MarginLeft,
                    ],
                    width,
                    false,
                );
                (item.before, item.after) = (units(margin.left), units(margin.right));
                let table = self.role(&c)? == Some(TableRole::Table);
                let picture = c.kind == BoxKind::Image;
                let decoration = dims.decoration().horizontal();
                item.around = item.before + item.after;
                if !table {
                    item.around += units(decoration);
                }
                (item.min_height, item.max_height) = (dims.min_height, dims.max_height);
                // A stretched item is a block whose height and cross
                // margins are auto (§8.3, §9.4 step 11).
                if item.cross == Cross::Stretch
                    && (table
                        || picture
                        || item.autos.top
                        || item.autos.bottom
                        || length(style, Property::Height, place.definite).is_some())
                {
                    item.cross = Cross::Start;
                }
                let border_box = keyword(style, Property::BoxSizing) == "border-box";
                let content = |v: f64| {
                    if border_box && !table {
                        units((v - decoration).max(0.0))
                    } else {
                        units(v.max(0.0))
                    }
                };
                let basis = match style.get(Property::FlexBasis) {
                    Some(ComputedValue::Keyword(_)) => None,
                    Some(ComputedValue::Length(Length::Auto)) | None => {
                        length(style, Property::Width, Some(width)).map(content)
                    }
                    Some(_) => length(style, Property::FlexBasis, Some(width)).map(content),
                };
                let definite = !matches!(
                    style.get(Property::FlexBasis),
                    Some(ComputedValue::Keyword(_) | ComputedValue::Length(Length::Auto)) | None
                );
                let specified = length(style, Property::Width, Some(width)).map(content);
                let low = length(style, Property::MinWidth, Some(width)).map(content);
                item.max = length(style, Property::MaxWidth, Some(width)).map_or(NONE, content);
                // The automatic minimum is a box's that is no scroll
                // container (§4.5); any other's is 0.
                let automatic = low.is_none()
                    && matches!(keyword(style, Property::OverflowX), "visible" | "clip");
                if table {
                    let (least, most) =
                        self.table_intrinsic(table::Measured::Root(id), c.style, depth + 1)?;
                    item.base = basis.unwrap_or(most);
                    item.min = least;
                    item.max = NONE;
                } else if picture {
                    let size = units(dims.width);
                    item.base = basis.unwrap_or(size);
                    item.min = low.unwrap_or(if automatic { size.min(item.max) } else { 0 });
                } else if basis.is_none() || (automatic && definite) {
                    let lead = self.inside_marker(id, &c)?;
                    let (least, most) = self.box_content(&c, lead, depth + 1)?;
                    item.base = basis.unwrap_or(most);
                    item.min = match low {
                        Some(low) => low,
                        None if automatic => least.min(specified.unwrap_or(NONE)).min(item.max),
                        None => 0,
                    };
                } else {
                    item.base = basis.unwrap_or(0);
                    match low {
                        Some(low) => item.min = low,
                        // No more than the base size, its width: measured
                        // only if the line shrinks.
                        None if automatic => {
                            item.pending = true;
                            item.specified = specified.unwrap_or(NONE);
                        }
                        None => item.min = 0,
                    }
                }
            }
        }
        item.bound();
        *self.flex.get_mut(at).ok_or(Error::Invalid)? = item;
        Ok(())
    }
    /// Resolves the flexible lengths of the items `start..end` in `room`,
    /// their line's main size less the gaps (§9.7): each item's target
    /// main size.
    fn resolve(&mut self, start: usize, end: usize, room: i64, depth: usize) -> Result<(), Error> {
        let count = end - start;
        self.counters.step(count)?;
        let items = self.flex.get_mut(start..end).ok_or(Error::Invalid)?;
        let hypothetical: i64 = items
            .iter()
            .fold(0i64, |sum, item| sum.saturating_add(item.outer_main()));
        let growing = hypothetical < room;
        // Inflexible items take their hypothetical size.
        for item in items.iter_mut() {
            let factor = if growing { item.grow } else { item.shrink };
            let hypothetical = item.clamp(item.base);
            item.frozen = factor == 0
                || (growing && item.base > hypothetical)
                || (!growing && item.base < hypothetical);
            item.target = hypothetical;
        }
        // A shrinking line's flexible items need their automatic minimum.
        if !growing {
            for at in start..end {
                let item = *self.flex.get(at).ok_or(Error::Invalid)?;
                if !item.pending || item.frozen {
                    continue;
                }
                let Source::Element(id) = item.source else {
                    return Err(Error::Invalid);
                };
                let c = *self.styled.box_node(id).ok_or(Error::Invalid)?;
                let lead = self.inside_marker(id, &c)?;
                let (least, _) = self.box_content(&c, lead, depth + 1)?;
                let item = self.flex.get_mut(at).ok_or(Error::Invalid)?;
                item.min = least.min(item.specified).min(item.max);
                item.pending = false;
                item.bound();
            }
        }
        let used = |items: &[FlexItem]| -> i64 {
            items.iter().fold(0i64, |sum, item| {
                sum.saturating_add(if item.frozen { item.target } else { item.base })
                    .saturating_add(item.around)
            })
        };
        let initial = room.saturating_sub(used(self.flex.get(start..end).ok_or(Error::Invalid)?));
        loop {
            // Each pass visits every item, a unit each: a cascade of
            // freezes is charged as it goes.
            self.counters.step(3 * count)?;
            let items = self.flex.get_mut(start..end).ok_or(Error::Invalid)?;
            let factors: i128 = items
                .iter()
                .filter(|item| !item.frozen)
                .map(|item| i128::from(if growing { item.grow } else { item.shrink }))
                .sum();
            if items.iter().all(|item| item.frozen) {
                return Ok(());
            }
            let mut remaining = room.saturating_sub(used(items));
            // Each factor is within half a unit of its value, so a sum
            // within that of 1 is 1; a sum under it takes its share of
            // the initial free space, to the nearest 1/64 px.
            let unfrozen = items.iter().filter(|item| !item.frozen).count() as i128;
            if factors + unfrozen.div_euclid(2) + 1 < i128::from(ONE) {
                let product = i128::from(initial) * factors;
                let scaled = (product + product.signum() * i128::from(ONE / 2)) / i128::from(ONE);
                let scaled = scaled as i64;
                if scaled.abs() < remaining.abs() {
                    remaining = scaled;
                }
            }
            let weight = |item: &FlexItem| -> i128 {
                if growing {
                    i128::from(item.grow)
                } else {
                    i128::from(item.shrink) * i128::from(item.base)
                }
            };
            let weights: i128 = items.iter().filter(|item| !item.frozen).map(weight).sum();
            // Each share is the step between floors of the cumulative
            // shares: within 1/64 px of its exact share, and summing
            // exactly to the free space, rounding's remainder going to
            // the items it fell short of, the last one's included.
            // The weights are scaled down to fit, past 2^68 in all.
            let free = i128::from(remaining.abs());
            let scale = (128 - weights.leading_zeros()).saturating_sub(68);
            let whole = weights >> scale;
            let mut before = 0i128;
            for item in items.iter_mut().filter(|item| !item.frozen) {
                let share = if whole <= 0 {
                    0
                } else {
                    let low = free * (before >> scale) / whole;
                    before += weight(item);
                    (free * (before >> scale) / whole - low) as i64
                };
                item.target = if growing {
                    item.base.saturating_add(remaining.signum() * share)
                } else {
                    item.base.saturating_sub(share)
                };
            }
            let mut total = 0i64;
            for item in items.iter_mut().filter(|item| !item.frozen) {
                let clamped = item.clamp(item.target);
                let violation = clamped - item.target;
                item.violation = violation.signum() as i8;
                total = total.saturating_add(violation);
                item.target = clamped;
            }
            for item in items.iter_mut().filter(|item| !item.frozen) {
                item.frozen = total == 0
                    || (total > 0 && item.violation > 0)
                    || (total < 0 && item.violation < 0);
            }
        }
    }
    /// Each item's main offset and used main margins: auto margins share
    /// the positive free space, else `justify-content` places the items
    /// in it, or past it where it is negative (§8.1, §8.2, §9.5): a
    /// column's items never move up for it (`settle`). A row from the
    /// right has its offsets mirrored.
    fn place_items(
        &mut self,
        start: usize,
        end: usize,
        room: i64,
        gap: i64,
        justified: &str,
        lay: Lay,
    ) -> Result<(), Error> {
        let items = self.flex.get_mut(start..end).ok_or(Error::Invalid)?;
        let count = items.len() as i64;
        let used = items.iter().fold(0i64, |sum, item| {
            sum.saturating_add(item.target).saturating_add(item.around)
        });
        let gaps = gap.saturating_mul((count - 1).max(0));
        let free = room.saturating_sub(used).saturating_sub(gaps);
        let main = |item: &FlexItem| item.autos.main(lay.column);
        let autos = items
            .iter()
            .map(|item| i64::from(main(item).0) + i64::from(main(item).1))
            .sum::<i64>();
        let mut cursor = 0i64;
        if free > 0 && autos > 0 {
            let mut seen = 0i64;
            let share = |seen: &mut i64| -> i64 {
                let before = (i128::from(free) * i128::from(*seen) / i128::from(autos)) as i64;
                *seen += 1;
                let after = (i128::from(free) * i128::from(*seen) / i128::from(autos)) as i64;
                after - before
            };
            for item in items.iter_mut() {
                let (before, after) = main(item);
                if before {
                    item.before = share(&mut seen);
                }
                if after {
                    item.after = share(&mut seen);
                }
                item.x = cursor;
                cursor = cursor
                    .saturating_add(item.target + item.around + item.before * i64::from(before))
                    .saturating_add(item.after * i64::from(after))
                    .saturating_add(gap);
            }
        } else {
            for (at, item) in items.iter_mut().enumerate() {
                let (before, after) = main(item);
                if before {
                    item.before = 0;
                }
                if after {
                    item.after = 0;
                }
                item.x = cursor.saturating_add(justify(justified, free, at as i64, count));
                cursor = cursor
                    .saturating_add(item.target)
                    .saturating_add(item.around)
                    .saturating_add(gap);
            }
        }
        if lay.reverse {
            for item in items.iter_mut() {
                let (before, after) = main(item);
                let outer = item
                    .target
                    .saturating_add(item.around)
                    .saturating_add(item.before * i64::from(before))
                    .saturating_add(item.after * i64::from(after));
                item.x = room.saturating_sub(item.x).saturating_sub(outer);
            }
        }
        Ok(())
    }
    /// Lays the items `start..end` once each, at its x and the line's
    /// top, keeping what it laid; returns the tallest margin box.
    fn lay_items(
        &mut self,
        id: BoxId,
        strut: StyleId,
        place: &Place,
        start: usize,
        end: usize,
        depth: usize,
    ) -> Result<f64, Error> {
        let mut tallest = 0.0f64;
        for at in start..end {
            let item = self.flex.get(at).ok_or(Error::Invalid)?;
            let x = place.container.x + px(item.x);
            let item = self.lay_one(at, id, strut, place, x, depth)?;
            tallest = tallest.max(item.outer);
        }
        Ok(tallest)
    }
    /// Lays the item `at`, a row's at `x` and the line's top, a column's
    /// at its main offset, keeping its own bounds and what it laid.
    fn lay_one(
        &mut self,
        at: usize,
        id: BoxId,
        strut: StyleId,
        place: &Place,
        x: f64,
        depth: usize,
    ) -> Result<FlexItem, Error> {
        let mut item = *self.flex.get(at).ok_or(Error::Invalid)?;
        let (lines, markers, boxes) = (self.lines.len(), self.markers.len(), self.boxes.len());
        let outer = (self.left, self.top, self.right, self.bottom);
        (self.left, self.top, self.right, self.bottom) = (
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        );
        let laid = if Lay::of(self.styled.style(strut).ok_or(Error::Invalid)?).column {
            self.lay_column_item(&mut item, id, place, depth)
        } else {
            self.lay_flex_item(&mut item, id, strut, place, x, depth)
        };
        let own = (self.left, self.top, self.right, self.bottom);
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
        *self.flex.get_mut(at).ok_or(Error::Invalid)? = item;
        Ok(item)
    }
    /// Lays one item at `x`, its margin box's left, and the line's top.
    fn lay_flex_item(
        &mut self,
        item: &mut FlexItem,
        id: BoxId,
        strut: StyleId,
        place: &Place,
        x: f64,
        depth: usize,
    ) -> Result<(), Error> {
        let top = place.container.y;
        let width = place.container.width;
        match item.source {
            Source::Run(first, until) => {
                item.outer = self.children(
                    Some(first),
                    until,
                    Place {
                        container: Rect {
                            x,
                            y: top,
                            width: px(item.target),
                            height: 0.0,
                        },
                        definite: None,
                        offset: place.offset,
                        strut,
                        // Its lines name the container, as an anonymous
                        // cell's name the box holding its run.
                        block: Some(id),
                    },
                    depth + 1,
                    None,
                )?;
            }
            Source::Element(child) => {
                let c = *self.styled.box_node(child).ok_or(Error::Invalid)?;
                let mut dims = self.block_dimensions(&c, width, place.definite)?;
                if self.role(&c)? == Some(TableRole::Table) {
                    // A table sizes itself in the room its main size and
                    // margins leave, its own margins inside that room.
                    let shift = if item.autos.left {
                        px(item.before)
                    } else {
                        0.0
                    };
                    let room = Place {
                        container: Rect {
                            x: x + shift,
                            width: px(item.target + item.around),
                            ..place.container
                        },
                        ..*place
                    };
                    let (height, _) =
                        self.inline_table(child, dims, &room, top + dims.margin.top, depth + 1)?;
                    item.outer = height + dims.margin.vertical();
                    return Ok(());
                }
                dims.width = px(item.target);
                dims.margin.left = px(item.before);
                dims.margin.right = px(item.after);
                let height = self.block(
                    child,
                    c,
                    dims,
                    (x, top + dims.margin.top),
                    place.offset,
                    depth + 1,
                )?;
                let own = self
                    .boxes
                    .len()
                    .checked_sub(1)
                    .filter(|at| self.boxes.get(*at).is_some_and(|g| g.node == c.node));
                item.geometry = own.map(index).transpose()?;
                item.outer = height + dims.margin.vertical();
            }
        }
        Ok(())
    }
    /// Each of the items `start..end` stretched to their line's cross size
    /// `line`, or moved down within it by its auto margins or alignment
    /// (§9.4, §8.1, §8.3): never up, as `safe` alignment.
    fn align_items(&mut self, start: usize, end: usize, line: f64) -> Result<(), Error> {
        for at in start..end {
            let item = *self.flex.get(at).ok_or(Error::Invalid)?;
            let free = line - item.outer;
            if item.cross == Cross::Stretch {
                if let Some(at) = item.geometry {
                    let geometry = self.boxes.get_mut(at as usize).ok_or(Error::Invalid)?;
                    let decoration = geometry.padding.plus(geometry.border_widths).vertical();
                    let margins = geometry.margin.vertical();
                    let content = (line - margins - decoration)
                        .min(item.max_height)
                        .max(item.min_height)
                        .max(0.0);
                    geometry.content.height = content;
                    geometry.border.height = content + decoration;
                    let border = geometry.border;
                    self.bounds(border)?;
                }
                if let Some(rect) = item.bounds {
                    self.bounds(rect)?;
                }
                continue;
            }
            let Autos { top, bottom, .. } = item.autos;
            let shift = if top || bottom {
                let free = free.max(0.0);
                let above = match (top, bottom) {
                    (true, true) => free / 2.0,
                    (true, false) => free,
                    _ => 0.0,
                };
                if let Some(at) = item.geometry {
                    let geometry = self.boxes.get_mut(at as usize).ok_or(Error::Invalid)?;
                    if top {
                        geometry.margin.top = above;
                    }
                    if bottom {
                        geometry.margin.bottom = free - above;
                    }
                }
                above
            } else {
                match item.cross {
                    Cross::Center => free / 2.0,
                    Cross::End => free,
                    _ => 0.0,
                }
            };
            let shift = px(units(shift.max(0.0)));
            if shift != 0.0 {
                let range = |(start, end): (u32, u32)| (start as usize, end as usize);
                let laid = (range(item.lines), range(item.boxes), range(item.markers));
                self.move_down(laid, shift)?;
            }
            if let Some(rect) = item.bounds {
                self.bounds(Rect {
                    y: rect.y + shift,
                    ..rect
                })?;
            }
        }
        Ok(())
    }
    /// Lays a column's items down the page at their hypothetical main
    /// sizes, then grows them into free space a definite or least height
    /// leaves and places them by auto margins or `justify-content`, each
    /// moving down only (§9.2 to §9.7, §8.1, §8.2); returns its main size.
    /// Items are not shrunk: where they overflow, they stay as laid.
    fn flex_column(
        &mut self,
        id: BoxId,
        b: &BoxNode,
        dims: &Dimensions,
        place: Place,
        base: usize,
        depth: usize,
    ) -> Result<(f64, f64), Error> {
        let style = self.styled.style(b.style).ok_or(Error::Invalid)?;
        let gap = length(style, Property::RowGap, place.definite).map_or(0, |g| units(g.max(0.0)));
        let align = keyword(style, Property::AlignItems);
        let justified = main_justify(keyword(style, Property::JustifyContent), Lay::of(style));
        let floor = match place.definite {
            Some(height) => Some(units(height)),
            None => (dims.min_height > 0.0).then(|| units(dims.min_height)),
        };
        if floor.is_none() {
            return self.flex_column_laid(id, b, dims, &place, (base, gap, align), depth);
        }
        self.flex_items(b, align)?;
        let end = self.flex.len();
        // Free space, and so any move, needs a definite or least height,
        // and none is left once the items laid reach it. Short of it, an
        // item moves only where `justify-content` places items off the
        // start, by its own auto top margin, or by an earlier item's auto
        // bottom margin or growth: decided from styles before each item is
        // laid, as a table decides its rows'.
        let mut done = false;
        let mut opened = matches!(
            justified,
            "end"
                | "flex-end"
                | "right"
                | "center"
                | "space-between"
                | "space-around"
                | "space-evenly"
        );
        let (mut retained, mut outermost) = (false, false);
        let mut y = 0i64;
        for at in base..end {
            if at > base {
                y = y.saturating_add(gap);
            }
            let (moves, opens) = match self.flex.get(at).ok_or(Error::Invalid)?.source {
                Source::Element(child) => {
                    let c = self.styled.box_node(child).ok_or(Error::Invalid)?;
                    let style = self.styled.style(c.style).ok_or(Error::Invalid)?;
                    let auto = |p| length(style, p, Some(place.container.width)).is_none();
                    let grows = factor(style, Property::FlexGrow) > 0;
                    (
                        auto(Property::MarginTop),
                        grows || auto(Property::MarginBottom),
                    )
                }
                Source::Run(..) => (false, false),
            };
            if !done && !retained && (opened || moves) {
                outermost = self.retain == 0;
                if outermost {
                    self.filtered = (self.lines.len(), self.markers.len());
                }
                self.retain = self.retain.checked_add(1).ok_or(Error::Invalid)?;
                retained = true;
            }
            self.flex.get_mut(at).ok_or(Error::Invalid)?.laid = y;
            let item = self.lay_one(at, id, b.style, &place, place.container.x, depth)?;
            y = y.saturating_add(item.outer_main());
            if !coordinate(px(y)) {
                return Err(Error::Limit("MAX_COORDINATE"));
            }
            opened |= opens;
            if !done && floor.is_some_and(|floor| y >= floor) {
                done = true;
                if retained {
                    if outermost {
                        self.keep_window()?;
                    }
                    self.retain -= 1;
                    retained = false;
                }
            }
        }
        let main = match place.definite {
            Some(height) => units(height),
            None => y.min(units(dims.max_height)).max(units(dims.min_height)),
        };
        let gaps = gap.saturating_mul(end.saturating_sub(base + 1) as i64);
        let room = main.saturating_sub(gaps);
        if y < main {
            self.resolve(base, end, room, depth)?;
        } else {
            for item in self.flex.get_mut(base..end).ok_or(Error::Invalid)? {
                item.target = item.clamp(item.base);
            }
        }
        let lay = Lay::of(style);
        self.place_items(base, end, room, gap, justified, lay)?;
        for at in base..end {
            self.settle(at)?;
        }
        if retained {
            self.retain -= 1;
            if outermost {
                self.keep_window()?;
            }
        }
        Ok((px(main.max(0)), px(y.max(0))))
    }
    /// Lays a column of no definite or least height, which leaves its
    /// items no free space, so none grows or moves: each as it is met,
    /// at its hypothetical main size, its record dropped once it is laid.
    fn flex_column_laid(
        &mut self,
        id: BoxId,
        b: &BoxNode,
        dims: &Dimensions,
        place: &Place,
        (base, gap, align): (usize, i64, &str),
        depth: usize,
    ) -> Result<(f64, f64), Error> {
        let mut next = b.first_child;
        let mut y = 0i64;
        let mut first = true;
        while let Some(mut item) = self.next_flex_item(&mut next, align)? {
            if !first {
                y = y.saturating_add(gap);
            }
            first = false;
            item.laid = y;
            self.counters.admit_growth(&mut self.flex)?;
            self.flex.try_push(item)?;
            let item = self.lay_one(base, id, b.style, place, place.container.x, depth)?;
            let target = item.clamp(item.base);
            let laid = self.flex.get_mut(base).ok_or(Error::Invalid)?;
            (laid.target, laid.x) = (target, y);
            self.settle(base)?;
            self.flex.truncate(base);
            y = y.saturating_add(target).saturating_add(item.around);
            if !coordinate(px(y)) {
                return Err(Error::Limit("MAX_COORDINATE"));
            }
        }
        Ok((px(y.min(units(dims.max_height)).max(0)), px(y.max(0))))
    }
    /// Lays one column item at its main offset as laid, at its cross
    /// size: a stretched block the container's width less its margins,
    /// any other its own or shrink-to-fit, placed across by its auto
    /// margins or alignment; then its hypothetical main size, its laid
    /// height but for a definite `flex-basis` (§9.2).
    fn lay_column_item(
        &mut self,
        item: &mut FlexItem,
        id: BoxId,
        place: &Place,
        depth: usize,
    ) -> Result<(), Error> {
        let width = place.container.width;
        let top = place.container.y + px(item.laid);
        let across = |cross: Cross, free: f64| match cross {
            Cross::Center => (free / 2.0).max(0.0),
            Cross::End => free.max(0.0),
            _ => 0.0,
        };
        match item.source {
            Source::Run(first, until) => {
                let room = if item.cross == Cross::Stretch {
                    width
                } else {
                    let (least, most) =
                        self.content_with(Some(first), until, place.strut, None, depth + 1)?;
                    px(most).min(px(least).max(width))
                };
                let x = place.container.x + px(units(across(item.cross, width - room)));
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
                        offset: place.offset,
                        strut: place.strut,
                        block: Some(id),
                    },
                    depth + 1,
                    None,
                )?;
                item.base = holding(item.outer);
                item.min = item.base;
            }
            Source::Element(child) => {
                let c = *self.styled.box_node(child).ok_or(Error::Invalid)?;
                let style = self.styled.style(c.style).ok_or(Error::Invalid)?;
                item.grow = factor(style, Property::FlexGrow);
                item.shrink = factor(style, Property::FlexShrink);
                let side = |p| length(style, p, Some(width)).is_none();
                item.autos = Autos {
                    left: side(Property::MarginLeft),
                    right: side(Property::MarginRight),
                    top: side(Property::MarginTop),
                    bottom: side(Property::MarginBottom),
                };
                let table = self.role(&c)? == Some(TableRole::Table);
                let picture = c.kind == BoxKind::Image;
                let stretch = item.cross == Cross::Stretch
                    && !table
                    && !picture
                    && !item.autos.left
                    && !item.autos.right
                    && length(style, Property::Width, Some(width)).is_none();
                let mut dims = if stretch || table {
                    self.block_dimensions(&c, width, place.definite)?
                } else {
                    self.shrink_dimensions(child, &c, place, depth)?
                };
                // Auto main margins are 0 until free space is shared.
                if item.autos.top {
                    dims.margin.top = 0.0;
                }
                if item.autos.bottom {
                    dims.margin.bottom = 0.0;
                }
                (item.before, item.after) = (units(dims.margin.top), units(dims.margin.bottom));
                let vertical = dims.decoration().vertical();
                let mut natural = 0.0;
                let height = if table {
                    // A table is laid as block flow lays one, its auto
                    // margins sharing the room it leaves, else placed in
                    // it by its alignment.
                    let room = Place {
                        container: Rect {
                            y: top,
                            ..place.container
                        },
                        ..*place
                    };
                    let share = match item.cross {
                        Cross::Center => 0.5,
                        Cross::End => 1.0,
                        _ => 0.0,
                    };
                    let item = (child, dims, share);
                    self.table_item(item, &room, top + dims.margin.top, depth + 1)?
                } else {
                    let outer = dims.width
                        + dims.decoration().horizontal()
                        + dims.margin.left
                        + dims.margin.right;
                    let free = width - outer;
                    let x = match (item.autos.left, item.autos.right) {
                        (true, true) => {
                            dims.margin.left = (free / 2.0).max(0.0);
                            dims.margin.right = (free - dims.margin.left).max(0.0);
                            place.container.x
                        }
                        (true, false) => {
                            dims.margin.left = free.max(0.0);
                            place.container.x
                        }
                        (false, true) => {
                            dims.margin.right = free.max(0.0);
                            place.container.x
                        }
                        _ => place.container.x + px(units(across(item.cross, free))),
                    };
                    let height = self.block(
                        child,
                        c,
                        dims,
                        (x, top + dims.margin.top),
                        place.offset,
                        depth + 1,
                    )?;
                    natural = self.natural;
                    let own = self
                        .boxes
                        .len()
                        .checked_sub(1)
                        .filter(|at| self.boxes.get(*at).is_some_and(|g| g.node == c.node));
                    item.geometry = own.map(index).transpose()?;
                    height
                };
                item.outer = height + dims.margin.vertical();
                let decoration = if table { 0.0 } else { vertical };
                item.around = item.before + item.after + units(decoration);
                let laid = holding(height - decoration).max(0);
                let border_box = keyword(style, Property::BoxSizing) == "border-box";
                let content = |v: f64| {
                    if border_box && !table {
                        units((v - decoration).max(0.0))
                    } else {
                        units(v.max(0.0))
                    }
                };
                let definite = place.definite;
                let basis = match style.get(Property::FlexBasis) {
                    Some(ComputedValue::Keyword(_)) => None,
                    Some(ComputedValue::Length(Length::Auto)) | None => {
                        length(style, Property::Height, definite).map(content)
                    }
                    Some(_) => length(style, Property::FlexBasis, definite).map(content),
                };
                let low = length(style, Property::MinHeight, definite).map(content);
                item.max = length(style, Property::MaxHeight, definite).map_or(NONE, content);
                let overflow = keyword(style, Property::OverflowY);
                let automatic = low.is_none() && matches!(overflow, "visible" | "clip");
                item.base = basis.unwrap_or(laid);
                item.min = match low {
                    Some(low) => low,
                    // Its content's height at an auto height, or its own
                    // where that is less (§4.5); a table's as laid.
                    None if automatic && table => laid.min(item.max),
                    None if automatic => holding(natural).min(laid).min(item.max),
                    None => 0,
                };
                // td does not scroll a box (td-web/DESIGN.md §10): a box
                // that would scroll, or hides what a scroll pane in it
                // would, is no smaller than it laid.
                if matches!(overflow, "auto" | "scroll" | "hidden") {
                    item.min = item.min.max(laid);
                }
            }
        }
        item.bound();
        Ok(())
    }
    /// Moves a column item down to its main offset and gives its box its
    /// target main size and used auto margins.
    fn settle(&mut self, at: usize) -> Result<(), Error> {
        let item = *self.flex.get(at).ok_or(Error::Invalid)?;
        let before = if item.autos.top { item.before } else { 0 };
        let shift = item
            .x
            .saturating_add(before)
            .saturating_sub(item.laid)
            .max(0);
        let shift = px(shift);
        if shift != 0.0 {
            let range = |(start, end): (u32, u32)| (start as usize, end as usize);
            let laid = (range(item.lines), range(item.boxes), range(item.markers));
            self.move_down(laid, shift)?;
        }
        if let Some(at) = item.geometry {
            let geometry = self.boxes.get_mut(at as usize).ok_or(Error::Invalid)?;
            let decoration = geometry.padding.plus(geometry.border_widths).vertical();
            let content = px(item.target);
            if units(geometry.content.height) != item.target {
                geometry.content.height = content;
                geometry.border.height = content + decoration;
            }
            if item.autos.top {
                geometry.margin.top = px(item.before);
            }
            if item.autos.bottom {
                geometry.margin.bottom = px(item.after);
            }
            let border = geometry.border;
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
}
