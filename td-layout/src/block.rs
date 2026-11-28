//! Normal-flow geometry and windowed line retention.
use crate::inline::{
    coordinate, white_space, Counters, Cuts, Flow, FontMetrics, ImageRun, Line, LineBounds,
    LineEnd, LineSink, Marker, Pending, TextRun,
};
use crate::list::{MarkerFragment, MarkerKind};
use crate::{
    float_side, inline_block, inline_picture, keyword, BoxId, BoxKind, BoxNode, Error, StyleId,
    StyledDocument, TableRole, MAX_PAGE_BYTES,
};
use float::{units, Clear, Floats, Item, Kind, View};
use std::ops::Range;
use std::sync::atomic::AtomicBool;
use td_alloc::{Budget, BudgetVec};
use td_css::values::Length;
use td_css::{ComputedStyle, ComputedValue, Property};
use td_html::document::NodeId;

#[cfg(test)]
mod ends;
mod flex;
#[cfg(test)]
mod flex_tests;
pub(crate) mod float;
#[cfg(test)]
mod float_tests;
pub(crate) mod grid;
#[cfg(test)]
mod grid_tests;
#[cfg(test)]
mod inline_block_tests;
#[cfg(test)]
mod inline_flex_tests;
#[cfg(test)]
mod inline_grid_tests;
#[cfg(test)]
mod inline_table_tests;
mod intrinsic;
#[cfg(test)]
mod relay_tests;
mod table;

pub const MAX_VIEWPORT: f64 = td_css::values::MAX_ABSOLUTE_VALUE;
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Edges {
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
    pub left: f64,
}
impl Edges {
    fn horizontal(self) -> f64 {
        self.left + self.right
    }
    fn vertical(self) -> f64 {
        self.top + self.bottom
    }
    fn plus(self, other: Self) -> Self {
        Self {
            top: self.top + other.top,
            right: self.right + other.right,
            bottom: self.bottom + other.bottom,
            left: self.left + other.left,
        }
    }
}
/// A replaced element's natural size as the caller knows it (DESIGN.md
/// §5). Dimensions are in 1/64 CSS px, FontMetrics' unit, so none is NaN
/// or infinite; each is clamped to MAX_VIEWPORT. The ratio is width to
/// height in any one unit, and one with a zero term is none: it is not
/// derived from the two dimensions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IntrinsicSize {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub ratio: Option<(u32, u32)>,
}
impl IntrinsicSize {
    /// A decoded picture's size in 1/64 CSS px: both dimensions and their
    /// ratio.
    pub fn picture(width: u32, height: u32) -> Self {
        Self {
            width: Some(width),
            height: Some(height),
            ratio: Some((width, height)),
        }
    }
    /// picture() from whole CSS px, saturating; the ratio keeps the px.
    pub fn picture_px(width: u32, height: u32) -> Self {
        Self {
            width: Some(width.saturating_mul(64)),
            height: Some(height.saturating_mul(64)),
            ratio: Some((width, height)),
        }
    }
}
/// The caller's intrinsic sizes, asked once for each image box a layout
/// sizes, with the image's style (td-web measures an alt line in its
/// font); None keeps the attribute placeholder (DESIGN.md §5). Layout
/// holds the FontMetrics mutably meanwhile, so a lookup measures with its
/// own handle or answers what was measured before the layout.
pub trait IntrinsicSizes {
    fn intrinsic(&self, node: NodeId, style: StyleId) -> Option<IntrinsicSize>;
}
impl<F: Fn(NodeId, StyleId) -> Option<IntrinsicSize>> IntrinsicSizes for F {
    fn intrinsic(&self, node: NodeId, style: StyleId) -> Option<IntrinsicSize> {
        self(node, style)
    }
}
impl std::fmt::Debug for dyn IntrinsicSizes + Sync + '_ {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("IntrinsicSizes")
    }
}
#[derive(Clone, Debug)]
pub struct Request<'c> {
    pub width: f64,
    pub height: f64,
    pub window: Option<Range<f64>>,
    /// Replaced elements' intrinsic sizes; changing them needs only a
    /// new layout of the same styled document.
    pub intrinsic: Option<&'c (dyn IntrinsicSizes + Sync)>,
    /// Old layout output kept alive during this call, excluding shared input.
    pub retained_output_bytes: usize,
    /// This call's ceiling on charged bytes, in place of the styled
    /// document's Limits::bytes; clamped to MAX_PAGE_BYTES. A document
    /// built by new() or with_state() owns a td-alloc budget sized from its
    /// Limits::bytes, so a raise past that can still fail Error::Memory;
    /// with_budget() and shared() callers size the budget themselves.
    pub bytes: Option<usize>,
    /// Read every CANCEL_INTERVAL work units; another thread may set it.
    pub cancel: Option<&'c AtomicBool>,
}
impl Request<'_> {
    pub fn new(width: f64, height: f64) -> Self {
        Self {
            width,
            height,
            window: None,
            intrinsic: None,
            retained_output_bytes: 0,
            bytes: None,
            cancel: None,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BoxGeometry {
    pub node: NodeId,
    pub style: StyleId,
    pub content: Rect,
    pub border: Rect,
    pub padding: Edges,
    pub border_widths: Edges,
    pub margin: Edges,
    pub replaced: bool,
}
pub struct Layout {
    pub boxes: BudgetVec<BoxGeometry>,
    pub lines: BudgetVec<Line>,
    /// List markers, generated text with no DOM bytes (DESIGN.md §5, "List
    /// markers"), retained as their lines are: those with a line in line
    /// order, outer item first; one whose item laid none as its item ends.
    pub markers: BudgetVec<MarkerFragment>,
    pub height: f64,
    pub overflow: Rect,
    pub line_count: usize,
    /// Lines cut at Limits::line_scalars, whether retained or not.
    pub cuts: Cuts,
    pub work: usize,
    /// What this layout admitted: `Charges::held` (tree and styled),
    /// `retained_output_bytes` and this output (DESIGN.md §2). It sees no
    /// sibling styled document: one sharing the tree counts it again, and
    /// a caller holding another folds it into `retained_output_bytes`.
    pub page_bytes: usize,
    /// This output's charge alone, what a later request retains.
    pub output_bytes: usize,
}
pub fn layout(
    styled: &StyledDocument<'_>,
    metrics: &mut impl FontMetrics,
    request: &Request<'_>,
) -> Result<Layout, Error> {
    layout_with_budget(styled, metrics, request, styled.budget())
}
/// Keep output and scratch under a caller-selected child of the page budget.
pub fn layout_with_budget(
    styled: &StyledDocument<'_>,
    metrics: &mut impl FontMetrics,
    request: &Request<'_>,
    memory: &Budget,
) -> Result<Layout, Error> {
    for n in [request.width, request.height] {
        if !n.is_finite() || !(0.0..=MAX_VIEWPORT).contains(&n) {
            return Err(Error::Limit("MAX_VIEWPORT"));
        }
    }
    if request
        .window
        .as_ref()
        .is_some_and(|w| !coordinate(w.start) || !coordinate(w.end) || w.start > w.end)
    {
        return Err(Error::Invalid);
    }
    let mut counters = Counters::new(styled, request.cancel);
    if let Some(bytes) = request.bytes {
        counters.limits.bytes = bytes.min(MAX_PAGE_BYTES);
    }
    counters.charge(request.retained_output_bytes)?;
    let initial = counters.bytes;
    // Every ordinary block and block-level image emits one geometry record,
    // independently of wrapping or the requested window.
    let capacity = styled.geometry_count;
    if capacity != 0 {
        counters.charge(
            capacity
                .checked_mul(std::mem::size_of::<BoxGeometry>())
                .and_then(|n| n.checked_mul(2))
                .and_then(|n| n.checked_add(32))
                .ok_or(Error::Limit("MAX_PAGE_BYTES"))?,
        )?;
    }
    // A whole layout keeps at most a marker an item: admitted once, at that
    // capacity, as the geometry is. A window's grow as they are kept.
    let marker_capacity = if request.window.is_none() {
        styled.marker_count()
    } else {
        0
    };
    if marker_capacity != 0 {
        counters.charge(
            marker_capacity
                .checked_mul(std::mem::size_of::<MarkerFragment>())
                .and_then(|n| n.checked_mul(2))
                .and_then(|n| n.checked_add(32))
                .ok_or(Error::Limit("MAX_PAGE_BYTES"))?,
        )?;
    }
    // The first tick reads the flag after the up-front limits and before
    // anything is allocated.
    counters.step(0)?;
    let boxes = td_alloc::BudgetVec::with_capacity(memory, capacity)?;
    let markers = td_alloc::BudgetVec::with_capacity(memory, marker_capacity)?;
    let mut engine = Engine {
        styled,
        memory,
        metrics,
        counters,
        request,
        boxes,
        lines: BudgetVec::new(memory)?,
        markers,
        waiting: BudgetVec::new(memory)?,
        pending: BudgetVec::new(memory)?,
        last_dimensions: None,
        last_marker: None,
        list_cursor: 0,
        retain: 0,
        first_line: false,
        first_index: None,
        filtered: (0, 0),
        tables: table::Scratch::new(memory)?,
        floats: Floats::new(memory)?,
        nested: BudgetVec::new(memory)?,
        flex: BudgetVec::new(memory)?,
        flex_lines: BudgetVec::new(memory)?,
        flex_order: BudgetVec::new(memory)?,
        last_baseline: None,
        natural: 0.0,
        flex_wanted: false,
        flex_watch: None,
        flex_lead: None,
        flex_first: None,
        flex_wants: 0,
        flex_depends: 0,
        relaying: 0,
        flex_marks: BudgetVec::new(memory)?,
        flex_measure: false,
        flex_natural: false,
        grid: BudgetVec::new(memory)?,
        grid_tracks: BudgetVec::new(memory)?,
        grid_cells: BudgetVec::new(memory)?,
        grid_lanes: BudgetVec::new(memory)?,
        grid_tree: BudgetVec::new(memory)?,
        grid_order: BudgetVec::new(memory)?,
        grid_names: (grid::Names::new(memory)?, grid::Names::new(memory)?),
        grid_known: BudgetVec::new(memory)?,
        inner: (0.0, 0.0, 0.0, 0.0),
        grid_measured: BudgetVec::new(memory)?,
        left: 0.0,
        top: 0.0,
        right: request.width,
        bottom: 0.0,
    };
    let height = if let Some(first) = styled.first_box() {
        let strut = styled.box_node(first).ok_or(Error::Invalid)?.style;
        engine.children(
            Some(first),
            None,
            Place {
                container: Rect {
                    x: 0.0,
                    y: 0.0,
                    width: request.width,
                    height: request.height,
                },
                definite: Some(request.height),
                offset: (0.0, 0.0),
                strut,
                block: None,
            },
            0,
            None,
        )?
    } else {
        0.0
    };
    Ok(Layout {
        boxes: engine.boxes,
        lines: engine.lines,
        markers: engine.markers,
        height: height.max(engine.bottom),
        overflow: Rect {
            x: engine.left,
            y: engine.top,
            width: engine.right - engine.left,
            height: engine.bottom - engine.top,
        },
        line_count: engine.counters.lines,
        cuts: engine.counters.cuts,
        work: engine.counters.work,
        page_bytes: engine.counters.bytes,
        output_bytes: engine.counters.bytes - initial,
    })
}
struct Engine<'s, 'd, 'm, 'r, M> {
    styled: &'s StyledDocument<'d>,
    memory: &'s Budget,
    metrics: &'m mut M,
    counters: Counters<'r>,
    request: &'r Request<'r>,
    boxes: BudgetVec<BoxGeometry>,
    lines: BudgetVec<Line>,
    markers: BudgetVec<MarkerFragment>,
    /// Outside markers of open list items not yet on a line, outermost
    /// first; the next line laid takes them all.
    waiting: BudgetVec<Waiting>,
    pending: BudgetVec<Pending>,
    last_dimensions: Option<(StyleId, f64, Option<f64>, Dimensions)>,
    /// The last marker measured: a bullet of the same style and kind, or
    /// the same number, measures the same.
    last_marker: Option<Marker>,
    /// Layout meets list items in box order: the next one's index.
    list_cursor: usize,
    /// Tables being laid: while any is, a line the window may yet meet is
    /// kept, since a cell's lines move down once its row is done; the
    /// outermost filters them for the window as each row with no rowspan
    /// open ends, and as it ends.
    retain: u32,
    /// Keep the next line whatever the window: a cell's first, whose
    /// baseline its row is aligned by.
    first_line: bool,
    /// The line that took `first_line`, which a float's lines laid before
    /// it may precede, or the box a grid's first item of no line gave.
    first_index: Option<First>,
    /// The lines and markers before these the outermost table has filtered.
    filtered: (usize, usize),
    /// Tables' grids, a stack: a nested table's after its ancestors'.
    tables: table::Scratch,
    /// The open formatting contexts' floats (DESIGN.md §5, "Floats").
    floats: Floats,
    /// The boxes inline sequences laid apart and have not yet placed, a
    /// stack: a nested sequence's after its ancestors' (§5, "Floats").
    nested: BudgetVec<Item>,
    /// The flex items of the containers being laid, a stack: a nested
    /// container's after its ancestors' (§5, "Flex").
    flex: BudgetVec<flex::FlexItem>,
    /// The lines of the wrapping containers being laid, a stack as
    /// `flex` is.
    flex_lines: BudgetVec<flex::FlexLine>,
    /// A row's items by `order` then tree order, as `place_items` places
    /// them: scratch, refilled for each row that orders its items.
    flex_order: BudgetVec<(i32, u32)>,
    /// The baseline of the last line laid in flow, on a page with
    /// inline-blocks: an inline-block's own is its last line's (§5,
    /// "Inline-blocks"). Nested items and tables keep it as they found it.
    last_baseline: Option<f64>,
    /// The content height the last block laid would take at an auto
    /// height, a flex container's its lines' or items' and a picture's
    /// its own: a flex column item's automatic minimum (§5, "Flex").
    natural: f64,
    /// The next flex or grid container laid is an inline-flex or an
    /// inline-grid, whose baseline its line takes: it watches
    /// `flex_watch`, its first item, for its first line, `flex_lead`, and
    /// leaves its first baseline in `flex_first` (§5, "Flex", "Grid").
    flex_wanted: bool,
    flex_watch: Option<usize>,
    flex_lead: Option<First>,
    flex_first: Option<f64>,
    /// On a page with flex containers: whether the content of the box
    /// being laid would lay otherwise at another definite height of it,
    /// gathered as it is laid; the last box laid's, which a flex item
    /// takes; and the re-lays of flex items at their used sizes open
    /// around what is being laid (§5, "Flex", "Laying again").
    flex_wants: u8,
    flex_depends: u8,
    relaying: u32,
    /// The state before each item of the lines and columns being laid
    /// that may be laid again, a stack as `flex` is.
    flex_marks: BudgetVec<flex::Mark>,
    /// The next box laid is a column item of percentage height measured
    /// for its automatic minimum, and laid again at its used height.
    flex_measure: bool,
    /// The next box laid is a column item whose height as laid at an
    /// auto height is its automatic minimum's content height: a row
    /// laid in it lays its stretched items so first.
    flex_natural: bool,
    /// The items of the grid containers being laid or measured, a stack
    /// as `flex` is, and their tracks (§5, "Grid").
    grid: BudgetVec<grid::GridItem>,
    grid_tracks: BudgetVec<grid::Track>,
    /// Scratch while a grid's items are placed: the cells they occupy, a
    /// bitset, and each lane's cursor.
    grid_cells: BudgetVec<u64>,
    grid_lanes: BudgetVec<u32>,
    /// The grids being laid's rows' floors, Fenwick trees, a stack.
    grid_tree: BudgetVec<i64>,
    /// Items by span as a grid's tracks are sized: scratch.
    grid_order: BudgetVec<(u32, u32)>,
    /// Scratch while a grid's items are placed: its rows' and columns'
    /// line name indexes, emptied for each grid, so charged once at the
    /// largest (§5, "Grid", "Bounds").
    grid_names: (grid::Names<'s>, grid::Names<'s>),
    /// Each grid's items' measured contributions, kept for the layout
    /// once it is measured a second time, so a grid laid again or
    /// measured again measures none more: by the box of its first item's
    /// source, never measured, measured once, or where they start in
    /// `grid_measured` (DESIGN.md §5, "Grid", "Bounds").
    grid_known: BudgetVec<u32>,
    grid_measured: BudgetVec<grid::Measured>,
    /// On a page with grid containers, the bounds a block's content
    /// reached before its own border box was taken in: what a grid item
    /// stretched to another height overflows (§5, "Grid", "Alignment").
    inner: (f64, f64, f64, f64),
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
}
/// An outside marker and the x its end abuts: its item's border edge.
#[derive(Clone, Copy)]
struct Waiting {
    marker: Marker,
    end: f64,
}
#[derive(Clone, Copy)]
enum Event {
    Inline(BoxId),
    Block(BoxId),
    /// A float, met within an inline sequence or between blocks, which
    /// it does not divide (DESIGN.md §5, "Floats").
    Float(BoxId),
    /// An inline-block, laid whole in its line (DESIGN.md §5,
    /// "Inline-blocks").
    Atomic(BoxId),
}
impl Event {
    /// Inline content, a float or an inline-block, which an inline
    /// sequence holds.
    fn flows(self) -> bool {
        matches!(self, Self::Inline(_) | Self::Float(_) | Self::Atomic(_))
    }
}
struct Events {
    inline: [Option<Event>; 4],
    len: usize,
    extra: Option<BudgetVec<Event>>,
}
impl Events {
    fn get(&self, at: usize) -> Option<&Event> {
        if at < self.inline.len() {
            self.inline.get(at).and_then(Option::as_ref)
        } else {
            self.extra.as_ref()?.get(at - self.inline.len())
        }
    }
    fn add(&mut self, event: Event, counters: &mut Counters, memory: &Budget) -> Result<(), Error> {
        if let Some(slot) = self.inline.get_mut(self.len) {
            *slot = Some(event);
        } else {
            if self.extra.is_none() {
                self.extra = Some(BudgetVec::new(memory)?);
            }
            let extra = self.extra.as_mut().ok_or(Error::Invalid)?;
            counters.admit_growth(extra)?;
            extra.try_push(event)?;
        }
        self.len = self.len.checked_add(1).ok_or(Error::Invalid)?;
        Ok(())
    }
}
#[derive(Clone, Copy)]
struct Dimensions {
    width: f64,
    height: Option<f64>,
    margin: Edges,
    padding: Edges,
    border: Edges,
    min_height: f64,
    max_height: f64,
    relative: (f64, f64),
    /// A picture of specified width: its height were its `height` auto,
    /// which its width keeps; None for any other box, whose width an
    /// auto height would change or which is no picture.
    auto_height: Option<f64>,
}
impl Dimensions {
    fn decoration(self) -> Edges {
        self.padding.plus(self.border)
    }
}
/// Which lines and markers layout keeps.
#[derive(Clone, Copy)]
enum Keep<'r> {
    All,
    Window(&'r Range<f64>),
    /// In a table: those the window may meet once moved down, as a cell's
    /// alignment only moves them.
    Above(&'r Range<f64>),
}
impl Keep<'_> {
    fn meets(self, y: f64, height: f64) -> bool {
        match self {
            Self::All => true,
            Self::Window(window) => meets(window, y, height),
            Self::Above(window) => window.start < window.end && y < window.end,
        }
    }
}
impl<'r, M> Engine<'_, '_, '_, 'r, M> {
    fn window(&self) -> Keep<'r> {
        match self.request.window.as_ref() {
            None => Keep::All,
            Some(window) if self.retain > 0 => Keep::Above(window),
            Some(window) => Keep::Window(window),
        }
    }
}
impl<M: FontMetrics> Engine<'_, '_, '_, '_, M> {
    fn block_dimensions(
        &mut self,
        b: &BoxNode,
        width: f64,
        height: Option<f64>,
    ) -> Result<Dimensions, Error> {
        // Ordinary block dimensions depend only on the shared style and
        // containing dimensions. Replaced elements also read node attributes
        // and the request's intrinsic sizes.
        if b.kind == BoxKind::Block {
            if let Some((style, w, h, value)) = self.last_dimensions {
                if style == b.style && w == width && h == height {
                    return Ok(value);
                }
            }
        }
        let style = self.styled.style(b.style).ok_or(Error::Invalid)?;
        let intrinsic = lookup(self.request, b);
        let value = dimensions(self.styled, b, style, width, height, intrinsic)?;
        if b.kind == BoxKind::Block {
            self.last_dimensions = Some((b.style, width, height, value));
        }
        Ok(value)
    }
    fn bounds(&mut self, rect: Rect) -> Result<(), Error> {
        include_bounds(
            &mut self.left,
            &mut self.top,
            &mut self.right,
            &mut self.bottom,
            rect,
        )
    }
    /// The inline items and blocks of the siblings from `first` up to
    /// `until`, inline boxes flattened.
    fn events(&mut self, first: Option<BoxId>, until: Option<BoxId>) -> Result<Events, Error> {
        let mut events = Events {
            inline: [None; 4],
            len: 0,
            extra: None,
        };
        let mut stack: Option<BudgetVec<BoxId>> = None;
        let mut next = first;
        while let Some(id) = next.filter(|id| Some(*id) != until) {
            self.counters.step(1)?;
            let b = *self.styled.box_node(id).ok_or(Error::Invalid)?;
            if b.kind == BoxKind::Inline {
                if let Some(child) = b.first_child {
                    if let Some(sibling) = b.next_sibling {
                        if stack.is_none() {
                            stack = Some(BudgetVec::new(self.memory)?);
                        }
                        stack.as_mut().ok_or(Error::Invalid)?.try_push(sibling)?;
                    }
                    next = Some(child);
                    continue;
                }
            } else {
                let style = self.styled.style(b.style).ok_or(Error::Invalid)?;
                let block = b.kind == BoxKind::Block
                    || (b.kind == BoxKind::Image
                        && !inline_picture(keyword(style, Property::Display)));
                let floats = self.styled.has_floats()
                    && b.parent.is_some()
                    && float_side(style, b.kind).is_some();
                let atomic = self.styled.has_inline_blocks()
                    && b.parent.is_some()
                    && inline_block(style, b.kind);
                events.add(
                    if floats {
                        Event::Float(id)
                    } else if atomic {
                        Event::Atomic(id)
                    } else if block {
                        Event::Block(id)
                    } else {
                        Event::Inline(id)
                    },
                    &mut self.counters,
                    self.memory,
                )?;
            }
            next = b
                .next_sibling
                .or_else(|| stack.as_mut().and_then(BudgetVec::pop));
        }
        Ok(events)
    }
    fn children(
        &mut self,
        first: Option<BoxId>,
        until: Option<BoxId>,
        place: Place,
        depth: usize,
        mut inside: Option<Marker>,
    ) -> Result<f64, Error> {
        let Place {
            container,
            definite,
            offset,
            ..
        } = place;
        if depth > self.counters.limits.depth {
            return Err(Error::Limit("MAX_DEPTH"));
        }
        let events = self.events(first, until)?;
        let mut at = 0;
        let mut run = Run {
            y: container.y,
            previous_margin: None,
        };
        while let Some(event) = events.get(at).copied() {
            match event {
                Event::Block(id) => {
                    // An inside marker before a block is a line of its own.
                    if let Some(marker) = inside.take() {
                        self.inline_run(&events, at..at, &place, &mut run, Some(marker), depth)?;
                    }
                    let b = *self.styled.box_node(id).ok_or(Error::Invalid)?;
                    let role = self.role(&b)?;
                    if let Some(end) = self.table_run(&events, at, role)? {
                        // Table boxes without a table: one anonymous table,
                        // with no margins of its own (CSS 2.2 §17.2.1).
                        run.y += run.previous_margin.unwrap_or(0.0);
                        run.y += self.table(
                            table::Source::Anonymous(&events, at..end),
                            &place,
                            run.y,
                            depth + 1,
                        )?;
                        run.previous_margin = Some(0.0);
                        at = end;
                        if !coordinate(run.y) {
                            return Err(Error::Limit("MAX_COORDINATE"));
                        }
                        continue;
                    }
                    let mut dims = self.block_dimensions(&b, container.width, definite)?;
                    let gap = run.previous_margin.map_or(dims.margin.top, |previous| {
                        collapse(previous, dims.margin.top)
                    });
                    run.y += gap;
                    let mut x = container.x;
                    if self.styled.has_floats() {
                        (x, dims) = self.among_floats(&b, role, dims, &place, &mut run.y)?;
                    }
                    run.y += if role == Some(TableRole::Table) {
                        self.table(table::Source::Root(id, dims), &place, run.y, depth + 1)?
                    } else {
                        self.block(id, b, dims, (x, run.y), offset, depth + 1)?
                    };
                    run.previous_margin = Some(dims.margin.bottom);
                    at += 1;
                }
                Event::Inline(_) | Event::Float(_) | Event::Atomic(_) => {
                    let start = at;
                    while events.get(at).is_some_and(|e| e.flows()) {
                        at += 1;
                    }
                    self.inline_run(&events, start..at, &place, &mut run, inside.take(), depth)?;
                }
            }
            if !coordinate(run.y) {
                return Err(Error::Limit("MAX_COORDINATE"));
            }
        }
        // An item with nothing inline still lays its inside marker.
        if let Some(marker) = inside.take() {
            self.inline_run(&events, at..at, &place, &mut run, Some(marker), depth)?;
        }
        let end = run.y + run.previous_margin.unwrap_or(0.0);
        let height = (end - container.y).max(0.0);
        if !coordinate(end) || !coordinate(height) {
            return Err(Error::Limit("MAX_COORDINATE"));
        }
        Ok(height)
    }
    /// One inline sequence, the `range` of `events`, led by an inside
    /// marker when it is its list item's first. Its floats and
    /// inline-blocks are laid first, where it starts, then placed as its
    /// lines meet them and moved there, their lines kept whatever the
    /// window until then (§5, "Floats", "Inline-blocks").
    fn inline_run(
        &mut self,
        events: &Events,
        range: Range<usize>,
        place: &Place,
        run: &mut Run,
        marker: Option<Marker>,
        depth: usize,
    ) -> Result<(), Error> {
        let Place {
            container,
            definite,
            offset,
            strut,
            block,
        } = *place;
        let run_y = run.y + run.previous_margin.unwrap_or(0.0);
        let floating = self.styled.has_floats()
            && (range.start..range.end).any(|at| matches!(events.get(at), Some(Event::Float(_))));
        let nesting = floating
            || (self.styled.has_inline_blocks()
                && (range.start..range.end)
                    .any(|at| matches!(events.get(at), Some(Event::Atomic(_)))));
        let outermost = nesting && self.retain == 0;
        let base = self.nested.len();
        if nesting {
            self.lay_nested(events, &range, place, run_y, outermost, depth)?;
        }
        let sequence = self.lines.len();
        let before = self.counters.lines;
        let style = self.styled.style(strut).ok_or(Error::Invalid)?;
        let align = keyword(style, Property::TextAlign);
        let window = self.window();
        let mut sink = Positioned {
            lines: &mut self.lines,
            markers: &mut self.markers,
            waiting: &mut self.waiting,
            window,
            first: &mut self.first_line,
            first_index: &mut self.first_index,
            x: container.x + offset.0,
            origin: container.x,
            dy: offset.1,
            width: container.width,
            align,
            left: &mut self.left,
            top: &mut self.top,
            right: &mut self.right,
            bottom: &mut self.bottom,
        };
        let mut flow = Flow::new(
            self.styled,
            self.metrics,
            container.width.floor() as u32,
            strut,
            run_y,
            self.memory,
            &mut self.pending,
        )?
        .naming(block);
        if nesting {
            flow.nesting(&mut self.nested, &self.boxes, offset);
        }
        if floating || (self.styled.has_floats() && self.floats.any()) {
            flow.beside(
                &mut self.floats,
                (container.x, container.x + container.width),
                &mut self.counters,
            )?;
        }
        if let Some(marker) = marker {
            flow.marker(&marker, &mut self.counters, &mut sink)?;
        }
        let mut next_item = base;
        for index in range {
            let event = events.get(index).ok_or(Error::Invalid)?;
            let id = match event {
                Event::Inline(id) => id,
                Event::Float(_) => {
                    flow.float(next_item, &mut self.counters)?;
                    next_item += 1;
                    continue;
                }
                Event::Atomic(id) => {
                    let b = self.styled.box_node(*id).ok_or(Error::Invalid)?;
                    // The breaks around it are its parent's.
                    let parent = b.parent.and_then(|p| self.styled.box_node(p));
                    let style = parent
                        .and_then(|p| self.styled.style(p.style))
                        .ok_or(Error::Invalid)?;
                    flow.inline_block(
                        next_item,
                        (b.node, b.style),
                        white_space(style).2,
                        &mut self.counters,
                        &mut sink,
                    )?;
                    next_item += 1;
                    continue;
                }
                Event::Block(_) => return Err(Error::Invalid),
            };
            let b = self.styled.box_node(*id).ok_or(Error::Invalid)?;
            match b.kind {
                BoxKind::Text => {
                    let text = self
                        .styled
                        .document()
                        .node(b.node)
                        .and_then(|n| n.text())
                        .ok_or(Error::Invalid)?;
                    flow.text(
                        &TextRun {
                            node: b.node,
                            style: b.style,
                            range: 0..text.len(),
                        },
                        &mut self.counters,
                        &mut sink,
                    )?;
                }
                BoxKind::Break => {
                    flow.hard_break(b.style, LineEnd::Break, &mut self.counters, &mut sink)?
                }
                BoxKind::Image => {
                    let style = self.styled.style(b.style).ok_or(Error::Invalid)?;
                    if self.styled.lays_apart() && flex::percent_height(style) {
                        self.flex_wants |= flex::HEIGHT | flex::PERCENT;
                    }
                    let dims = dimensions(
                        self.styled,
                        b,
                        style,
                        container.width,
                        definite,
                        lookup(self.request, b),
                    )?;
                    let decor = dims.decoration();
                    flow.image(
                        ImageRun {
                            node: b.node,
                            style: b.style,
                            width: dims.width + decor.horizontal(),
                            height: dims.height.unwrap_or(0.0) + decor.vertical(),
                            margin: [
                                dims.margin.top,
                                dims.margin.right,
                                dims.margin.bottom,
                                dims.margin.left,
                            ],
                        },
                        &mut self.counters,
                        &mut sink,
                    )?;
                }
                _ => return Err(Error::Invalid),
            }
        }
        flow.finish(&mut self.counters, &mut sink)?;
        let end_y = flow.y;
        if self.styled.has_inline_blocks() && flow.baseline.is_some() {
            self.last_baseline = flow.baseline;
        }
        if nesting {
            self.place_nested(base, sequence, outermost)?;
        }
        if self.counters.lines != before {
            run.y = end_y;
            run.previous_margin = None;
        }
        Ok(())
    }
    /// Whether `b` roots a block formatting context of its own, whose
    /// floats its lines alone see and its auto height holds: the root, a
    /// float, an inline-block, a caption, a flex or grid container or
    /// item, or a
    /// block whose overflow is neither visible nor clip (a table cell is
    /// one too, which its table opens).
    fn opens_context(&self, b: &BoxNode) -> Result<bool, Error> {
        let style = self.styled.style(b.style).ok_or(Error::Invalid)?;
        Ok(b.kind == BoxKind::Block
            && (b.parent.is_none()
                || float_side(style, b.kind).is_some()
                || matches!(
                    keyword(style, Property::Display),
                    "table-caption" | "inline-block"
                )
                || !matches!(keyword(style, Property::OverflowX), "visible" | "clip")
                || !matches!(keyword(style, Property::OverflowY), "visible" | "clip")
                || (self.styled.lays_apart() && self.apart(b)?)))
    }
    /// Whether `b` is a flex or grid container or item.
    fn apart(&self, b: &BoxNode) -> Result<bool, Error> {
        Ok(
            (self.styled.has_flex() && (self.flex_container(b)? || self.flex_item(b)?))
                || (self.styled.has_grid() && (self.grid_container(b)? || self.grid_item(b)?)),
        )
    }
    /// A block met in a context with floats: below those it clears, and
    /// a context's root (not a table) or a picture beside those at its
    /// top; its x and dimensions.
    #[cold]
    #[inline(never)]
    fn among_floats(
        &mut self,
        b: &BoxNode,
        role: Option<TableRole>,
        dims: Dimensions,
        place: &Place,
        y: &mut f64,
    ) -> Result<(f64, Dimensions), Error> {
        let style = self.styled.style(b.style).ok_or(Error::Invalid)?;
        let clear = Clear::of(keyword(style, Property::Clear));
        if let Some(below) = self.floats.clearance(clear) {
            *y = y.max(below);
        }
        // A context's root, or a block-level picture (CSS 2.2 §9.5).
        if role != Some(TableRole::Table)
            && self.floats.any()
            && (b.kind == BoxKind::Image || self.opens_context(b)?)
        {
            return self.beside_floats(b, dims, place, y);
        }
        Ok((place.container.x, dims))
    }
    /// A formatting context's root in flow beside floats: its x and
    /// dimensions in the room they leave at its top, its auto width that
    /// room's, where it fits; else `y` moves down past the nearest float
    /// until it does or none is beside it.
    #[cold]
    #[inline(never)]
    fn beside_floats(
        &mut self,
        b: &BoxNode,
        sized: Dimensions,
        place: &Place,
        y: &mut f64,
    ) -> Result<(f64, Dimensions), Error> {
        // A picture keeps the size its container gives it.
        let picture = b.kind == BoxKind::Image;
        let sides = (place.container.x, place.container.x + place.container.width);
        loop {
            let band = self
                .floats
                .band(sides, *y, 0.0, View::Lines, &mut self.counters)?;
            let room = band.room();
            let dims = if picture {
                sized
            } else {
                self.block_dimensions(b, room, place.definite)?
            };
            // Its margins as computed: sizing gives an over-constrained
            // box's excess to its right margin.
            let style = self.styled.style(b.style).ok_or(Error::Invalid)?;
            let margin = edges(
                style,
                [
                    Property::MarginTop,
                    Property::MarginRight,
                    Property::MarginBottom,
                    Property::MarginLeft,
                ],
                // A picture's percentages are its container's.
                if picture { place.container.width } else { room },
                false,
            );
            let outer = dims.width + dims.decoration().horizontal() + margin.horizontal();
            let fits = units(outer) <= units(room);
            match band.below {
                Some(below) if !fits => *y = below,
                Some(_) if picture => {
                    // Its margins resolved again in the room, auto ones
                    // sharing what it leaves, as sizing shares the
                    // container's.
                    let free = (room - outer).max(0.0);
                    let basis = Some(place.container.width);
                    let auto_left = length(style, Property::MarginLeft, basis).is_none();
                    let auto_right = length(style, Property::MarginRight, basis).is_none();
                    let mut dims = dims;
                    dims.margin.left = match (auto_left, auto_right) {
                        (true, true) => free / 2.0,
                        (true, false) => free,
                        (false, _) => margin.left,
                    };
                    dims.margin.right = match (auto_left, auto_right) {
                        (true, true) => free / 2.0,
                        (false, true) => free,
                        (_, false) => margin.right + if auto_left { 0.0 } else { free },
                    };
                    return Ok((band.left, dims));
                }
                Some(_) => return Ok((band.left, dims)),
                None if picture => return Ok((place.container.x, sized)),
                None => {
                    let dims = self.block_dimensions(b, place.container.width, place.definite)?;
                    return Ok((place.container.x, dims));
                }
            }
        }
    }
    /// An inline sequence's floats and inline-blocks laid first, its
    /// lines and theirs kept whatever the window until it ends.
    #[cold]
    #[inline(never)]
    fn lay_nested(
        &mut self,
        events: &Events,
        range: &Range<usize>,
        place: &Place,
        y: f64,
        outermost: bool,
        depth: usize,
    ) -> Result<(), Error> {
        if outermost {
            self.filtered = (self.lines.len(), self.markers.len());
        }
        self.retain = self.retain.checked_add(1).ok_or(Error::Invalid)?;
        for at in range.start..range.end {
            match events.get(at).copied() {
                Some(Event::Float(id)) => self.lay_float(id, place, y, depth)?,
                Some(Event::Atomic(id)) => self.lay_inline_block(id, place, y, depth)?,
                _ => {}
            }
        }
        Ok(())
    }
    /// The sequence ended: its floats and inline-blocks moved where it
    /// placed them, their lines put after those they were met in, and,
    /// outermost, the window kept.
    #[cold]
    #[inline(never)]
    fn place_nested(&mut self, base: usize, sequence: usize, outermost: bool) -> Result<(), Error> {
        self.move_nested(base)?;
        self.order_nested(base, sequence)?;
        self.nested.truncate(base);
        self.retain -= 1;
        if outermost {
            self.keep_window()?;
        }
        Ok(())
    }
    /// Lays the float `id` met in an inline sequence starting at `y`: its
    /// margin box's top left at the container's left edge and `y`, its
    /// width its set one or shrink-to-fit (CSS 2.2 §10.3.5), its auto
    /// margins 0, its output and its own overflow kept as an item to be
    /// moved where the sequence places it.
    #[cold]
    #[inline(never)]
    fn lay_float(&mut self, id: BoxId, place: &Place, y: f64, depth: usize) -> Result<(), Error> {
        let b = *self.styled.box_node(id).ok_or(Error::Invalid)?;
        let style = self.styled.style(b.style).ok_or(Error::Invalid)?;
        let side = float_side(style, b.kind).ok_or(Error::Invalid)?;
        let clear = Clear::of(keyword(style, Property::Clear));
        let dims = self.shrink_dimensions(id, &b, place, depth)?;
        let kind = Kind::Float { side, clear };
        self.lay_item(id, dims, (kind, false), place, y, depth)
    }
    /// Lays the inline-block `id` met in an inline sequence starting at
    /// `y` as a float is laid, CSS 2.2 §10.3.9 sizing it as §10.3.5 does a
    /// float, or an inline table as its table sizes itself; `lay_item`
    /// finds its baseline (§10.8.1).
    #[cold]
    #[inline(never)]
    fn lay_inline_block(
        &mut self,
        id: BoxId,
        place: &Place,
        y: f64,
        depth: usize,
    ) -> Result<(), Error> {
        let b = *self.styled.box_node(id).ok_or(Error::Invalid)?;
        let table = self.role(&b)? == Some(TableRole::Table);
        let dims = if table {
            self.block_dimensions(&b, place.container.width, place.definite)?
        } else {
            self.shrink_dimensions(id, &b, place, depth)?
        };
        let kind = Kind::Inline { baseline: 0.0 };
        self.lay_item(id, dims, (kind, table), place, y, depth)
    }
    /// Lays the box `id`, `dims` in its container, apart where an inline
    /// sequence starting at `y` starts: its margin box's top left at the
    /// container's left edge and `y`, no outside marker waiting for its
    /// lines and none of them its container's first nor its last in flow,
    /// its output and its own overflow kept as an item of `kind` to be
    /// moved where the sequence places it; an inline table, `table`, laid
    /// by its table. An inline-block's kind takes its baseline: its last
    /// line's in flow, else, or where its overflow is neither visible nor
    /// clip, its bottom margin edge; an inline table's its first row's,
    /// else its bottom margin edge (CSS 2.2 §10.8.1, §17.4).
    #[cold]
    #[inline(never)]
    fn lay_item(
        &mut self,
        id: BoxId,
        dims: Dimensions,
        (kind, table): (Kind, bool),
        place: &Place,
        y: f64,
        depth: usize,
    ) -> Result<(), Error> {
        let b = *self.styled.box_node(id).ok_or(Error::Invalid)?;
        let decor = dims.decoration();
        let x = place.container.x;
        let (lines, markers, boxes) = (self.lines.len(), self.markers.len(), self.boxes.len());
        let first_line = std::mem::replace(&mut self.first_line, false);
        let waiting = std::mem::replace(&mut self.waiting, BudgetVec::new(self.memory)?);
        let last_baseline = self.last_baseline.take();
        let outer = (self.left, self.top, self.right, self.bottom);
        (self.left, self.top, self.right, self.bottom) = (
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        );
        let flex = matches!(kind, Kind::Inline { .. })
            && !table
            && (self.flex_container(&b)? || self.grid_container(&b)?);
        self.flex_wanted = flex;
        let laid = if table {
            self.inline_table(id, dims, place, y + dims.margin.top, depth + 1)
        } else {
            self.block(
                id,
                b,
                dims,
                (x, y + dims.margin.top),
                place.offset,
                depth + 1,
            )
            .map(|height| (height, None))
        };
        let own = (self.left, self.top, self.right, self.bottom);
        (self.left, self.top, self.right, self.bottom) = outer;
        self.waiting = waiting;
        self.first_line = first_line;
        let last = std::mem::replace(&mut self.last_baseline, last_baseline);
        let (laid, mut first_row) = laid?;
        if flex {
            // An inline-flex's or inline-grid's is its first baseline, as
            // a table's is its first row's.
            first_row = self.flex_first.take();
        }
        let height = laid + dims.margin.vertical();
        let width = if table {
            // A table sizes itself: its margin box is that of its box, the
            // last it laid.
            self.boxes
                .last()
                .filter(|g| g.node == b.node)
                .map(|g| g.border.width + g.margin.left + g.margin.right)
                .ok_or(Error::Invalid)?
        } else {
            dims.width + decor.horizontal() + dims.margin.horizontal()
        };
        if !coordinate(width) || !coordinate(height) {
            return Err(Error::Limit("MAX_COORDINATE"));
        }
        let mut kind = kind;
        // A float's height is no less than 0; an inline-block's keeps its
        // sign, its line taking its margin box above and below its
        // baseline.
        let mut kept = height.max(0.0);
        if let Kind::Inline { baseline } = &mut kind {
            let style = self.styled.style(b.style).ok_or(Error::Invalid)?;
            // Clip, like visible, keeps its lines' baseline.
            let shown = |p| matches!(keyword(style, p), "visible" | "clip");
            // An inline table's is its first row's; laying a table keeps
            // `last_baseline`, so it has no `last`.
            *baseline = match (first_row, last) {
                (Some(row), _) => row - y,
                (None, Some(last)) if shown(Property::OverflowX) && shown(Property::OverflowY) => {
                    last - y
                }
                (None, _) => height,
            };
            if !coordinate(*baseline) {
                return Err(Error::Limit("MAX_COORDINATE"));
            }
            kept = height;
        }
        let bounds = (own.0 <= own.2).then_some(Rect {
            x: own.0,
            y: own.1,
            width: own.2 - own.0,
            height: own.3 - own.1,
        });
        let item = Item::new(
            (width, kept),
            kind,
            (x, y),
            (
                (lines, self.lines.len()),
                (boxes, self.boxes.len()),
                (markers, self.markers.len()),
            ),
            bounds,
        )?;
        self.counters.admit_growth(&mut self.nested)?;
        self.nested.try_push(item)?;
        Ok(())
    }
    /// A float's or an inline-block's dimensions in its container: as a
    /// block's, or an image's, but with its margins as computed, auto ones
    /// 0, and an auto width shrink-to-fit, min(max(min-content, the room),
    /// max-content) within its min and max widths (CSS 2.2 §10.3.5,
    /// §10.3.9).
    #[cold]
    #[inline(never)]
    fn shrink_dimensions(
        &mut self,
        id: BoxId,
        b: &BoxNode,
        place: &Place,
        depth: usize,
    ) -> Result<Dimensions, Error> {
        let width = place.container.width;
        let style = self.styled.style(b.style).ok_or(Error::Invalid)?;
        let mut dims = dimensions(
            self.styled,
            b,
            style,
            width,
            place.definite,
            lookup(self.request, b),
        )?;
        dims.margin = edges(
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
        if b.kind != BoxKind::Block || length(style, Property::Width, Some(width)).is_some() {
            return Ok(dims);
        }
        let decoration = dims.decoration().horizontal();
        let border_box = keyword(style, Property::BoxSizing) == "border-box";
        let content = |v: f64| {
            if border_box {
                (v - decoration).max(0.0)
            } else {
                v.max(0.0)
            }
        };
        let low = length(style, Property::MinWidth, Some(width)).map_or(0.0, content);
        let high = length(style, Property::MaxWidth, Some(width)).map_or(f64::INFINITY, content);
        let lead = self.inside_marker(id, b)?;
        let (least, most) = self.box_content(b, lead, depth + 1)?;
        let room = (width - decoration - dims.margin.horizontal()).max(0.0);
        let fit = (most as f64 / 64.0).min((least as f64 / 64.0).max(room));
        dims.width = fit.min(high).max(low);
        if !coordinate(dims.width) {
            return Err(Error::Limit("MAX_COORDINATE"));
        }
        Ok(dims)
    }
    /// Moves each item laid since `base` from where it was laid to where
    /// it was placed, its boxes, lines and markers with it, and counts its
    /// own overflow there.
    #[cold]
    #[inline(never)]
    fn move_nested(&mut self, base: usize) -> Result<(), Error> {
        for at in base..self.nested.len() {
            let item = *self.nested.get(at).ok_or(Error::Invalid)?;
            let (x, y) = item.placed().ok_or(Error::Invalid)?;
            let (dx, dy) = (x - item.laid.0, y - item.laid.1);
            let (lines, boxes, markers) = (item.lines(), item.boxes(), item.markers());
            self.counters
                .step(1 + (lines.1 - lines.0) + (boxes.1 - boxes.0) + (markers.1 - markers.0))?;
            if dx != 0.0 {
                let laid = self.lines.get(lines.0..lines.1).ok_or(Error::Invalid)?;
                let pieces = laid
                    .iter()
                    .fold(0usize, |n, l| n + l.fragments.len() + l.replaced.len());
                self.counters.step(pieces)?;
            }
            if dx != 0.0 || dy != 0.0 {
                for line in self.lines.get_mut(lines.0..lines.1).ok_or(Error::Invalid)? {
                    line.y += dy;
                    for f in &mut line.fragments {
                        f.x += dx;
                    }
                    for image in &mut line.replaced {
                        image.x += dx;
                        image.y += dy;
                    }
                    if !coordinate(line.y + line.height) {
                        return Err(Error::Limit("MAX_COORDINATE"));
                    }
                }
                for geometry in self.boxes.get_mut(boxes.0..boxes.1).ok_or(Error::Invalid)? {
                    geometry.content.x += dx;
                    geometry.content.y += dy;
                    geometry.border.x += dx;
                    geometry.border.y += dy;
                }
                for marker in self
                    .markers
                    .get_mut(markers.0..markers.1)
                    .ok_or(Error::Invalid)?
                {
                    marker.x += dx;
                    marker.y += dy;
                    marker.baseline += dy;
                }
            }
            if let Some(rect) = item.bounds {
                self.bounds(Rect {
                    x: rect.x + dx,
                    y: rect.y + dy,
                    ..rect
                })?;
            }
        }
        Ok(())
    }
    /// Puts each item laid since `base` (whose lines lie before the
    /// sequence's, from `sequence`) after the line of the sequence holding
    /// what came before it, an inline-block's after the line holding it,
    /// its markers with its lines, so that lines lie in the tree order of
    /// their starts, an item's within the line it was met in (DESIGN.md
    /// §5, "Floats", "Placement"). A float met before any of its sequence
    /// moves nothing.
    #[cold]
    #[inline(never)]
    fn order_nested(&mut self, base: usize, sequence: usize) -> Result<(), Error> {
        let count = self
            .lines
            .len()
            .checked_sub(sequence)
            .ok_or(Error::Invalid)?;
        let mut floor = 0;
        for item in self.nested.get_mut(base..).ok_or(Error::Invalid)? {
            item.anchor = item.anchor.max(floor).min(count);
            floor = item.anchor;
        }
        let items = self.nested.get(base..).ok_or(Error::Invalid)?;
        let (Some(first), Some(last)) = (items.first(), items.last()) else {
            return Ok(());
        };
        if floor == 0 {
            return Ok(());
        }
        let (start, markers) = (first.lines().0, first.markers().0);
        let floated = items
            .iter()
            .fold(0usize, |n, item| n + (item.lines().1 - item.lines().0));
        if last.lines().1 != sequence || sequence - start != floated {
            return Err(Error::Invalid);
        }
        let marked = self
            .markers
            .len()
            .checked_sub(markers)
            .ok_or(Error::Invalid)?;
        self.counters.step(2 * (floated + count + marked))?;
        // The scratch below is charged while held and given back after.
        let charged = self.counters.bytes;
        let index = |n: usize| u32::try_from(n).map_err(|_| Error::Invalid);
        // The markers in their new order: each float's after the
        // sequence's on the lines before it, as their lines will be.
        let mut order: BudgetVec<u32> = BudgetVec::new(self.memory)?;
        let mut cut = last.markers().1;
        for item in items {
            let ahead = sequence + item.anchor;
            while let Some(marker) = self.markers.get(cut) {
                if !marker.line.is_some_and(|line| (line as usize) < ahead) {
                    break;
                }
                self.counters.admit_growth(&mut order)?;
                order.try_push(index(cut - markers)?)?;
                cut += 1;
            }
            for at in item.markers().0..item.markers().1 {
                self.counters.admit_growth(&mut order)?;
                order.try_push(index(at - markers)?)?;
            }
        }
        for at in cut..self.markers.len() {
            self.counters.admit_growth(&mut order)?;
            order.try_push(index(at - markers)?)?;
        }
        // Where each line goes, in its order now: the floats', then the
        // sequence's.
        let mut to: BudgetVec<u32> = BudgetVec::new(self.memory)?;
        let mut before = 0;
        for item in items {
            for at in 0..item.lines().1 - item.lines().0 {
                self.counters.admit_growth(&mut to)?;
                to.try_push(index(item.anchor + before + at)?)?;
            }
            before += item.lines().1 - item.lines().0;
        }
        let (mut next, mut before) = (0, 0);
        for at in 0..count {
            while let Some(item) = items.get(next).filter(|item| item.anchor <= at) {
                before += item.lines().1 - item.lines().0;
                next += 1;
            }
            self.counters.admit_growth(&mut to)?;
            to.try_push(index(at + before)?)?;
        }
        let moved = |line: usize| -> Result<usize, Error> {
            match line.checked_sub(start) {
                Some(at) => Ok(start + *to.get(at).ok_or(Error::Invalid)? as usize),
                None => Ok(line),
            }
        };
        if let Some(First::Line(line)) = self.first_index {
            self.first_index = Some(First::Line(moved(line)?));
        }
        for marker in self.markers.get_mut(markers..).ok_or(Error::Invalid)? {
            if let Some(line) = marker.line {
                marker.line = Some(index(moved(line as usize)?)?);
            }
        }
        permute(self.lines.get_mut(start..).ok_or(Error::Invalid)?, &mut to)?;
        // `order` inverted: where each marker goes.
        to.clear();
        for _ in 0..order.len() {
            self.counters.admit_growth(&mut to)?;
            to.try_push(0)?;
        }
        for (new, &old) in order.iter().enumerate() {
            *to.get_mut(old as usize).ok_or(Error::Invalid)? = index(new)?;
        }
        let permuted = permute(
            self.markers.get_mut(markers..).ok_or(Error::Invalid)?,
            &mut to,
        );
        self.counters.bytes = charged;
        permuted
    }
    /// A list item's marker, measured, and whether it is inside; None for
    /// any other box and for `list-style-type: none`.
    fn marker(&mut self, id: BoxId, b: &BoxNode) -> Result<Option<(Marker, bool)>, Error> {
        if b.kind != BoxKind::Block {
            return Ok(None);
        }
        let style = self.styled.style(b.style).ok_or(Error::Invalid)?;
        if keyword(style, Property::Display) != "list-item" {
            return Ok(None);
        }
        let Some(kind) = MarkerKind::from_keyword(keyword(style, Property::ListStyleType)) else {
            return Ok(None);
        };
        let inside = keyword(style, Property::ListStylePosition) == "inside";
        let ordinal = self.ordinal(id)?;
        if let Some(last) = self.last_marker.filter(|m| {
            m.style == b.style && m.kind == kind && (kind.bullet() || m.ordinal == ordinal)
        }) {
            // Charged as if measured again.
            self.counters.step(last.scalars)?;
            let marker = Marker {
                item: b.node,
                ordinal,
                ..last
            };
            return Ok(Some((marker, inside)));
        }
        let known = self
            .last_marker
            .filter(|m| m.style == b.style)
            .map(|m| (m.above, m.below));
        let marker = Marker::new(
            self.styled,
            self.metrics,
            (b.node, b.style),
            (kind, ordinal),
            known,
            &mut self.counters,
        )?;
        self.last_marker = Some(marker);
        Ok(Some((marker, inside)))
    }
    /// A list item's ordinal, by a cursor through the styled document's
    /// items, which layout meets in order, else by search.
    fn ordinal(&mut self, id: BoxId) -> Result<i32, Error> {
        let items = self.styled.list_items();
        while items.get(self.list_cursor).is_some_and(|i| i.id.0 < id.0) {
            self.list_cursor += 1;
        }
        match items.get(self.list_cursor) {
            Some(item) if item.id == id => Ok(item.ordinal),
            _ => self.styled.ordinal(id).ok_or(Error::Invalid),
        }
    }
    fn block(
        &mut self,
        id: BoxId,
        b: BoxNode,
        dims: Dimensions,
        (x, y): (f64, f64),
        offset: (f64, f64),
        depth: usize,
    ) -> Result<f64, Error> {
        self.counters.step(1)?;
        let decor = dims.decoration();
        let x = x + dims.margin.left;
        let content_x = x + decor.left;
        let content_y = y + decor.top;
        let mut offset = offset;
        offset.0 += dims.relative.0;
        offset.1 += dims.relative.1;
        let mut inside = None;
        if let Some((marker, is_inside)) = self.marker(id, &b)? {
            if is_inside {
                inside = Some(marker);
            } else {
                // Hangs outside: its end abuts the item's border edge.
                self.waiting.try_push(Waiting {
                    marker,
                    end: x + offset.0,
                })?;
            }
        }
        let root = self.styled.has_floats() && self.opens_context(&b)?;
        if root {
            self.floats.open(&mut self.counters)?;
        }
        // What this box's content gathers, and what its parent's had.
        let wants = self
            .styled
            .lays_apart()
            .then(|| std::mem::replace(&mut self.flex_wants, 0));
        // A flex item measured for its column lays its content as at an
        // auto height (§5, "Flex", "Laying again").
        let basis = if wants.is_some() && std::mem::take(&mut self.flex_measure) {
            None
        } else {
            dims.height
        };
        let natural_wanted = wants.is_some() && std::mem::take(&mut self.flex_natural);
        let mut content = None;
        let mut natural = if b.kind == BoxKind::Image {
            content = dims.auto_height;
            dims.height.unwrap_or(0.0)
        } else if self.styled.has_flex() && self.flex_container(&b)? {
            let laid = Dimensions {
                height: basis,
                ..dims
            };
            self.flex_natural = natural_wanted;
            let (used, own) =
                self.flex_content(id, &b, &laid, (content_x, content_y, offset), depth)?;
            content = Some(own);
            used
        } else {
            let place = Place {
                container: Rect {
                    x: content_x,
                    y: content_y,
                    width: dims.width,
                    height: basis.unwrap_or(0.0),
                },
                definite: basis,
                offset,
                strut: b.style,
                block: Some(id),
            };
            if self.styled.has_grid() {
                let (used, own) = self.grid_or_children(&b, &dims, place, depth, inside)?;
                content = own;
                used
            } else {
                self.children(b.first_child, None, place, depth, inside)?
            }
        };
        if root {
            // A context's root holds its floats (CSS 2.2 §10.6.7).
            if let Some(bottom) = self.floats.close()? {
                natural = natural.max(bottom - content_y);
            }
        }
        if self.waiting.last().is_some_and(|w| w.marker.item == b.node) {
            // The item laid no line: its marker stands at its content top,
            // which it holds open.
            let waiting = self.waiting.pop().ok_or(Error::Invalid)?;
            let top = content_y + offset.1;
            let fragment = waiting.marker.fragment(
                waiting.end,
                top + waiting.marker.above as f64 / 64.0,
                false,
            );
            natural = natural.max(fragment.height);
            self.lineless(fragment)?;
        }
        self.natural = content.unwrap_or(natural);
        if let Some(wants) = wants {
            self.flex_depends = self.flex_wants;
            let style = self.styled.style(b.style).ok_or(Error::Invalid)?;
            self.flex_wants = wants
                | if flex::percent_height(style) {
                    flex::HEIGHT | flex::PERCENT
                } else {
                    0
                };
        }
        let height = dims
            .height
            .unwrap_or(natural)
            .min(dims.max_height)
            .max(dims.min_height);
        let geometry = BoxGeometry {
            node: b.node,
            style: b.style,
            content: Rect {
                x: content_x + offset.0,
                y: content_y + offset.1,
                width: dims.width,
                height,
            },
            border: Rect {
                x: x + offset.0,
                y: y + offset.1,
                width: dims.width + decor.horizontal(),
                height: height + decor.vertical(),
            },
            padding: dims.padding,
            border_widths: dims.border,
            margin: dims.margin,
            replaced: b.kind == BoxKind::Image,
        };
        if self.styled.has_grid() {
            self.inner = (self.left, self.top, self.right, self.bottom);
        }
        self.bounds(geometry.border)?;
        self.counters.admit_growth(&mut self.boxes)?;
        self.boxes.try_push(geometry)?;
        Ok(height + decor.vertical())
    }
    /// A marker with no line: within the bounds, and kept where the window
    /// meets it.
    fn lineless(&mut self, fragment: MarkerFragment) -> Result<(), Error> {
        self.counters.step(1)?;
        self.bounds(Rect {
            x: fragment.x,
            y: fragment.y,
            width: fragment.width,
            height: fragment.height,
        })?;
        if self.window().meets(fragment.y, fragment.height) {
            fragment_count(&mut self.counters)?;
            self.counters.admit_growth(&mut self.markers)?;
            self.markers.try_push(fragment)?;
        }
        Ok(())
    }
}
/// What an inline sequence continues in its block: the pen's y and the
/// margin a block before it left.
struct Run {
    y: f64,
    previous_margin: Option<f64>,
}
/// A block's content: its box, definite height, offset and strut, and
/// the block its lines name.
#[derive(Clone, Copy)]
struct Place {
    container: Rect,
    definite: Option<f64>,
    offset: (f64, f64),
    strut: StyleId,
    block: Option<BoxId>,
}
/// Whether a non-empty window meets the span from `y`, `height` tall.
fn meets(window: &Range<f64>, y: f64, height: f64) -> bool {
    window.start < window.end && y + height > window.start && y < window.end
}
fn fragment_count(counters: &mut Counters) -> Result<(), Error> {
    if counters.fragments >= counters.limits.fragments {
        return Err(Error::Limit("MAX_FRAGMENTS"));
    }
    counters.fragments += 1;
    Ok(())
}
/// Where a first baseline was taken: a line's, or, for a grid whose
/// first item in grid order has no line, that item's border box's bottom
/// (§5, "Grid", "Inline grid and baselines"). Read once the boxes and
/// lines holding it are where they end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum First {
    Line(usize),
    Border(usize),
}
impl First {
    /// As a mark keeps it: a box's index with the top bit.
    pub(crate) fn pack(first: Option<Self>) -> Option<Option<u32>> {
        let small = |n: usize| u32::try_from(n).ok().filter(|n| *n < 1 << 31);
        match first {
            None => Some(None),
            Some(Self::Line(n)) => small(n).map(Some),
            Some(Self::Border(n)) => small(n).map(|n| Some(n | 1 << 31)),
        }
    }
    pub(crate) fn unpack(packed: Option<u32>) -> Option<Self> {
        packed.map(|n| {
            if n & 1 << 31 == 0 {
                Self::Line(n as usize)
            } else {
                Self::Border((n & !(1 << 31)) as usize)
            }
        })
    }
}
struct Positioned<'a> {
    lines: &'a mut BudgetVec<Line>,
    markers: &'a mut BudgetVec<MarkerFragment>,
    waiting: &'a mut BudgetVec<Waiting>,
    window: Keep<'a>,
    first: &'a mut bool,
    first_index: &'a mut Option<First>,
    /// Where its lines start, with the relative offset, and without it,
    /// in flow, where items laid apart are placed.
    x: f64,
    origin: f64,
    dy: f64,
    width: f64,
    align: &'a str,
    left: &'a mut f64,
    top: &'a mut f64,
    right: &'a mut f64,
    bottom: &'a mut f64,
}
impl LineSink for Positioned<'_> {
    fn measure(&mut self, bounds: LineBounds, counters: &mut Counters) -> Result<f64, Error> {
        if !self.waiting.is_empty() {
            self.hang(&bounds, counters)?;
        }
        let (inset, room) = bounds.band.unwrap_or((0.0, self.width));
        let aligned = alignment(self.align, room - bounds.advance);
        let shift = self.x + inset + aligned;
        include_bounds(
            self.left,
            self.top,
            self.right,
            self.bottom,
            Rect {
                x: shift + bounds.left,
                y: self.dy + bounds.top,
                width: bounds.right - bounds.left,
                height: bounds.bottom - bounds.top,
            },
        )?;
        // In flow: an item laid apart was laid with the offset already.
        Ok(self.origin + inset + aligned)
    }
    fn wants(&self, y: f64, height: f64) -> bool {
        *self.first || self.window.meets(y + self.dy, height)
    }
    fn line(
        &mut self,
        mut line: Line,
        marker: Option<MarkerFragment>,
        band: Option<(f64, f64)>,
        counters: &mut Counters,
    ) -> Result<(), Error> {
        let (inset, room) = band.unwrap_or((0.0, self.width));
        let shift = self.x + inset + alignment(self.align, room - line.width);
        if let Some(mut marker) = marker {
            marker.x += shift;
            marker.y += self.dy;
            marker.baseline += self.dy;
            marker.line = Some(u32::try_from(self.lines.len()).map_err(|_| Error::Invalid)?);
            if !coordinate(marker.x + marker.width) {
                return Err(Error::Limit("MAX_COORDINATE"));
            }
            counters.admit_growth(self.markers)?;
            self.markers.try_push(marker)?;
        }
        if *self.first {
            *self.first_index = Some(First::Line(self.lines.len()));
        }
        *self.first = false;
        let local_y = line.y;
        line.y += self.dy;
        for f in &mut line.fragments {
            f.x += shift;
            if !coordinate(f.x + f.width) {
                return Err(Error::Limit("MAX_COORDINATE"));
            }
        }
        for f in &mut line.replaced {
            f.x += shift;
            f.y = (local_y + f.y) + self.dy;
            if !coordinate(f.x + f.width) || !coordinate(f.y + f.height) {
                return Err(Error::Limit("MAX_COORDINATE"));
            }
        }
        counters.admit_growth(self.lines)?;
        self.lines.try_push(line)?;
        Ok(())
    }
}
impl Positioned<'_> {
    /// The open items' outside markers take this line, their baselines on
    /// its baseline, and are kept as it is.
    #[cold]
    #[inline(never)]
    fn hang(&mut self, bounds: &LineBounds, counters: &mut Counters) -> Result<(), Error> {
        let wanted = self.wants(bounds.y, bounds.height);
        let line = u32::try_from(self.lines.len()).map_err(|_| Error::Invalid)?;
        let baseline = self.dy + bounds.y + bounds.baseline;
        for waiting in self.waiting.iter() {
            counters.step(1)?;
            let mut fragment = waiting.marker.fragment(waiting.end, baseline, false);
            include_bounds(
                self.left,
                self.top,
                self.right,
                self.bottom,
                Rect {
                    x: fragment.x,
                    y: fragment.y,
                    width: fragment.width,
                    height: fragment.height,
                },
            )?;
            if wanted {
                fragment_count(counters)?;
                fragment.line = Some(line);
                counters.admit_growth(self.markers)?;
                self.markers.try_push(fragment)?;
            }
        }
        self.waiting.clear();
        Ok(())
    }
}
/// Moves each of `items` to `to[its index]`, a permutation, following its
/// cycles: each item moves once, and `to` is spent.
fn permute<T>(items: &mut [T], to: &mut [u32]) -> Result<(), Error> {
    const DONE: u32 = u32::MAX;
    if items.len() != to.len() {
        return Err(Error::Invalid);
    }
    for start in 0..items.len() {
        let mut at = start;
        loop {
            let slot = to.get_mut(at).ok_or(Error::Invalid)?;
            let next = std::mem::replace(slot, DONE);
            if next == DONE || next as usize == start {
                break;
            }
            let next = next as usize;
            if next >= items.len() {
                return Err(Error::Invalid);
            }
            items.swap(start, next);
            at = next;
        }
    }
    Ok(())
}
fn length(style: &ComputedStyle, p: Property, basis: Option<f64>) -> Option<f64> {
    match style.get(p) {
        Some(ComputedValue::Length(Length::Px(n))) => Some(*n),
        Some(ComputedValue::Length(Length::Percent(percent))) => {
            basis.map(|basis| (percent / 100.0 * basis).clamp(-MAX_VIEWPORT, MAX_VIEWPORT))
        }
        Some(ComputedValue::Length(Length::Calc {
            px,
            percent,
            non_negative,
        })) => basis.map(|basis| {
            (px + percent / 100.0 * basis).clamp(
                if *non_negative { 0.0 } else { -MAX_VIEWPORT },
                MAX_VIEWPORT,
            )
        }),
        _ => None,
    }
}
fn edges(style: &ComputedStyle, properties: [Property; 4], basis: f64, nonnegative: bool) -> Edges {
    let mut values = properties.map(|p| length(style, p, Some(basis)).unwrap_or(0.0));
    if nonnegative {
        values = values.map(|v| v.max(0.0));
    }
    let [top, right, bottom, left] = values;
    Edges {
        top,
        right,
        bottom,
        left,
    }
}
fn dimensions(
    styled: &StyledDocument<'_>,
    b: &BoxNode,
    style: &ComputedStyle,
    width: f64,
    height: Option<f64>,
    intrinsic: Option<IntrinsicSize>,
) -> Result<Dimensions, Error> {
    sized(styled, b, style, Some(width), height, intrinsic)
}
/// The content width layout gives `id`, an ordinary block whose
/// ancestors are all ordinary blocks, in a viewport `viewport` wide: each
/// sized against its parent's content as `children` sizes it, the root
/// against the viewport.
pub(crate) fn content_width(
    styled: &StyledDocument<'_>,
    id: BoxId,
    viewport: f64,
    depth: usize,
) -> Result<f64, Error> {
    if depth > styled.limits().depth {
        return Err(Error::Limit("MAX_DEPTH"));
    }
    let b = styled.box_node(id).ok_or(Error::Invalid)?;
    if b.kind != BoxKind::Block || styled.table_role(id).is_some() {
        return Err(Error::Invalid);
    }
    let container = match b.parent {
        Some(parent) => content_width(styled, parent, viewport, depth + 1)?,
        None => viewport,
    };
    let style = styled.style(b.style).ok_or(Error::Invalid)?;
    Ok(dimensions(styled, b, style, container, None, None)?.width)
}
/// dimensions() against a containing width, or none: then percentages of
/// it are auto, or 0 for edges, as an intrinsic contribution takes them.
fn sized(
    styled: &StyledDocument<'_>,
    b: &BoxNode,
    style: &ComputedStyle,
    basis: Option<f64>,
    height: Option<f64>,
    intrinsic: Option<IntrinsicSize>,
) -> Result<Dimensions, Error> {
    use Property::*;
    let width = basis.unwrap_or(0.0);
    let padding = edges(
        style,
        [PaddingTop, PaddingRight, PaddingBottom, PaddingLeft],
        width,
        true,
    );
    let border = edges(
        style,
        [
            BorderTopWidth,
            BorderRightWidth,
            BorderBottomWidth,
            BorderLeftWidth,
        ],
        width,
        true,
    );
    let mut margin = edges(
        style,
        [MarginTop, MarginRight, MarginBottom, MarginLeft],
        width,
        false,
    );
    let decor = padding.plus(border);
    let border_box = keyword(style, BoxSizing) == "border-box";
    let content = |value: f64, decoration: f64| {
        if border_box {
            (value - decoration).max(0.0)
        } else {
            value.max(0.0)
        }
    };
    let specified_width = length(style, Width, basis).map(|v| content(v, decor.horizontal()));
    let specified_height = length(style, Height, height).map(|v| content(v, decor.vertical()));
    let stretch = (width - margin.horizontal() - decor.horizontal()).max(0.0);
    let bounds = Bounds {
        min_width: length(style, MinWidth, basis)
            .map(|v| content(v, decor.horizontal()))
            .unwrap_or(0.0),
        max_width: length(style, MaxWidth, basis)
            .map(|v| content(v, decor.horizontal()))
            .unwrap_or(f64::INFINITY),
        min_height: length(style, MinHeight, height)
            .map(|v| content(v, decor.vertical()))
            .unwrap_or(0.0),
        max_height: length(style, MaxHeight, height)
            .map(|v| content(v, decor.vertical()))
            .unwrap_or(f64::INFINITY),
    };
    let mut auto_height = None;
    let (used_width, used_height) = if b.kind == BoxKind::Image {
        let element = styled
            .document()
            .node(b.node)
            .and_then(|n| n.element())
            .ok_or(Error::Invalid)?;
        let attribute = |name| {
            element
                .attribute(name)
                .and_then(|v| v.trim().parse::<f64>().ok())
                .filter(|n| n.is_finite() && (0.0..=MAX_VIEWPORT).contains(n))
        };
        let attributes = (attribute("width"), attribute("height"));
        let (w, h) = replaced(
            (specified_width, specified_height),
            attributes,
            intrinsic,
            stretch,
            bounds,
        );
        if specified_width.is_some() {
            let auto = replaced(
                (specified_width, None),
                attributes,
                intrinsic,
                stretch,
                bounds,
            );
            auto_height = Some(auto.1);
        }
        (w, Some(h))
    } else {
        (
            bounds.width(specified_width.unwrap_or(stretch)),
            specified_height.map(|h| bounds.height(h)),
        )
    };
    let Bounds {
        min_height,
        max_height,
        ..
    } = bounds;
    let block = b.kind == BoxKind::Block
        || (b.kind == BoxKind::Image && !inline_picture(keyword(style, Display)));
    if block {
        let free = width - used_width - decor.horizontal() - margin.horizontal();
        let auto_left = length(style, MarginLeft, Some(width)).is_none();
        let auto_right = length(style, MarginRight, Some(width)).is_none();
        if free > 0.0 && (auto_left || auto_right) {
            if auto_left {
                margin.left = if auto_right { free / 2.0 } else { free };
            }
            if auto_right {
                margin.right = if auto_left { free / 2.0 } else { free };
            }
        } else {
            margin.right += free;
        }
    }
    if !coordinate(used_width) || used_height.is_some_and(|h| !coordinate(h)) {
        return Err(Error::Limit("MAX_COORDINATE"));
    }
    let relative = if keyword(style, Position) == "relative" {
        (
            length(style, Left, Some(width))
                .or_else(|| length(style, Right, Some(width)).map(|v| -v))
                .unwrap_or(0.0),
            length(style, Top, height)
                .or_else(|| length(style, Bottom, height).map(|v| -v))
                .unwrap_or(0.0),
        )
    } else {
        (0.0, 0.0)
    };
    Ok(Dimensions {
        width: used_width,
        height: used_height,
        margin,
        padding,
        border,
        min_height,
        max_height,
        relative,
        auto_height,
    })
}
#[derive(Clone, Copy)]
struct Bounds {
    min_width: f64,
    max_width: f64,
    min_height: f64,
    max_height: f64,
}
impl Bounds {
    // A minimum wins over a smaller maximum (CSS 2.2 §10.4, §10.7).
    fn width(self, w: f64) -> f64 {
        w.min(self.max_width).max(self.min_width)
    }
    fn height(self, h: f64) -> f64 {
        h.min(self.max_height).max(self.min_height)
    }
}
/// A replaced element's used content size (DESIGN.md §5): CSS 2.2
/// §10.3.2 and §10.6.2, with the attributes between the CSS value and the
/// intrinsic size, and with a ratio §10.4's table.
fn replaced(
    (css_width, css_height): (Option<f64>, Option<f64>),
    (attr_width, attr_height): (Option<f64>, Option<f64>),
    intrinsic: Option<IntrinsicSize>,
    stretch: f64,
    bounds: Bounds,
) -> (f64, f64) {
    let natural = intrinsic.unwrap_or_default();
    let px = |v: Option<u32>| v.map(|v| (f64::from(v) / 64.0).min(MAX_VIEWPORT));
    let (width, height) = (px(natural.width), px(natural.height));
    // The natural ratio, else the attributes' (HTML maps them to
    // `aspect-ratio: auto w / h`). Finite and positive, so a finite size
    // through it is never NaN, and bound() makes it finite.
    let ratio = natural
        .ratio
        .filter(|(w, h)| *w > 0 && *h > 0)
        .map(|(w, h)| f64::from(w) / f64::from(h))
        .or_else(|| attr_width.zip(attr_height).map(|(w, h)| w / h))
        .filter(|r| r.is_finite() && *r > 0.0);
    match (css_width, css_height) {
        (Some(w), Some(h)) => (bounds.width(w), bounds.height(h)),
        (Some(css), None) => {
            let w = bounds.width(css);
            let h = match ratio {
                Some(r) => w / r,
                None => attr_height.or(height).unwrap_or(150.0),
            };
            (w, bound(bounds.height(h)))
        }
        (None, Some(css)) => {
            let h = bounds.height(css);
            let w = match ratio {
                Some(r) => h * r,
                None => attr_width.or(width).unwrap_or(300.0),
            };
            (bound(bounds.width(w)), h)
        }
        (None, None) => {
            let by_height = |h: Option<f64>| ratio.zip(h).map(|(r, h)| h * r);
            let w = attr_width
                .or_else(|| by_height(attr_height))
                .or(width)
                .or_else(|| by_height(height))
                // CSS 2.2 §10.3.2's suggestion for a ratio alone.
                .or(ratio.map(|_| stretch))
                .unwrap_or(300.0);
            let by_width = |w: Option<f64>| ratio.zip(w).map(|(r, w)| w / r);
            let h = attr_height
                .or_else(|| by_width(attr_width))
                .or(height)
                .or_else(|| by_width(Some(w)))
                .unwrap_or(150.0);
            let (w, h) = match ratio {
                // §10.4 scales the tentative size by its own ratio, which
                // is the given one unless two attributes disagree with a
                // picture; a zero side has none of its own.
                Some(r) => {
                    let own = Some(w / h).filter(|q| w > 0.0 && q.is_finite() && *q > 0.0);
                    constrain(w, h, own.unwrap_or(r), bounds)
                }
                None => (w, h),
            };
            (bound(bounds.width(w)), bound(bounds.height(h)))
        }
    }
}
fn bound(v: f64) -> f64 {
    v.clamp(0.0, MAX_VIEWPORT)
}
/// CSS 2.2 §10.4's table for auto width and height with a ratio, before
/// the caller clamps both sides to their min and max, which stands for
/// the table's own max() and min() of the side it derives. Each row fixes
/// one side and derives the other through the ratio. A double violation
/// picks its side by the ratio, as the table does when w/h is the ratio,
/// so a tentative size with a zero side, whose ratio is the given one,
/// still meets its bounds.
fn constrain(w: f64, h: f64, ratio: f64, bounds: Bounds) -> (f64, f64) {
    let Bounds {
        min_width,
        min_height,
        ..
    } = bounds;
    let max_width = bounds.max_width.max(min_width);
    let max_height = bounds.max_height.max(min_height);
    let by_width = |w: f64| (w, w / ratio);
    let by_height = |h: f64| (h * ratio, h);
    let (over_w, under_w) = (w > max_width, w < min_width);
    let (over_h, under_h) = (h > max_height, h < min_height);
    if over_w && over_h {
        if max_width / ratio <= max_height {
            by_width(max_width)
        } else {
            by_height(max_height)
        }
    } else if under_w && under_h {
        if min_height * ratio >= min_width {
            by_height(min_height)
        } else {
            by_width(min_width)
        }
    } else if under_w && over_h {
        (min_width, max_height)
    } else if over_w && under_h {
        (max_width, min_height)
    } else if over_w {
        by_width(max_width)
    } else if under_w {
        by_width(min_width)
    } else if over_h {
        by_height(max_height)
    } else if under_h {
        by_height(min_height)
    } else {
        (w, h)
    }
}
fn lookup(request: &Request<'_>, b: &BoxNode) -> Option<IntrinsicSize> {
    if b.kind != BoxKind::Image {
        return None;
    }
    request.intrinsic.and_then(|s| s.intrinsic(b.node, b.style))
}
fn collapse(a: f64, b: f64) -> f64 {
    a.max(b).max(0.0) + a.min(b).min(0.0)
}
fn alignment(align: &str, free: f64) -> f64 {
    match align {
        "right" | "end" => free,
        "center" => free / 2.0,
        _ => 0.0,
    }
}

fn include_bounds(
    left: &mut f64,
    top: &mut f64,
    right: &mut f64,
    bottom: &mut f64,
    rect: Rect,
) -> Result<(), Error> {
    for n in [
        rect.x,
        rect.y,
        rect.width,
        rect.height,
        rect.x + rect.width,
        rect.y + rect.height,
    ] {
        if !coordinate(n) {
            return Err(Error::Limit("MAX_COORDINATE"));
        }
    }
    *left = left.min((rect.x * 64.0).floor() / 64.0);
    *top = top.min((rect.y * 64.0).floor() / 64.0);
    *right = right.max(((rect.x + rect.width) * 64.0).ceil() / 64.0);
    *bottom = bottom.max(((rect.y + rect.height) * 64.0).ceil() / 64.0);
    if !coordinate(*right - *left) || !coordinate(*bottom - *top) {
        return Err(Error::Limit("MAX_COORDINATE"));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::inline::FontLine;
    use crate::Limits;
    use td_css::{Origin, StyleSet, HTML_UA_CSS};
    struct Metrics;
    impl FontMetrics for Metrics {
        fn advance(&mut self, _: &ComputedStyle, _: char) -> u32 {
            8 * 64
        }
        fn line(&mut self, s: &ComputedStyle) -> FontLine {
            FontLine {
                height: (s.font_size() * 80.0) as u32,
                baseline: (s.font_size() * 60.0) as u32,
            }
        }
    }
    fn sheets(css: &str) -> StyleSet {
        StyleSet::new(&[
            (Origin::UserAgent, HTML_UA_CSS),
            (Origin::Author, "*{margin:0;padding:0;border-width:0}"),
            (Origin::Author, css),
        ])
        .unwrap()
    }
    fn find<'a>(doc: &td_html::document::Document, out: &'a Layout, id: &str) -> &'a BoxGeometry {
        out.boxes
            .iter()
            .find(|g| {
                doc.node(g.node)
                    .and_then(|n| n.element())
                    .and_then(|e| e.attribute("id"))
                    == Some(id)
            })
            .unwrap()
    }
    #[test]
    fn widths_box_sizing_and_percentage_edges() {
        let html = "<div id=outer><p id=inner>x</p></div>";
        let doc = td_html::document::parse(html).unwrap();
        let css=sheets("#outer{width:200px;padding:10px;border:2px solid;margin:auto}#inner{width:50%;padding:10%}");
        let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
        let out = layout(&styled, &mut Metrics, &Request::new(400.0, 300.0)).unwrap();
        let outer = find(&doc, &out, "outer");
        let inner = find(&doc, &out, "inner");
        assert_eq!(outer.content.x, 100.0);
        assert_eq!(outer.border.width, 224.0);
        assert_eq!(outer.margin.left, 88.0);
        assert_eq!(outer.margin.right, 88.0);
        assert_eq!(inner.content.width, 100.0);
        assert_eq!(inner.border.width, 140.0);
        assert_eq!(inner.content.x, 120.0);
        let css = sheets("#outer{width:200px;padding:10px;border:2px solid;box-sizing:border-box}");
        let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
        let out = layout(&styled, &mut Metrics, &Request::new(400.0, 300.0)).unwrap();
        assert_eq!(find(&doc, &out, "outer").content.width, 176.0);
    }
    #[test]
    fn sibling_margin_collapse_and_relative_flow_space() {
        let html = "<div id=a></div> \n <div id=b></div>";
        let doc = td_html::document::parse(html).unwrap();
        for (bottom, top, want) in [(20, 30, 40.0), (20, -5, 25.0), (-20, -30, -20.0)] {
            let css = sheets(&format!(
                "div{{height:10px}}#a{{margin-bottom:{bottom}px}}#b{{margin-top:{top}px}}"
            ));
            let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
            let out = layout(&styled, &mut Metrics, &Request::new(100.0, 100.0)).unwrap();
            assert_eq!(find(&doc, &out, "b").border.y, want);
        }
        let css = sheets("div{height:20px}#a{position:relative;left:5px;top:10px}");
        let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
        let out = layout(&styled, &mut Metrics, &Request::new(100.0, 100.0)).unwrap();
        assert_eq!(find(&doc, &out, "a").border.y, 10.0);
        assert_eq!(find(&doc, &out, "a").border.x, 5.0);
        assert_eq!(find(&doc, &out, "b").border.y, 20.0);
    }
    #[test]
    fn definite_height_percentages_and_min_max() {
        let html = "<div id=outer><div id=inner>x</div></div>";
        let doc = td_html::document::parse(html).unwrap();
        for (outer_height, want) in [("200px", 100.0), ("auto", 20.0)] {
            let css=sheets(&format!("#outer{{height:{outer_height}}}#inner{{height:50%;width:90%;max-width:60px;min-width:40px}}"));
            let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
            let out = layout(&styled, &mut Metrics, &Request::new(100.0, 300.0)).unwrap();
            let inner = find(&doc, &out, "inner");
            assert_eq!(inner.content.height, want);
            assert_eq!(inner.content.width, 60.0);
        }
        let css = sheets(
            "#outer{width:200px;height:200px}#inner{width:50px;position:relative;left:50%;top:50%}",
        );
        let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
        let out = layout(&styled, &mut Metrics, &Request::new(300.0, 300.0)).unwrap();
        assert_eq!(find(&doc, &out, "inner").content.x, 100.0);
        assert_eq!(find(&doc, &out, "inner").content.y, 100.0);
    }
    #[test]
    fn inline_images_have_attribute_placeholders_and_source_ids() {
        for (attributes, css, w, h) in [
            ("", "", 300.0, 150.0),
            ("width=40 height=20", "", 40.0, 20.0),
            ("width=40 height=20", "img{width:80px}", 80.0, 40.0),
            ("width=40", "", 40.0, 150.0),
            ("width=-2 height=nan", "", 300.0, 150.0),
        ] {
            let html = format!("<p>a<img {attributes}>b");
            let doc = td_html::document::parse(&html).unwrap();
            let styled =
                StyledDocument::new(&doc, &sheets(css), html.len(), Limits::default()).unwrap();
            let out = layout(&styled, &mut Metrics, &Request::new(500.0, 300.0)).unwrap();
            assert_eq!(out.lines.len(), 1);
            let image = &out.lines[0].replaced[0];
            assert_eq!((image.width, image.height), (w, h));
            assert_eq!(image.x, 8.0);
            assert_eq!(doc.node(image.node).unwrap().element().unwrap().name, "img");
        }
    }
    const PX: u32 = 64;
    /// The first image's size at a 500 px width: an inline image's border
    /// box, a block image's content box.
    fn image_size(
        html: &str,
        css: &str,
        sizes: Option<&(dyn IntrinsicSizes + Sync)>,
    ) -> (f64, f64) {
        let doc = td_html::document::parse(html).unwrap();
        let styled =
            StyledDocument::new(&doc, &sheets(css), html.len(), Limits::default()).unwrap();
        let request = Request {
            intrinsic: sizes,
            ..Request::new(500.0, 300.0)
        };
        let out = layout(&styled, &mut Metrics, &request).unwrap();
        if let Some(image) = out.lines.iter().find_map(|l| l.replaced.first()) {
            return (image.width, image.height);
        }
        let image = out.boxes.iter().find(|g| g.replaced).unwrap();
        (image.content.width, image.content.height)
    }
    fn every(size: IntrinsicSize) -> impl Fn(NodeId, StyleId) -> Option<IntrinsicSize> {
        move |_, _| Some(size)
    }
    #[test]
    fn intrinsic_sizes_follow_css_2_2_replaced_sizing() {
        let picture = every(IntrinsicSize::picture(40 * PX, 20 * PX));
        let width = every(IntrinsicSize {
            width: Some(40 * PX),
            ..IntrinsicSize::default()
        });
        let height = every(IntrinsicSize {
            height: Some(20 * PX),
            ratio: Some((2, 1)),
            ..IntrinsicSize::default()
        });
        let ratio = every(IntrinsicSize {
            ratio: Some((4, 1)),
            ..IntrinsicSize::default()
        });
        let text = every(IntrinsicSize {
            width: Some(40 * PX),
            height: Some(20 * PX),
            ratio: None,
        });
        for (sizes, attributes, css, want) in [
            // Auto width and height take the intrinsic size.
            (&picture, "", "", (40.0, 20.0)),
            (&picture, "", "img{display:block}", (40.0, 20.0)),
            (&width, "", "", (40.0, 150.0)),
            (&height, "", "", (40.0, 20.0)),
            // A ratio alone stretches to the containing width (§10.3.2).
            (&ratio, "", "img{display:block}", (500.0, 125.0)),
            (
                &ratio,
                "",
                "p{margin:0 20px}img{display:block;padding:5px}",
                (450.0, 112.5),
            ),
            // One CSS dimension: the ratio gives the other.
            (&picture, "", "img{width:80px}", (80.0, 40.0)),
            (&picture, "", "img{height:10px}", (20.0, 10.0)),
            (&ratio, "", "img{height:10px}", (40.0, 10.0)),
            (&text, "", "img{width:80px}", (80.0, 20.0)),
            // A natural ratio comes before the attributes' (HTML's
            // `aspect-ratio: auto w / h`).
            (
                &picture,
                "width=30 height=30",
                "img{width:80px}",
                (80.0, 40.0),
            ),
            // Attributes are dimensions over the intrinsic size,
            (&picture, "width=30 height=30", "", (30.0, 30.0)),
            (&picture, "width=30", "", (30.0, 15.0)),
            (&picture, "height=30", "", (60.0, 30.0)),
            (&width, "height=30", "", (40.0, 30.0)),
            (&text, "width=30", "", (30.0, 20.0)),
            // and CSS over attributes.
            (
                &picture,
                "width=30 height=30",
                "img{width:8px;height:9px}",
                (8.0, 9.0),
            ),
        ] {
            let html = format!("<p>a<img {attributes}>b");
            assert_eq!(
                image_size(&html, css, Some(sizes)),
                want,
                "{attributes} {css}"
            );
        }
    }
    #[test]
    fn intrinsic_ratios_hold_through_min_and_max_sizes() {
        let picture = every(IntrinsicSize::picture(400 * PX, 200 * PX));
        for (css, want) in [
            ("img{max-width:100px}", (100.0, 50.0)),
            ("img{max-height:100px}", (200.0, 100.0)),
            ("img{min-width:800px}", (800.0, 400.0)),
            ("img{min-height:300px}", (600.0, 300.0)),
            ("img{min-width:600px;max-height:250px}", (600.0, 250.0)),
            ("img{max-width:100px;min-height:300px}", (100.0, 300.0)),
            ("img{max-width:200px;max-height:50px}", (100.0, 50.0)),
            ("img{max-width:100px;max-height:150px}", (100.0, 50.0)),
            ("img{min-width:800px;min-height:500px}", (1000.0, 500.0)),
            ("img{min-width:1200px;min-height:500px}", (1200.0, 600.0)),
            ("img{max-width:100px;min-height:60px}", (100.0, 60.0)),
            // A minimum beats a smaller maximum.
            ("img{min-width:150px;max-width:100px}", (150.0, 75.0)),
            // A percentage of the containing width, as responsive pages set.
            ("img{max-width:50%}", (250.0, 125.0)),
            // With one dimension set, the other follows its used value.
            ("img{width:100px;max-height:20px}", (100.0, 20.0)),
            ("img{height:100px;max-width:120px}", (120.0, 100.0)),
            ("img{width:300px;max-width:100px}", (100.0, 50.0)),
            (
                "img{display:block;box-sizing:border-box;padding:10px;max-width:120px}",
                (100.0, 50.0),
            ),
            // Each row of §10.4's table, and the bound on the side a row
            // derives.
            ("img{min-width:600px;max-height:100px}", (600.0, 100.0)),
            (
                "img{max-width:100px;max-height:150px;min-height:120px}",
                (100.0, 120.0),
            ),
            (
                "img{max-width:500px;max-height:50px;min-width:150px}",
                (150.0, 50.0),
            ),
            (
                "img{min-width:800px;min-height:500px;max-width:900px}",
                (900.0, 500.0),
            ),
            (
                "img{min-width:1200px;min-height:500px;max-height:550px}",
                (1200.0, 550.0),
            ),
            ("img{max-height:50px;min-width:150px}", (150.0, 50.0)),
            ("img{min-height:300px;max-width:500px}", (500.0, 300.0)),
            ("img{max-width:100px;min-height:30px}", (100.0, 50.0)),
            ("img{min-width:800px;max-height:300px}", (800.0, 300.0)),
        ] {
            assert_eq!(image_size("<p><img>", css, Some(&picture)), want, "{css}");
        }
        // The attributes' ratio, without a natural one, holds as well.
        assert_eq!(
            image_size(
                "<p><img width=1200 height=800>",
                "img{max-width:100%}",
                None
            ),
            (500.0, 1000.0 / 3.0)
        );
        // A stand-in without a ratio is clamped a dimension at a time.
        let text = every(IntrinsicSize {
            width: Some(400 * PX),
            height: Some(200 * PX),
            ratio: None,
        });
        assert_eq!(
            image_size("<p><img>", "img{max-width:100px}", Some(&text)),
            (100.0, 200.0)
        );
    }
    #[test]
    fn hostile_intrinsic_sizes_are_clamped() {
        let huge = every(IntrinsicSize::picture(u32::MAX, u32::MAX));
        assert_eq!(
            image_size("<img>", "img{display:block}", Some(&huge)),
            (MAX_VIEWPORT, MAX_VIEWPORT)
        );
        let tall = every(IntrinsicSize {
            width: Some(PX),
            ratio: Some((1, u32::MAX)),
            ..IntrinsicSize::default()
        });
        assert_eq!(
            image_size("<img>", "img{display:block}", Some(&tall)),
            (1.0, MAX_VIEWPORT)
        );
        let wide = every(IntrinsicSize {
            ratio: Some((u32::MAX, 1)),
            ..IntrinsicSize::default()
        });
        for css in ["img{height:1000px}", "img{min-height:1000px}"] {
            assert_eq!(
                image_size("<p><img>", css, Some(&wide)),
                (MAX_VIEWPORT, 1000.0),
                "{css}"
            );
        }
        // Nothing, as td-web sizes an image without alt text.
        let nothing = every(IntrinsicSize::picture(0, 0));
        assert_eq!(image_size("<p><img>", "", Some(&nothing)), (0.0, 0.0));
        assert_eq!(
            image_size("<p><img>", "img{min-width:10px}", Some(&nothing)),
            (10.0, 0.0)
        );
        // A ratio with a zero term is none.
        for zero in [(0, 5), (5, 0)] {
            let degenerate = every(IntrinsicSize {
                ratio: Some(zero),
                ..IntrinsicSize::default()
            });
            assert_eq!(
                image_size("<p><img>", "img{width:80px}", Some(&degenerate)),
                (80.0, 150.0)
            );
        }
        let flat = every(IntrinsicSize {
            width: Some(0),
            ratio: Some((2, 1)),
            ..IntrinsicSize::default()
        });
        assert_eq!(
            image_size(
                "<p><img>",
                "img{min-width:10px;min-height:10px}",
                Some(&flat)
            ),
            (20.0, 10.0)
        );
    }
    #[test]
    fn tentative_sizes_off_the_ratio_or_zero_still_meet_their_bounds() {
        let cases: &[(&str, &str, IntrinsicSize, (f64, f64))] = &[
            // Two attributes off a picture's ratio, under two bounds,
            // keep the attributes' proportions, continuous and monotone
            // as a bound tightens.
            (
                "<p><img width=300 height=300>",
                "img{max-width:100%;max-height:300px}",
                IntrinsicSize::picture_px(1600, 400),
                (300.0, 300.0),
            ),
            (
                "<p><img width=300 height=300>",
                "img{max-width:100%;max-height:299px}",
                IntrinsicSize::picture_px(1600, 400),
                (299.0, 299.0),
            ),
            (
                "<p><img width=300 height=300>",
                "img{max-width:100%;max-height:200px}",
                IntrinsicSize::picture_px(1600, 400),
                (200.0, 200.0),
            ),
            (
                "<p><img width=30 height=30>",
                "img{min-width:40px;min-height:50px}",
                IntrinsicSize::picture_px(40, 80),
                (50.0, 50.0),
            ),
            // One attribute takes the picture's ratio.
            (
                "<p><img width=300>",
                "img{max-width:100%;max-height:50px}",
                IntrinsicSize::picture_px(1600, 400),
                (200.0, 50.0),
            ),
            (
                "<p><img width=300 height=10>",
                "img{max-width:200px;min-height:20px}",
                IntrinsicSize::picture_px(1, 1),
                (200.0, 20.0),
            ),
            (
                "<p><img width=10 height=300>",
                "img{min-width:200px;max-height:50px}",
                IntrinsicSize::picture_px(1000, 100),
                (200.0, 50.0),
            ),
            // A zero side, in both orientations, and the rows crossing a
            // minimum and a maximum, which a zero side alone reaches
            // beyond the final clamp.
            (
                "<p><img width=0>",
                "img{min-width:10px;min-height:10px}",
                IntrinsicSize::picture_px(40, 80),
                (10.0, 20.0),
            ),
            (
                "<p><img height=0>",
                "img{min-width:10px;min-height:10px}",
                IntrinsicSize::picture_px(80, 40),
                (20.0, 10.0),
            ),
            (
                "<p><img width=0 height=300>",
                "img{min-width:200px;max-height:50px}",
                IntrinsicSize::picture_px(1000, 100),
                (200.0, 50.0),
            ),
            (
                "<p><img width=300 height=0>",
                "img{max-width:200px;min-height:50px}",
                IntrinsicSize::picture_px(100, 1000),
                (200.0, 50.0),
            ),
            // A ratio alone with nothing to stretch over.
            (
                "<img>",
                "img{display:block;margin-left:500px;min-width:333px;min-height:5px}",
                IntrinsicSize {
                    ratio: Some((2, 9)),
                    ..IntrinsicSize::default()
                },
                (333.0, 1498.5),
            ),
            // A tentative size past MAX_VIEWPORT picks its row unclamped.
            (
                "<p><img height=900000>",
                "img{max-width:600px;max-height:500px}",
                IntrinsicSize {
                    ratio: Some((2, 1)),
                    ..IntrinsicSize::default()
                },
                (600.0, 300.0),
            ),
        ];
        for (html, css, size, want) in cases {
            let sizes = every(*size);
            assert_eq!(image_size(html, css, Some(&sizes)), *want, "{html} {css}");
        }
    }
    #[test]
    fn hostile_attribute_ratios_cost_only_their_box() {
        for (attributes, css, want) in [
            (
                "width=1000000 height=0.000001",
                "img{height:1000px}",
                (MAX_VIEWPORT, 1000.0),
            ),
            (
                "width=1000000 height=0.000001",
                "img{max-width:10px}",
                (10.0, 0.00000000001),
            ),
            ("width=1 height=1e-310", "img{height:1000px}", (1.0, 1000.0)),
            (
                "width=1e-310 height=1",
                "img{width:1000px}",
                (1000.0, MAX_VIEWPORT),
            ),
            ("width=1e-310 height=1", "img{height:1000px}", (0.0, 1000.0)),
        ] {
            let html = format!("<p><img {attributes}>x");
            let (w, h) = image_size(&html, css, None);
            assert!(w.is_finite() && h.is_finite(), "{attributes} {css}");
            assert!(
                (w - want.0).abs() < 1e-9 && (h - want.1).abs() < 1e-9,
                "{attributes} {css}: {w} {h}"
            );
        }
    }
    #[test]
    fn without_intrinsic_sizes_the_placeholder_rule_is_unchanged() {
        // The rule before intrinsic sizes, kept as the oracle where the
        // attributes give no ratio or no bound applies.
        let placeholder = |(cw, ch): (Option<f64>, Option<f64>),
                           (aw, ah): (Option<f64>, Option<f64>),
                           b: Bounds| {
            let ratio = aw.zip(ah).filter(|(w, h)| *w > 0.0 && *h > 0.0);
            let ratio = ratio.map(|(w, h)| w / h);
            let w = cw.unwrap_or_else(|| ch.zip(ratio).map_or(aw.unwrap_or(300.0), |(h, r)| h * r));
            let h = ch.unwrap_or_else(|| cw.zip(ratio).map_or(ah.unwrap_or(150.0), |(w, r)| w / r));
            (
                w.min(b.max_width).max(b.min_width),
                h.min(b.max_height).max(b.min_height),
            )
        };
        let bounds = |min_width, max_width, min_height, max_height| Bounds {
            min_width,
            max_width,
            min_height,
            max_height,
        };
        let unbounded = f64::INFINITY;
        let bounded = |b: Bounds| {
            b.min_width > 0.0
                || b.max_width.is_finite()
                || b.min_height > 0.0
                || b.max_height.is_finite()
        };
        // Expected values worked by hand: the attributes' ratio through
        // §10.4's rows, or through the one CSS dimension set.
        let some = Some;
        for (css, attributes, b, want) in [
            (
                (None, None),
                (some(80.0), some(40.0)),
                bounds(0.0, 30.0, 0.0, unbounded),
                (30.0, 15.0),
            ),
            (
                (None, None),
                (some(80.0), some(40.0)),
                bounds(100.0, unbounded, 0.0, unbounded),
                (100.0, 50.0),
            ),
            (
                (None, None),
                (some(80.0), some(40.0)),
                bounds(0.0, unbounded, 0.0, 10.0),
                (20.0, 10.0),
            ),
            (
                (None, None),
                (some(80.0), some(40.0)),
                bounds(0.0, unbounded, 200.0, unbounded),
                (400.0, 200.0),
            ),
            (
                (None, None),
                (some(8.5), some(80.0)),
                bounds(10.0, 35.0, 12.0, 18.0),
                (10.0, 18.0),
            ),
            (
                (None, None),
                (some(80.0), some(8.5)),
                bounds(10.0, 35.0, 12.0, 18.0),
                (35.0, 12.0),
            ),
            (
                (None, None),
                (some(80.0), some(40.0)),
                bounds(0.0, 50.0, 0.0, 20.0),
                (40.0, 20.0),
            ),
            (
                (some(8.5), None),
                (some(80.0), some(40.0)),
                bounds(0.0, unbounded, 0.0, 3.0),
                (8.5, 3.0),
            ),
            (
                (None, some(80.0)),
                (some(80.0), some(40.0)),
                bounds(0.0, 100.0, 0.0, unbounded),
                (100.0, 80.0),
            ),
            (
                (some(80.0), None),
                (some(80.0), some(40.0)),
                bounds(0.0, 30.0, 0.0, unbounded),
                (30.0, 15.0),
            ),
        ] {
            assert_eq!(
                replaced(css, attributes, None, 400.0, b),
                want,
                "{css:?} {attributes:?}"
            );
        }
        let all = [
            bounds(0.0, unbounded, 0.0, unbounded),
            bounds(0.0, 30.0, 0.0, unbounded),
            bounds(30.0, unbounded, 0.0, unbounded),
            bounds(0.0, unbounded, 200.0, unbounded),
            bounds(0.0, unbounded, 0.0, 6.0),
            bounds(100.0, 50.0, 0.0, 5.0),
            bounds(10.0, 35.0, 12.0, 18.0),
        ];
        let lengths = [None, Some(0.0), Some(8.5), Some(80.0)];
        for cw in lengths {
            for ch in lengths {
                for aw in lengths {
                    for ah in lengths {
                        let ratio = aw.zip(ah).filter(|(w, h)| *w > 0.0 && *h > 0.0);
                        for b in all {
                            // With a ratio and a bound, the attributes
                            // lay out as a picture of their size would.
                            let want = match ratio {
                                Some((w, h)) if bounded(b) => {
                                    let picture = IntrinsicSize::picture(
                                        (w * 64.0) as u32,
                                        (h * 64.0) as u32,
                                    );
                                    replaced((cw, ch), (None, None), Some(picture), 400.0, b)
                                }
                                _ => placeholder((cw, ch), (aw, ah), b),
                            };
                            for none in [None, Some(IntrinsicSize::default())] {
                                assert_eq!(
                                    replaced((cw, ch), (aw, ah), none, 400.0, b),
                                    want,
                                    "{cw:?} {ch:?} {aw:?} {ah:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn the_lookup_is_given_the_image_and_its_own_style() {
        let html = "<p>a<img id=x style='font-size:30px'>b";
        let doc = td_html::document::parse(html).unwrap();
        let styled = StyledDocument::new(&doc, &sheets(""), html.len(), Limits::default()).unwrap();
        let image = styled
            .boxes()
            .iter()
            .find(|b| b.kind == crate::BoxKind::Image)
            .unwrap();
        let seen = std::sync::Mutex::new(None);
        let sizes = |node: NodeId, style: StyleId| {
            *seen.lock().unwrap() = Some((node, style));
            let size = styled.style(style)?.font_size() as u32 * PX;
            Some(IntrinsicSize {
                width: Some(size),
                height: Some(size),
                ratio: None,
            })
        };
        let request = Request {
            intrinsic: Some(&sizes),
            ..Request::new(500.0, 300.0)
        };
        let out = layout(&styled, &mut Metrics, &request).unwrap();
        assert_eq!(*seen.lock().unwrap(), Some((image.node, image.style)));
        let fragment = &out.lines[0].replaced[0];
        assert_eq!((fragment.width, fragment.height), (30.0, 30.0));
        fn shareable<T: Send + Sync>(_: &T) {}
        shareable(&request);
    }
    #[test]
    fn none_supplied_lays_out_as_without_sizes() {
        let none = |_: NodeId, _: StyleId| None::<IntrinsicSize>;
        for (html, css) in [
            ("<p>a<img>b<img width=40 height=20> c", ""),
            ("<p>a<img width=40>b<img height=7>", "img{max-width:20px}"),
            (
                "<img width=40 height=20><p>x",
                "img{display:block;height:10px}",
            ),
            (
                "<p><img width=400 height=200>",
                "img{min-width:600px;max-height:50px}",
            ),
            ("<p><img width=4 height=2 style='width:9px'>", ""),
        ] {
            let doc = td_html::document::parse(html).unwrap();
            let styled =
                StyledDocument::new(&doc, &sheets(css), html.len(), Limits::default()).unwrap();
            let without = layout(&styled, &mut Metrics, &Request::new(300.0, 200.0)).unwrap();
            let request = Request {
                intrinsic: Some(&none),
                ..Request::new(300.0, 200.0)
            };
            let with = layout(&styled, &mut Metrics, &request).unwrap();
            assert_eq!(with.boxes, without.boxes, "{html}");
            assert_eq!(
                with.lines.iter().collect::<Vec<_>>(),
                without.lines.iter().collect::<Vec<_>>()
            );
            assert_eq!(with.page_bytes, without.page_bytes);
        }
    }
    #[test]
    fn a_changed_intrinsic_size_needs_only_a_relayout() {
        let html = "<p>alpha <img id=a> beta <img id=b width=10> gamma\
                    <img id=c style='display:block'>";
        let doc = td_html::document::parse(html).unwrap();
        let css = sheets("img{max-width:100%}");
        let asked = std::sync::atomic::AtomicUsize::new(0);
        let by_id = |sizes: [IntrinsicSize; 3]| {
            let (doc, asked) = (&doc, &asked);
            move |node: NodeId, _: StyleId| {
                asked.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let id = doc.node(node)?.element()?.attribute("id")?;
                let at = ["a", "b", "c"].iter().position(|n| *n == id)?;
                sizes.get(at).copied()
            }
        };
        let first = by_id([
            IntrinsicSize::default(),
            IntrinsicSize::picture(20 * PX, 40 * PX),
            IntrinsicSize::picture(400 * PX, 100 * PX),
        ]);
        let second = by_id([
            IntrinsicSize::picture(30 * PX, 10 * PX),
            IntrinsicSize::default(),
            IntrinsicSize::picture(800 * PX, 100 * PX),
        ]);
        let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
        let before = Request {
            intrinsic: Some(&first),
            ..Request::new(120.0, 300.0)
        };
        let before = layout(&styled, &mut Metrics, &before).unwrap();
        // Each image's size is asked once a layout, and nothing else's.
        assert_eq!(asked.load(std::sync::atomic::Ordering::Relaxed), 3);
        let after = Request {
            intrinsic: Some(&second),
            ..Request::new(120.0, 300.0)
        };
        let relaid = layout(&styled, &mut Metrics, &after).unwrap();
        let again = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
        let fresh = layout(&again, &mut Metrics, &after).unwrap();
        assert_eq!(relaid.boxes, fresh.boxes);
        assert_eq!(
            relaid.lines.iter().collect::<Vec<_>>(),
            fresh.lines.iter().collect::<Vec<_>>()
        );
        assert_eq!(relaid.height, fresh.height);
        assert_eq!(relaid.overflow, fresh.overflow);
        assert_ne!(
            relaid.lines.iter().collect::<Vec<_>>(),
            before.lines.iter().collect::<Vec<_>>()
        );
        let block = |out: &Layout| {
            let c = find(&doc, out, "c").content;
            (c.width, c.height)
        };
        assert_eq!(block(&before), (120.0, 30.0));
        assert_eq!(block(&relaid), (120.0, 15.0));
    }
    #[test]
    fn floats_shorten_lines_and_inline_block_splits_keep_alignment() {
        let html = "<div id=float></div><span>before<div id=block>x</div>after</span>";
        let doc = td_html::document::parse(html).unwrap();
        let css = sheets(
            "#float{float:left;width:30px;height:20px}#block{height:10px}body{text-align:center}",
        );
        let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
        let out = layout(&styled, &mut Metrics, &Request::new(100.0, 200.0)).unwrap();
        assert_eq!(out.lines.len(), 3);
        let float = find(&doc, &out, "float").border;
        assert_eq!(
            (float.x, float.y, float.width, float.height),
            (0.0, 0.0, 30.0, 20.0)
        );
        // Beside the float, centred in the 70 px it leaves.
        assert_eq!(out.lines[0].y, 0.0);
        assert_eq!(out.lines[0].fragments[0].x, 41.0);
        assert_eq!(find(&doc, &out, "block").border.y, 20.0);
        // Below it, centred in the whole width.
        assert_eq!(out.lines[2].y, 30.0);
        assert_eq!(out.lines[2].fragments[0].x, 30.0);
    }
    #[test]
    fn windowed_output_equals_filtering_full_and_resize_charges_overlap() {
        let html = format!("<p>{}", "hello world ".repeat(500));
        let doc = td_html::document::parse(&html).unwrap();
        let css = sheets("");
        let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
        let request = Request::new(100.0, 200.0);
        let full = layout(&styled, &mut Metrics, &request).unwrap();
        let mut window = request.clone();
        window.window = Some(100.0..200.0);
        let subset = layout(&styled, &mut Metrics, &window).unwrap();
        assert_eq!(
            subset.lines.iter().collect::<Vec<_>>(),
            full.lines
                .iter()
                .filter(|l| l.y + l.height > 100.0 && l.y < 200.0)
                .collect::<Vec<_>>()
        );
        assert_eq!(subset.height, full.height);
        assert_eq!(subset.boxes, full.boxes);
        assert_eq!(subset.line_count, full.line_count);
        assert!(subset.output_bytes < full.output_bytes);
        window.width = 80.0;
        window.retained_output_bytes = full.output_bytes;
        let resized = layout(&styled, &mut Metrics, &window).unwrap();
        assert_eq!(
            resized.page_bytes,
            styled.charges().held() + full.output_bytes + resized.output_bytes
        );
        assert!(resized.line_count > full.line_count);
        window.retained_output_bytes = crate::MAX_PAGE_BYTES;
        assert!(matches!(
            layout(&styled, &mut Metrics, &window),
            Err(Error::Limit("MAX_PAGE_BYTES"))
        ));
    }
    #[test]
    fn a_request_sets_its_own_byte_ceiling() {
        let html = format!("<p>{}", "hello world ".repeat(50));
        let doc = td_html::document::parse(&html).unwrap();
        let css = sheets("");
        let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
        let request = Request::new(100.0, 200.0);
        assert_eq!(request.bytes, None);
        let full = layout(&styled, &mut Metrics, &request).unwrap();
        let at = |bytes| Request {
            bytes: Some(bytes),
            ..request.clone()
        };
        let exact = layout(&styled, &mut Metrics, &at(full.page_bytes)).unwrap();
        assert_eq!(exact.page_bytes, full.page_bytes);
        assert_eq!(
            exact.lines.iter().collect::<Vec<_>>(),
            full.lines.iter().collect::<Vec<_>>()
        );
        assert!(matches!(
            layout(&styled, &mut Metrics, &at(full.page_bytes - 1)),
            Err(Error::Limit("MAX_PAGE_BYTES"))
        ));
        // Output kept from an earlier layout counts against it too.
        let both = full.page_bytes + full.output_bytes;
        let mut kept = at(both);
        kept.retained_output_bytes = full.output_bytes;
        assert_eq!(
            layout(&styled, &mut Metrics, &kept).unwrap().page_bytes,
            both
        );
        kept.bytes = Some(both - 1);
        assert!(matches!(
            layout(&styled, &mut Metrics, &kept),
            Err(Error::Limit("MAX_PAGE_BYTES"))
        ));
        // It replaces the cascade's ceiling, above it as well as below.
        let tight = StyledDocument::new(
            &doc,
            &css,
            html.len(),
            Limits {
                bytes: styled.charges().construction(),
                ..Limits::default()
            },
        )
        .unwrap();
        assert!(full.page_bytes > styled.charges().construction());
        assert!(matches!(
            layout(&tight, &mut Metrics, &request),
            Err(Error::Limit("MAX_PAGE_BYTES"))
        ));
        let raised = layout(&tight, &mut Metrics, &at(full.page_bytes)).unwrap();
        assert_eq!(raised.page_bytes, full.page_bytes);
        // Clamped to MAX_PAGE_BYTES.
        let mut beyond = at(usize::MAX);
        beyond.retained_output_bytes = crate::MAX_PAGE_BYTES - styled.charges().held() + 1;
        assert!(matches!(
            layout(&styled, &mut Metrics, &beyond),
            Err(Error::Limit("MAX_PAGE_BYTES"))
        ));
    }
    #[test]
    fn viewport_and_coordinate_admission_edges() {
        let doc = td_html::document::parse("<p>x").unwrap();
        let styled = StyledDocument::new(&doc, &sheets(""), 4, Limits::default()).unwrap();
        assert!(layout(
            &styled,
            &mut Metrics,
            &Request::new(MAX_VIEWPORT, MAX_VIEWPORT)
        )
        .is_ok());
        for width in [MAX_VIEWPORT + 1.0, -1.0, f64::INFINITY, f64::NAN] {
            assert!(matches!(
                layout(&styled, &mut Metrics, &Request::new(width, 100.0)),
                Err(Error::Limit("MAX_VIEWPORT"))
            ));
        }
        let mut r = Request::new(100.0, 100.0);
        r.window = Some(10.0..0.0);
        assert!(matches!(
            layout(&styled, &mut Metrics, &r),
            Err(Error::Invalid)
        ));
    }
    #[test]
    fn explicit_breaks_and_images_separate_crlf_and_use_break_metrics() {
        for (html, want_lines) in [
            ("<p>a&#13;<br>&#10;b", 4),
            ("<p>a&#13;<img width=1 height=1>&#10;b", 3),
        ] {
            let doc = td_html::document::parse(html).unwrap();
            let styled = StyledDocument::new(
                &doc,
                &sheets("p{white-space:pre-wrap}"),
                html.len(),
                Limits::default(),
            )
            .unwrap();
            let out = layout(&styled, &mut Metrics, &Request::new(300.0, 300.0)).unwrap();
            assert_eq!(out.lines.len(), want_lines, "{html}");
        }
        let html = "<p>x<br style='font-size:100px'>y";
        let doc = td_html::document::parse(html).unwrap();
        let styled = StyledDocument::new(&doc, &sheets(""), html.len(), Limits::default()).unwrap();
        let out = layout(&styled, &mut Metrics, &Request::new(300.0, 300.0)).unwrap();
        assert_eq!(out.lines[0].height, 125.0);
        assert_eq!(out.lines[1].y, 125.0);
        assert_eq!(out.lines[1].height, 20.0);
    }
    #[test]
    fn intrinsic_and_constrained_blocks_center_auto_margins() {
        for (html, css, id, width) in [
            (
                "<img id=x width=40 height=20>",
                "img{display:block;margin:auto}",
                "x",
                40.0,
            ),
            (
                "<div id=x>x</div>",
                "div{max-width:40px;margin:auto}",
                "x",
                40.0,
            ),
        ] {
            let doc = td_html::document::parse(html).unwrap();
            let styled =
                StyledDocument::new(&doc, &sheets(css), html.len(), Limits::default()).unwrap();
            let out = layout(&styled, &mut Metrics, &Request::new(100.0, 100.0)).unwrap();
            let b = find(&doc, &out, id);
            assert_eq!(b.border.width, width);
            assert_eq!(b.border.x, 30.0);
        }
    }

    #[test]
    fn overflowing_alignment_and_image_margins_reach_overflow_bounds() {
        let html = "<p><img width=200 height=20>";
        let doc = td_html::document::parse(html).unwrap();
        for (align, x) in [("center", -50.0), ("right", -100.0)] {
            let css = sheets(&format!("p{{text-align:{align}}}"));
            let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
            let out = layout(&styled, &mut Metrics, &Request::new(100.0, 100.0)).unwrap();
            assert_eq!(out.lines[0].replaced[0].x, x);
            assert_eq!(out.overflow.x, x);
        }
        let styled = StyledDocument::new(
            &doc,
            &sheets("img{margin-left:-20px;margin-top:-30px;margin-bottom:-40px}"),
            html.len(),
            Limits::default(),
        )
        .unwrap();
        let out = layout(&styled, &mut Metrics, &Request::new(300.0, 100.0)).unwrap();
        let image = &out.lines[0].replaced[0];
        assert!(out.overflow.x <= image.x);
        assert!(out.overflow.y <= image.y);
        assert!(out.overflow.y + out.overflow.height >= image.y + image.height);
        let mut request = Request::new(300.0, 100.0);
        request.window = Some(0.0..0.0);
        assert!(layout(&styled, &mut Metrics, &request)
            .unwrap()
            .lines
            .is_empty());
    }

    #[test]
    fn aggregate_overflow_extent_has_an_exact_limit() {
        // A thousand images of MAX_VIEWPORT reach MAX_COORDINATE.
        let html = format!("<div></div>{}", "<img width=1 height=1000000>".repeat(1000));
        let doc = td_html::document::parse(&html).unwrap();
        for (top, ok) in [(0, true), (-1, false)] {
            let css = sheets(&format!(
                "img{{display:block}}div{{height:0;position:relative;top:{top}px}}"
            ));
            let styled = StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
            let result = layout(&styled, &mut Metrics, &Request::new(100.0, 100.0));
            if ok {
                assert!(result.is_ok());
            } else {
                assert!(matches!(result, Err(Error::Limit("MAX_COORDINATE"))));
            }
        }
    }
    #[test]
    fn wrapping_image_drops_all_space_prefix() {
        let html = "<p>    <img width=8 height=10>";
        let doc = td_html::document::parse(html).unwrap();
        let styled = StyledDocument::new(
            &doc,
            &sheets("p{white-space:pre-wrap}"),
            html.len(),
            Limits::default(),
        )
        .unwrap();
        let out = layout(&styled, &mut Metrics, &Request::new(16.0, 100.0)).unwrap();
        assert_eq!(out.lines.len(), 1);
        assert_eq!(out.lines[0].y, 0.0);
    }
    #[test]
    fn image_outer_advance_keeps_negative_margins() {
        for (prefix, margin, want) in [("aaaa", -30, 22.0), ("a", -100, -72.0)] {
            let html = format!("<p>{prefix}<img width=20 height=20>b");
            let doc = td_html::document::parse(&html).unwrap();
            let styled = StyledDocument::new(
                &doc,
                &sheets(&format!("img{{margin-left:{margin}px}}")),
                html.len(),
                Limits::default(),
            )
            .unwrap();
            let out = layout(&styled, &mut Metrics, &Request::new(100.0, 100.0)).unwrap();
            let last = out.lines[0].fragments.last().unwrap();
            assert_eq!(last.x, want);
            assert!(out.overflow.x <= out.lines[0].replaced[0].x);
        }
    }
    #[test]
    fn fractional_image_geometry_stays_inside_overflow() {
        let html = "<p><img width=20 height=20.001>";
        let doc = td_html::document::parse(html).unwrap();
        let styled = StyledDocument::new(&doc, &sheets(""), html.len(), Limits::default()).unwrap();
        let out = layout(&styled, &mut Metrics, &Request::new(100.0, 100.0)).unwrap();
        let image = &out.lines[0].replaced[0];
        assert!(out.overflow.y <= image.y);
        assert!(out.overflow.y + out.overflow.height >= image.y + image.height);
    }
    #[test]
    fn final_margin_is_in_the_coordinate_admission() {
        let html = "<img width=1 height=1000000>".repeat(1000);
        let doc = td_html::document::parse(&html).unwrap();
        for (margin, ok) in [(0, true), (1, false)] {
            let styled = StyledDocument::new(
                &doc,
                &sheets(&format!(
                    "img{{display:block}}html{{margin-bottom:{margin}px}}"
                )),
                html.len(),
                Limits::default(),
            )
            .unwrap();
            let result = layout(&styled, &mut Metrics, &Request::new(100.0, 100.0));
            if ok {
                assert!(result.is_ok());
            } else {
                assert!(matches!(result, Err(Error::Limit("MAX_COORDINATE"))));
            }
        }
    }

    #[test]
    fn signed_fractional_inline_geometry_and_windows_agree() {
        let html = "<p>aaaa <img width=20.001 height=20.001> b cdef ghi";
        let doc = td_html::document::parse(html).unwrap();
        for left in [-40, -5, 0, 5] {
            for right in [-40, 0, 5] {
                for align in ["left", "center", "right"] {
                    let css = sheets(&format!(
                        "p{{text-align:{align}}}img{{margin:{left}px {right}px}}"
                    ));
                    let styled =
                        StyledDocument::new(&doc, &css, html.len(), Limits::default()).unwrap();
                    for width in [8.0, 16.0, 32.0, 80.0] {
                        let mut request = Request::new(width, 100.0);
                        let full = layout(&styled, &mut Metrics, &request).unwrap();
                        for line in &full.lines {
                            for image in &line.replaced {
                                assert!(image.x >= full.overflow.x);
                                assert!(image.y >= full.overflow.y);
                                assert!(
                                    image.x + image.width <= full.overflow.x + full.overflow.width
                                );
                                assert!(
                                    image.y + image.height
                                        <= full.overflow.y + full.overflow.height
                                );
                            }
                            for text in &line.fragments {
                                assert!(text.x >= full.overflow.x);
                                assert!(
                                    text.x + text.width <= full.overflow.x + full.overflow.width
                                );
                            }
                        }
                        request.window = Some(20.0..40.0);
                        let window = layout(&styled, &mut Metrics, &request).unwrap();
                        assert_eq!(full.overflow, window.overflow);
                        assert_eq!(
                            window.lines.iter().collect::<Vec<_>>(),
                            full.lines
                                .iter()
                                .filter(|l| l.y + l.height > 20.0 && l.y < 40.0)
                                .collect::<Vec<_>>()
                        );
                    }
                }
            }
        }
    }
}
