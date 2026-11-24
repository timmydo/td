//! CSS Flexbox 1 single-line row layout: flex items (§4), their base and
//! hypothetical sizes and automatic minimum (§9.2, §4.5), resolving
//! flexible lengths (§9.7), main-axis alignment with auto margins and
//! `justify-content` (§9.5, §8.1, §8.2), and the line's cross size,
//! stretch and cross-axis alignment (§9.4, §8.3). See DESIGN.md §5,
//! "Flex".
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

/// How a flex container is laid: one row, one row where its items fit
/// it unwrapped (`flex-wrap: wrap`), or block flow (a column).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Lay {
    Row,
    Wrap,
    Column,
}

impl Lay {
    pub(super) fn of(style: &ComputedStyle) -> Self {
        if matches!(
            keyword(style, Property::FlexDirection),
            "column" | "column-reverse"
        ) {
            Lay::Column
        } else if matches!(keyword(style, Property::FlexWrap), "wrap" | "wrap-reverse") {
            Lay::Wrap
        } else {
            Lay::Row
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

/// A flex item: its sizes in 1/64 px, of its content box but for a
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
    /// Its padding, border and margins but auto ones, horizontally.
    around: i64,
    /// Auto margins: left, right, top, bottom.
    autos: Autos,
    /// Its used left and right margins.
    left: i64,
    right: i64,
    frozen: bool,
    /// The sign of its last clamp: below its minimum, above its maximum.
    violation: i8,
    /// Its margin box's x from the container's content edge.
    x: i64,
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
            left: 0,
            right: 0,
            frozen: false,
            violation: 0,
            x: 0,
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
/// `free` laid by `justify-content` (§8.2): every share a floor of the
/// cumulative one, so the shares sum exactly.
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
    /// items', else its children's as block flow, a column's among them.
    pub(super) fn box_content(
        &mut self,
        b: &BoxNode,
        lead: Option<(i64, bool)>,
        depth: usize,
    ) -> Result<(i64, i64), Error> {
        if self.flex_container(b)? {
            let style = self.styled.style(b.style).ok_or(Error::Invalid)?;
            match Lay::of(style) {
                Lay::Row => return self.flex_intrinsic(b, false, depth),
                Lay::Wrap => return self.flex_intrinsic(b, true, depth),
                Lay::Column => {}
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
    /// Lays the flex container `id`'s items in one row in its content
    /// box, at `x`, `y` with `offset` and `dims` its own, as `block`
    /// lays a block's children, and returns its content height: the
    /// line's cross size (§9.4). A column, and a wrapping row whose
    /// items do not fit it unwrapped, lay their children as block flow.
    #[cold]
    #[inline(never)]
    pub(super) fn flex_content(
        &mut self,
        id: BoxId,
        b: &BoxNode,
        dims: &Dimensions,
        (x, y, offset): (f64, f64, (f64, f64)),
        depth: usize,
    ) -> Result<f64, Error> {
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
        if Lay::of(style) != Lay::Column {
            let base = self.flex.len();
            let laid = self.flex_line(id, b, dims, place, base, depth);
            self.flex.truncate(base);
            if let Some(line) = laid? {
                return Ok(line);
            }
        }
        // A flex container is no list item, so has no inside marker.
        self.children(b.first_child, None, place, depth, None)
    }
    fn flex_line(
        &mut self,
        id: BoxId,
        b: &BoxNode,
        dims: &Dimensions,
        place: Place,
        base: usize,
        depth: usize,
    ) -> Result<Option<f64>, Error> {
        let style = self.styled.style(b.style).ok_or(Error::Invalid)?;
        let width = place.container.width;
        let room = down(width);
        let gap = length(style, Property::ColumnGap, Some(width)).map_or(0, |g| units(g.max(0.0)));
        let align = keyword(style, Property::AlignItems);
        self.flex_items(b, align)?;
        let count = self.flex.len() - base;
        for at in base..self.flex.len() {
            self.size_item(at, &place, depth)?;
        }
        let gaps = gap.saturating_mul(count.saturating_sub(1) as i64);
        // A wrapping row whose items' hypothetical sizes do not fit it
        // unwrapped is laid as block flow, decided before anything is.
        // Each item's base size, margins and padding with border are
        // rounded to 1/64 px apart, half a unit each at most, and the
        // room down: two units an item and one more are that rounding,
        // so percentages summing to 100% fit at every width.
        if Lay::of(style) == Lay::Wrap {
            let items = self.flex.get(base..).ok_or(Error::Invalid)?;
            let hypothetical = items.iter().fold(gaps, |sum, item| {
                sum.saturating_add(item.clamp(item.base) + item.around)
            });
            let slack = 2i64.saturating_mul(count as i64).saturating_add(1);
            if hypothetical > room.saturating_add(slack) {
                return Ok(None);
            }
        }
        self.resolve(base, room.saturating_sub(gaps), depth)?;
        let justified = keyword(style, Property::JustifyContent);
        self.place_items(base, room, gap, justified)?;
        // Decided before anything is laid: whether any item will move
        // down once the line's height is known.
        let moves = self
            .flex
            .get(base..)
            .ok_or(Error::Invalid)?
            .iter()
            .any(|item| {
                matches!(item.cross, Cross::Center | Cross::End)
                    || item.autos.top
                    || item.autos.bottom
            });
        let outermost = moves && self.retain == 0;
        if moves {
            if outermost {
                self.filtered = (self.lines.len(), self.markers.len());
            }
            self.retain = self.retain.checked_add(1).ok_or(Error::Invalid)?;
        }
        let last_baseline = self.last_baseline;
        let laid = self.lay_items(id, b.style, &place, base, depth);
        let line = laid.and_then(|tallest| self.align_items(base, tallest, dims, &place));
        self.last_baseline = last_baseline;
        if moves {
            self.retain -= 1;
        }
        let line = line?;
        if outermost {
            self.keep_window()?;
        }
        Ok(Some(line))
    }
    /// The flex items of `b`, onto the stack: each element child, and
    /// each run of other children holding more than white space (§4).
    fn flex_items(&mut self, b: &BoxNode, align: &str) -> Result<(), Error> {
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
                    let item = FlexItem::new(Source::Run(first, next), cross("auto", align));
                    self.counters.admit_growth(&mut self.flex)?;
                    self.flex.try_push(item)?;
                }
                solid = false;
            }
            let Some((id, c)) = child else {
                return Ok(());
            };
            if element {
                let style = self.styled.style(c.style).ok_or(Error::Invalid)?;
                let item = FlexItem::new(
                    Source::Element(id),
                    cross(keyword(style, Property::AlignSelf), align),
                );
                self.counters.admit_growth(&mut self.flex)?;
                self.flex.try_push(item)?;
            } else {
                run = run.or(Some(id));
                if !solid {
                    solid = !self.blank(&c)?;
                }
            }
            next = c.next_sibling;
        }
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
                (item.left, item.right) = (units(margin.left), units(margin.right));
                let table = self.role(&c)? == Some(TableRole::Table);
                let picture = c.kind == BoxKind::Image;
                let decoration = dims.decoration().horizontal();
                item.around = item.left + item.right;
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
    /// Resolves the items' flexible lengths in `room`, their content
    /// width's less the gaps (§9.7): each item's target main size.
    fn resolve(&mut self, base: usize, room: i64, depth: usize) -> Result<(), Error> {
        let count = self.flex.len() - base;
        self.counters.step(count)?;
        let items = self.flex.get_mut(base..).ok_or(Error::Invalid)?;
        let hypothetical: i64 = items.iter().fold(0i64, |sum, item| {
            sum.saturating_add(item.clamp(item.base) + item.around)
        });
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
            for at in base..self.flex.len() {
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
        let initial = room.saturating_sub(used(self.flex.get(base..).ok_or(Error::Invalid)?));
        loop {
            // Each pass visits every item, a unit each: a cascade of
            // freezes is charged as it goes.
            self.counters.step(3 * count)?;
            let items = self.flex.get_mut(base..).ok_or(Error::Invalid)?;
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
    /// Each item's x and used horizontal margins: auto margins share the
    /// positive free space, else `justify-content` places the items in
    /// it, or past it where it is negative (§8.1, §8.2, §9.5).
    fn place_items(
        &mut self,
        base: usize,
        room: i64,
        gap: i64,
        justified: &str,
    ) -> Result<(), Error> {
        let items = self.flex.get_mut(base..).ok_or(Error::Invalid)?;
        let count = items.len() as i64;
        let used = items.iter().fold(0i64, |sum, item| {
            sum.saturating_add(item.target).saturating_add(item.around)
        });
        let gaps = gap.saturating_mul((count - 1).max(0));
        let free = room.saturating_sub(used).saturating_sub(gaps);
        let autos = items
            .iter()
            .map(|item| i64::from(item.autos.left) + i64::from(item.autos.right))
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
                if item.autos.left {
                    item.left = share(&mut seen);
                }
                if item.autos.right {
                    item.right = share(&mut seen);
                }
                item.x = cursor;
                cursor = cursor
                    .saturating_add(
                        item.target + item.around + item.left * i64::from(item.autos.left),
                    )
                    .saturating_add(item.right * i64::from(item.autos.right))
                    .saturating_add(gap);
            }
            return Ok(());
        }
        for (at, item) in items.iter_mut().enumerate() {
            if item.autos.left {
                item.left = 0;
            }
            if item.autos.right {
                item.right = 0;
            }
            item.x = cursor.saturating_add(justify(justified, free, at as i64, count));
            cursor = cursor
                .saturating_add(item.target)
                .saturating_add(item.around)
                .saturating_add(gap);
        }
        Ok(())
    }
    /// Lays each item once, at its x and the line's top, keeping what it
    /// laid; returns the tallest margin box.
    fn lay_items(
        &mut self,
        id: BoxId,
        strut: StyleId,
        place: &Place,
        base: usize,
        depth: usize,
    ) -> Result<f64, Error> {
        let mut tallest = 0.0f64;
        for at in base..self.flex.len() {
            let mut item = *self.flex.get(at).ok_or(Error::Invalid)?;
            let x = place.container.x + px(item.x);
            let (lines, markers, boxes) = (self.lines.len(), self.markers.len(), self.boxes.len());
            let outer = (self.left, self.top, self.right, self.bottom);
            (self.left, self.top, self.right, self.bottom) = (
                f64::INFINITY,
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::NEG_INFINITY,
            );
            let laid = self.lay_flex_item(&mut item, id, strut, place, x, depth);
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
            tallest = tallest.max(item.outer);
            *self.flex.get_mut(at).ok_or(Error::Invalid)? = item;
        }
        Ok(tallest)
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
                    let shift = if item.autos.left { px(item.left) } else { 0.0 };
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
                dims.margin.left = px(item.left);
                dims.margin.right = px(item.right);
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
    /// The line's cross size, and each item stretched to it or moved down
    /// within it by its auto margins or alignment (§9.4, §8.1, §8.3):
    /// never up, as `safe` alignment. Returns the line's cross size.
    fn align_items(
        &mut self,
        base: usize,
        tallest: f64,
        dims: &Dimensions,
        place: &Place,
    ) -> Result<f64, Error> {
        // One line: the container's definite height, else its items'
        // within its own min and max heights (§9.4 step 8).
        let line = match place.definite {
            Some(height) => height,
            None => tallest.max(0.0).min(dims.max_height).max(dims.min_height),
        };
        // In whole 1/64 px, never less than it holds.
        let whole = units(line);
        let line = px(if px(whole) + 1e-9 < line {
            whole + 1
        } else {
            whole
        });
        if !coordinate(line) {
            return Err(Error::Limit("MAX_COORDINATE"));
        }
        for at in base..self.flex.len() {
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
        Ok(line)
    }
}
