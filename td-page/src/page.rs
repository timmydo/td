//! A page's styled document and its layouts (td-web/DESIGN.md §10, "Pipeline and
//! threads", "Painting" and "Places"): td-layout's styled document over a
//! tree, with td-web's per-box tables, and a layout with what td-web
//! indexes over it. Each is immutable once made and shared between the
//! window thread and the render thread; a layout is replaced whole.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::fonts::Measure;
use crate::inline::{FitCopy, Inline, Set};
use crate::sheets::Omitted;
use crate::tree::{Tree, NONE};
use td_alloc::{Budget, BudgetError, BudgetShared, BudgetVec};
use td_css::values::{ComputedValue, Property};
use td_css::{ComputedStyle, StyleSet};
use td_html::document::NodeId;
use td_layout::block::{layout_with_budget, BoxGeometry, IntrinsicSize, Layout, Request};
use td_layout::inline::{FontMetrics, Line, UNIT};
use td_layout::{BoxKind, BoxNode, Error, Limits, StyleId, StyledDocument, TableRole};
use td_ui::picture::Fitted;
use td_ui::raster::TextFamily;

/// Lines, boxes and markers an index by height spans together.
pub const CHUNK: usize = 64;
/// The most scalars of an image's `alt` text measured for its size.
pub const MAX_ALT_SCALARS: usize = 256;

pub const UNDERLINE: u8 = 1;
pub const OVERLINE: u8 = 2;
pub const LINE_THROUGH: u8 = 4;

/// What a layout is made for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Want {
    /// The viewport in CSS px.
    pub width: f64,
    pub height: f64,
    /// Device pixels to a CSS px.
    pub scale: u8,
    pub body: TextFamily,
    /// With the page's own sheets and `style` attributes; `F9` clears it.
    pub styles: bool,
    /// The page's inline pictures come by its last settle, which it lays
    /// at their size (td-web/DESIGN.md §10, "Subresources").
    pub pictures: u32,
}

/// A tree styled: td-layout's styled document, its environment, and
/// td-web's tables over its boxes.
pub struct Styled {
    tree: BudgetShared<Tree>,
    styled: StyledDocument<'static>,
    /// Each node's first box, by node index; `NONE` for none.
    node_box: BudgetVec<u32>,
    /// Each box's text decorations, its own and its ancestors'.
    decorations: BudgetVec<u8>,
    /// Laid with the page's own sheets and attributes.
    pub own: bool,
    pub omitted: Omitted,
    /// The page's own sheets or attributes set a colour.
    pub coloured: bool,
}

fn decorations_of(style: Option<&ComputedStyle>) -> u8 {
    match style.and_then(|style| style.get(Property::TextDecoration)) {
        Some(ComputedValue::Decoration(d)) => {
            (u8::from(d.underline) * UNDERLINE)
                | (u8::from(d.overline) * OVERLINE)
                | (u8::from(d.line_through) * LINE_THROUGH)
        }
        _ => 0,
    }
}

impl Styled {
    /// `tree` cascaded with `set` under `limits`, its tables in `page`.
    pub fn new(
        tree: BudgetShared<Tree>,
        set: &StyleSet,
        input_bytes: usize,
        limits: Limits<'_>,
        omitted: Omitted,
        coloured: bool,
        page: &Budget,
    ) -> Result<Self, Error> {
        let own = limits.style_attributes;
        let styled = StyledDocument::shared(tree.shared()?, set, input_bytes, limits, page)?;
        let mut node_box = BudgetVec::filled(page, tree.document().len(), NONE)?;
        let boxes = styled.boxes();
        let mut decorations = td_alloc::BudgetVec::with_capacity(page, boxes.len())?;
        for (index, b) in boxes.iter().enumerate() {
            if let Some(slot) = node_box.get_mut(b.node.index()) {
                if *slot == NONE {
                    *slot = u32::try_from(index).unwrap_or(NONE);
                }
            }
            let inherited = b
                .parent
                .and_then(|parent| decorations.get(parent.index()).copied())
                .unwrap_or(0);
            decorations.try_push(inherited | decorations_of(styled.style(b.style)))?;
        }
        Ok(Self {
            tree,
            styled,
            node_box,
            decorations,
            own,
            omitted,
            coloured,
        })
    }

    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    pub fn styled(&self) -> &StyledDocument<'static> {
        &self.styled
    }

    pub fn style(&self, id: StyleId) -> Option<&ComputedStyle> {
        self.styled.style(id)
    }

    /// A box by its index.
    pub fn node_of(&self, index: usize) -> Option<&BoxNode> {
        self.styled.boxes().get(index)
    }

    /// `node`'s first box's index.
    pub fn box_of(&self, node: NodeId) -> Option<usize> {
        let at = *self.node_box.get(node.index())?;
        (at != NONE).then_some(at as usize)
    }

    /// The decorations a box's text draws.
    pub fn decorations(&self, index: usize) -> u8 {
        self.decorations.get(index).copied().unwrap_or(0)
    }

    /// A box's part in a table: a block whose display names one.
    pub fn table_role(&self, index: usize) -> Option<TableRole> {
        let b = self.node_of(index)?;
        if b.kind != BoxKind::Block {
            return None;
        }
        match self.style(b.style)?.get(Property::Display) {
            Some(ComputedValue::Keyword(display)) => TableRole::of(display),
            _ => None,
        }
    }

    /// The declared styled charge and td-web's own tables beside it.
    pub fn bytes(&self) -> usize {
        self.styled.charges().styled.saturating_add(
            (self.node_box.capacity() * 4).saturating_add(self.decorations.capacity()),
        )
    }
}

/// A span of CSS px down the page a run of index entries' ink covers,
/// and how far any one's ink reaches above and below its own box.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Span {
    pub top: f64,
    pub bottom: f64,
    pub above: f64,
    pub below: f64,
}

impl Span {
    const EMPTY: Self = Self {
        top: f64::INFINITY,
        bottom: f64::NEG_INFINITY,
        above: 0.0,
        below: 0.0,
    };
    fn with(self, ink: Extent) -> Self {
        Self {
            top: self.top.min(ink.top),
            bottom: self.bottom.max(ink.bottom),
            above: self.above.max(ink.above),
            below: self.below.max(ink.below),
        }
    }
    pub fn meets(self, top: f64, bottom: f64) -> bool {
        self.top < bottom && self.bottom > top
    }
    /// Whether the ink of an entry of this run whose box is `from..to`
    /// can meet `top..bottom`.
    pub fn reaches(self, from: f64, to: f64, top: f64, bottom: f64) -> bool {
        from - self.above < bottom && to + self.below > top
    }
}

/// One index entry's ink down the page, and how far it passes its box.
struct Extent {
    top: f64,
    bottom: f64,
    above: f64,
    below: f64,
}

impl Extent {
    /// Ink from `top` to `bottom` over a box from `from` to `to`.
    fn over(top: f64, bottom: f64, from: f64, to: f64) -> Self {
        Self {
            top,
            bottom,
            above: (from - top).max(0.0),
            below: (bottom - to).max(0.0),
        }
    }
}

/// A rectangle in CSS px, held as f32: a clip.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Clip {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl Clip {
    pub const NONE: Self = Self {
        left: f32::NEG_INFINITY,
        top: f32::NEG_INFINITY,
        right: f32::INFINITY,
        bottom: f32::INFINITY,
    };
    fn meet(self, other: Self) -> Self {
        Self {
            left: self.left.max(other.left),
            top: self.top.max(other.top),
            right: self.right.min(other.right),
            bottom: self.bottom.min(other.bottom),
        }
    }
}

/// Spans of `CHUNK` entries' ink, each from `span`.
fn chunks<T>(
    items: &[T],
    memory: &Budget,
    mut span: impl FnMut(&T) -> Option<Extent>,
) -> Result<BudgetVec<Span>, BudgetError> {
    let mut out = td_alloc::BudgetVec::with_capacity(memory, items.len().div_ceil(CHUNK))?;
    for group in items.chunks(CHUNK) {
        let covered = group
            .iter()
            .filter_map(&mut span)
            .fold(Span::EMPTY, Span::with);
        out.try_push(covered)?;
    }
    Ok(out)
}

/// A marker's box down the page, its ink's span widened from it.
pub fn marker_box(marker: &td_layout::list::MarkerFragment) -> (f64, f64) {
    (marker.y, marker.y + marker.height.max(1.0))
}

/// The span down the page in CSS px the ink of glyphs of `style` on a
/// baseline at `baseline` can cover: drawn from td-ui's `top_at` down its
/// line box at their size, which a tight `line-height` makes taller than
/// td-layout's, and passing that by its reach, a device pixel more each
/// way for the pen's rounding.
pub fn glyph_ink(
    measure: &mut Measure,
    style: &ComputedStyle,
    baseline: f64,
    scale: f64,
) -> (f64, f64) {
    let font = measure.font(style);
    let reach = measure.reach(font);
    let line = measure.metrics().line_at(font.size);
    let above = f64::from(line.baseline) + f64::from(reach.above) + 1.0;
    let below = f64::from(line.height) - f64::from(line.baseline) + f64::from(reach.below) + 1.0;
    (baseline - above / scale, baseline + below / scale)
}

/// The indexes of chunks of `spans` meeting `top..bottom`.
pub fn meeting(spans: &[Span], top: f64, bottom: f64) -> impl Iterator<Item = usize> + '_ {
    spans
        .iter()
        .enumerate()
        .filter(move |(_, span)| span.meets(top, bottom))
        .map(|(at, _)| at)
}

/// A layout and what td-web indexes over it.
pub struct Laid {
    styled: BudgetShared<Styled>,
    pub layout: Layout,
    pub want: Want,
    /// Spans of the lines' ink, `CHUNK` lines each.
    line_spans: BudgetVec<Span>,
    /// Geometry in tree order, a parent before its children, with floats
    /// and inline-blocks after the boxes around them (`floats_last`).
    painted: BudgetVec<u32>,
    box_spans: BudgetVec<Span>,
    marker_spans: BudgetVec<Span>,
    /// Each geometry's clip for its content, and the geometry its
    /// nearest ancestor with one has (`NONE` for the root).
    clips: BudgetVec<Clip>,
    parents: BudgetVec<u32>,
    /// Each node's geometry, by node index.
    node_geometry: BudgetVec<u32>,
    /// Links that laid a fragment or replaced box, in tree order, each
    /// with the line its first lies on.
    links: BudgetVec<(NodeId, u32)>,
    /// Each line's index among the lines, or `NONE`, of the line it lies
    /// within: a float's lines lie within the line it was met in, an
    /// inline-block's within the line holding it.
    outer: BudgetVec<u32>,
    /// Each line's lines before and after it in its paragraph, or `NONE`,
    /// on a page with floats or inline-blocks, whose lines can lie between
    /// two of a paragraph's (`text::joined_before`).
    joins: BudgetVec<(u32, u32)>,
    /// Each inline picture laid, fitted to its box, by node index.
    fits: BudgetVec<Fit>,
    /// The node indices of pictures whose copy was refused, for room or
    /// for `FIT_WORK`, in order: each keeps its placeholder.
    short: BudgetVec<usize>,
}

/// `painted`, in tree order with `parents` worked out over it, ordered
/// by how many floats and inline-blocks each box lies in, the box's own
/// counted, then by tree order: floats and inline-blocks after the
/// in-flow boxes around them, and one within another after it (CSS 2.2
/// appendix E paints an inline-block as an inline, after the blocks'
/// backgrounds). A parent stays before its children, as it lies in no
/// more of them than they.
fn floats_last(
    layout: &Layout,
    styled: &StyledDocument<'static>,
    tree: &Tree,
    parents: &[u32],
    painted: &mut [u32],
    memory: &Budget,
) -> Result<(), BudgetError> {
    let mut depth = BudgetVec::filled(memory, layout.boxes.len(), 0u32)?;
    for &at in painted.iter() {
        let Some(geometry) = layout.boxes.get(at as usize) else {
            continue;
        };
        let parent = parents
            .get(at as usize)
            .and_then(|&up| depth.get(up as usize))
            .copied()
            .unwrap_or(0);
        if let Some(slot) = depth.get_mut(at as usize) {
            let apart = styled.is_float(geometry) || styled.is_inline_block(geometry);
            *slot = parent.saturating_add(u32::from(apart));
        }
    }
    painted.sort_unstable_by_key(|&at| {
        let rank = layout
            .boxes
            .get(at as usize)
            .map_or(NONE, |g: &BoxGeometry| tree.rank(g.node));
        let depth = depth.get(at as usize).copied().unwrap_or(0);
        (depth, rank, u32::MAX - at)
    });
    Ok(())
}

/// An inline picture fitted to the box a layout lays it in: its node's
/// index, its size in device pixels, and what is drawn there.
pub struct Fit {
    node: usize,
    width: usize,
    height: usize,
    copy: Drawn,
}

/// What a picture's box draws: the picture itself at a whole multiple of
/// its size, each pixel a square that many device pixels on a side; else
/// a copy fitted to the box, shared with a later layout that lays it at
/// the same size.
enum Drawn {
    Scaled(BudgetShared<Inline>, usize),
    Copy(BudgetShared<FitCopy>),
}

/// A box's device pixels across, as a paint at `scale` places its edges:
/// each edge rounded on its own.
fn device(at: f64, size: f64, scale: f64) -> usize {
    let from = (at * scale).round();
    let to = ((at + size) * scale).round();
    if to > from && to - from < usize::MAX as f64 {
        (to - from) as usize
    } else {
        0
    }
}

/// Each picture of `pictures` that `layout` lays fitted to its box, in
/// device pixels at `scale`, its inline boxes and then its block-level
/// ones: drawn scaled when the box is a whole multiple of its size, else
/// a copy `reuse` made at that size shared, else one made and charged to
/// the pictures term within `FIT_WORK` steps a layout; one refused, for
/// room or work, left out and counted, its placeholder drawn. By node
/// index.
fn fit(
    layout: &Layout,
    pictures: &Set,
    reuse: Option<&Laid>,
    scale: f64,
    cancel: &AtomicBool,
    memory: &Budget,
) -> Result<(BudgetVec<Fit>, BudgetVec<usize>), Error> {
    let mut fits = BudgetVec::new(memory)?;
    let mut short = BudgetVec::new(memory)?;
    let mut work = pictures.work();
    // An inline-block's placeholder is no picture.
    let inline = layout
        .lines
        .iter()
        .flat_map(|line| line.replaced.iter())
        .filter(|r| !r.atomic)
        .map(|r| (r.node, r.x, r.y, r.width, r.height));
    let blocks = layout.boxes.iter().filter(|g| g.replaced).map(|g| {
        (
            g.node,
            g.content.x,
            g.content.y,
            g.content.width,
            g.content.height,
        )
    });
    for (node, x, y, w, h) in inline.chain(blocks) {
        let Some(picture) = pictures.get(node) else {
            continue;
        };
        if cancel.load(Ordering::SeqCst) {
            return Err(Error::Cancelled);
        }
        let index = node.index();
        if fits.iter().any(|fit: &Fit| fit.node == index) || short.contains(&index) {
            continue;
        }
        let (width, height) = (device(x, w, scale), device(y, h, scale));
        if width == 0 || height == 0 {
            continue;
        }
        let copy = match picture.factor(width, height) {
            Some(factor) => Drawn::Scaled(picture.try_clone()?, factor),
            None => {
                let kept = reuse
                    .and_then(|old| old.fit_of(node))
                    .filter(|old| old.width == width && old.height == height)
                    .and_then(|old| match &old.copy {
                        Drawn::Copy(copy) => Some(copy.try_clone()),
                        Drawn::Scaled(..) => None,
                    })
                    .transpose()?;
                let made = match kept {
                    Some(copy) => Some(copy),
                    None => picture
                        .fit(width, height, pictures.term(), &mut work, cancel)
                        .and_then(|copy| BudgetShared::new(pictures.term(), copy).ok()),
                };
                if cancel.load(Ordering::SeqCst) {
                    return Err(Error::Cancelled);
                }
                let Some(copy) = made else {
                    short.try_push(index)?;
                    continue;
                };
                Drawn::Copy(copy)
            }
        };
        fits.try_push(Fit {
            node: index,
            width,
            height,
            copy,
        })?;
    }
    fits.sort_unstable_by_key(|fit| fit.node);
    short.sort_unstable();
    Ok((fits, short))
}

/// Each image's intrinsic size: its picture's, in CSS px a pixel, once it
/// has come; else its `alt` text's line in its own font, none without
/// one (td-web/DESIGN.md §10, "Subresources"). Sorted by node index.
fn alt_sizes(
    styled: &Styled,
    measure: &mut Measure,
    pictures: Option<&Set>,
    memory: &Budget,
) -> Result<BudgetVec<(usize, IntrinsicSize)>, BudgetError> {
    let mut out = BudgetVec::new(memory)?;
    for b in styled.styled().boxes() {
        if b.kind != BoxKind::Image {
            continue;
        }
        let Some(style) = styled.style(b.style) else {
            continue;
        };
        if let Some(inline) = pictures.and_then(|set| set.get(b.node)) {
            let (width, height) = inline.natural();
            let width = u32::try_from(width).unwrap_or(u32::MAX);
            let height = u32::try_from(height).unwrap_or(u32::MAX);
            out.try_push((b.node.index(), IntrinsicSize::picture_px(width, height)))?;
            continue;
        }
        let alt = styled.tree().alt(b.node).unwrap_or("");
        let size = if alt.is_empty() {
            IntrinsicSize {
                width: Some(0),
                height: Some(0),
                ratio: None,
            }
        } else {
            let width = alt
                .chars()
                .filter(|c| !c.is_control())
                .take(MAX_ALT_SCALARS)
                .fold(0u32, |sum, c| sum.saturating_add(measure.advance(style, c)));
            IntrinsicSize {
                width: Some(width),
                height: Some(measure.line(style).height),
                ratio: None,
            }
        };
        out.try_push((b.node.index(), size))?;
    }
    out.sort_unstable_by_key(|(node, _)| *node);
    Ok(out)
}

/// CSS px of `units` of 1/64.
pub fn px(units: u32) -> f64 {
    f64::from(units) / f64::from(UNIT)
}

fn overflow_clips(style: &ComputedStyle) -> bool {
    [Property::OverflowX, Property::OverflowY].iter().any(|&p| {
        matches!(
            style.get(p),
            Some(ComputedValue::Keyword("hidden" | "clip"))
        )
    })
}

/// Steps of the walk up to a geometry's nearest ancestor with one between
/// reads of the cancel flag: a deep nest of elements without geometry over
/// many boxes makes the walk long.
const WALK_INTERVAL: usize = 4096;

/// Each node's geometry, and each geometry's clip and nearest ancestor
/// geometry.
struct Ancestry {
    node_geometry: BudgetVec<u32>,
    clips: BudgetVec<Clip>,
    parents: BudgetVec<u32>,
}

/// `Ancestry` over `layout`, its geometry visited in tree order
/// (`painted`), reading `cancelled` at the first step of the walk and
/// every `WALK_INTERVAL` after.
fn ancestry(
    layout: &Layout,
    styled: &Styled,
    painted: &[u32],
    mut cancelled: impl FnMut() -> bool,
    memory: &Budget,
) -> Result<Ancestry, Error> {
    let tree = styled.tree();
    let count = layout.boxes.len();
    let mut node_geometry = BudgetVec::filled(memory, tree.document().len(), NONE)?;
    let mut clips = BudgetVec::filled(memory, count, Clip::NONE)?;
    let mut parents = BudgetVec::filled(memory, count, NONE)?;
    let mut steps = 0usize;
    let mut tick = || {
        let read = steps.is_multiple_of(WALK_INTERVAL);
        steps = steps.wrapping_add(1);
        if read && cancelled() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    };
    for &at in painted {
        tick()?;
        let Some(geometry) = layout.boxes.get(at as usize) else {
            continue;
        };
        // The nearest ancestor element with geometry, already visited.
        let mut parent = NONE;
        let mut up = tree.document().node(geometry.node).and_then(|n| n.parent());
        while let Some(node) = up {
            tick()?;
            let found = node_geometry.get(node.index()).copied().unwrap_or(NONE);
            if found != NONE {
                parent = found;
                break;
            }
            up = tree.document().node(node).and_then(|n| n.parent());
        }
        if let Some(slot) = node_geometry.get_mut(geometry.node.index()) {
            if *slot == NONE {
                *slot = at;
            }
        }
        let outer = clips.get(parent as usize).copied().unwrap_or(Clip::NONE);
        let own = styled
            .style(geometry.style)
            .is_some_and(overflow_clips)
            .then(|| {
                let b = geometry.border;
                let w = geometry.border_widths;
                Clip {
                    left: (b.x + w.left) as f32,
                    top: (b.y + w.top) as f32,
                    right: (b.x + b.width - w.right) as f32,
                    bottom: (b.y + b.height - w.bottom) as f32,
                }
            });
        if let Some(slot) = clips.get_mut(at as usize) {
            *slot = own.map_or(outer, |own| outer.meet(own));
        }
        if let Some(slot) = parents.get_mut(at as usize) {
            *slot = parent;
        }
    }
    Ok(Ancestry {
        node_geometry,
        clips,
        parents,
    })
}

impl Laid {
    /// `styled` laid out for `want` with `measure`, its output charged to
    /// `memory` beside `retained` bytes of output kept; `pictures` laid at
    /// their size and fitted to their boxes, the copies `reuse` made at
    /// the same size shared.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        styled: BudgetShared<Styled>,
        measure: &mut Measure,
        want: Want,
        retained: usize,
        cancel: &AtomicBool,
        memory: &Budget,
        pictures: Option<&Set>,
        reuse: Option<&Laid>,
    ) -> Result<Self, Error> {
        measure.forget();
        let sizes = alt_sizes(&styled, measure, pictures, memory)?;
        let intrinsic = |node: NodeId, _: StyleId| {
            sizes
                .binary_search_by_key(&node.index(), |(at, _)| *at)
                .ok()
                .and_then(|at| sizes.get(at))
                .map(|(_, size)| *size)
        };
        let request = Request {
            intrinsic: Some(&intrinsic),
            retained_output_bytes: retained,
            cancel: Some(cancel),
            ..Request::new(want.width, want.height)
        };
        let layout = layout_with_budget(styled.styled(), measure, &request, memory)?;
        drop(sizes);
        let scale = f64::from(want.scale.max(1));
        let (fits, short) = match pictures {
            Some(pictures) => fit(&layout, pictures, reuse, scale, cancel, memory)?,
            None => (BudgetVec::new(memory)?, BudgetVec::new(memory)?),
        };
        let line_spans = chunks(&layout.lines, memory, |line: &Line| {
            // The line's box, where its fills fall, and its glyphs' ink.
            let (mut top, mut bottom) = (line.y, line.y + line.height);
            let baseline = line.y + line.baseline;
            for fragment in line.fragments.iter() {
                if let Some(style) = styled.style(fragment.style) {
                    let (above, below) = glyph_ink(measure, style, baseline, scale);
                    top = top.min(above);
                    bottom = bottom.max(below);
                }
            }
            for replaced in line.replaced.iter() {
                top = top.min(replaced.y);
                bottom = bottom.max(replaced.y + replaced.height);
            }
            Some(Extent::over(top, bottom, line.y, line.y + line.height))
        })?;
        let marker_spans = chunks(&layout.markers, memory, |marker| {
            let (from, to) = marker_box(marker);
            let (mut top, mut bottom) = (from, to);
            if let Some(style) = styled.style(marker.style) {
                let (above, below) = glyph_ink(measure, style, marker.baseline, scale);
                top = top.min(above);
                bottom = bottom.max(below);
            }
            Some(Extent::over(top, bottom, from, to))
        })?;
        let tree = styled.tree();
        // Geometry in tree order: by rank, and of one node's, outermost
        // (last in postorder) first.
        let count = layout.boxes.len();
        let mut painted = td_alloc::BudgetVec::with_capacity(memory, count)?;
        for at in 0..count {
            painted.try_push(u32::try_from(at).unwrap_or(NONE))?;
        }
        painted.sort_unstable_by_key(|&at| {
            let rank = layout
                .boxes
                .get(at as usize)
                .map_or(NONE, |g: &BoxGeometry| tree.rank(g.node));
            (rank, u32::MAX - at)
        });
        #[cfg(test)]
        tests::before_walk(cancel);
        let Ancestry {
            node_geometry,
            clips,
            parents,
        } = ancestry(
            &layout,
            &styled,
            &painted,
            || cancel.load(Ordering::SeqCst),
            memory,
        )?;
        if styled.styled().has_nested_lines() {
            floats_last(
                &layout,
                styled.styled(),
                tree,
                &parents,
                &mut painted,
                memory,
            )?;
        }
        let box_spans = chunks(painted.as_slice(), memory, |&at| {
            let g = layout.boxes.get(at as usize)?;
            let (top, bottom) = (g.border.y, g.border.y + g.border.height);
            Some(Extent::over(top, bottom, top, bottom))
        })?;
        let mut links: BudgetVec<(NodeId, u32)> = BudgetVec::new(memory)?;
        for (index, line) in layout.lines.iter().enumerate() {
            let index = u32::try_from(index).unwrap_or(NONE);
            let nodes = line
                .fragments
                .iter()
                .map(|f| f.node)
                .chain(line.replaced.iter().map(|r| r.node));
            for node in nodes {
                if let Some(link) = tree.link(node) {
                    if links.last().is_some_and(|(last, _)| *last == link) {
                        continue;
                    }
                    links.try_push((link, index))?;
                }
            }
        }
        links.sort_unstable_by_key(|&(link, line)| (tree.rank(link), line));
        let mut kept = 0usize;
        for at in 0..links.len() {
            let Some(here) = links.get(at).copied() else {
                break;
            };
            let previous = kept.checked_sub(1).and_then(|last| links.get(last));
            if previous.is_some_and(|(link, _)| *link == here.0) {
                continue;
            }
            if let Some(slot) = links.get_mut(kept) {
                *slot = here;
            }
            kept += 1;
        }
        links.truncate(kept);
        let mut laid = Self {
            styled,
            layout,
            want,
            line_spans,
            painted,
            box_spans,
            marker_spans,
            clips,
            parents,
            node_geometry,
            links,
            outer: BudgetVec::new(memory)?,
            joins: BudgetVec::new(memory)?,
            fits,
            short,
        };
        if laid.styled.styled().has_nested_lines() {
            (laid.outer, laid.joins) = crate::text::nesting(&laid, memory)?;
        }
        Ok(laid)
    }

    /// Line `line`'s lines before and after it in its paragraph, where
    /// the page keeps them (one with floats or inline-blocks).
    pub fn joins(&self, line: usize) -> Option<(Option<usize>, Option<usize>)> {
        let &(before, after) = self.joins.get(line)?;
        let line = |at: u32| (at != NONE).then_some(at as usize);
        Some((line(before), line(after)))
    }

    /// The line `line` lies within, if any: the one a float whose line
    /// it is was met in, or the one holding an inline-block whose line it
    /// is.
    pub fn outer(&self, line: usize) -> Option<usize> {
        self.outer
            .get(line)
            .filter(|&&at| at != NONE)
            .map(|&at| at as usize)
    }

    /// The fitted copy of `node`'s picture, if it was laid with one.
    fn fit_of(&self, node: NodeId) -> Option<&Fit> {
        let at = self
            .fits
            .binary_search_by_key(&node.index(), |fit| fit.node)
            .ok()?;
        self.fits.get(at)
    }

    /// How many pictures' copies were refused.
    pub fn short(&self) -> usize {
        self.short.len()
    }

    /// Whether `node`'s picture was settled but its copy refused: its
    /// placeholder is drawn.
    pub fn refused(&self, node: NodeId) -> bool {
        self.short.binary_search(&node.index()).is_ok()
    }

    /// How many pictures it draws from copies of their own.
    pub fn copies(&self) -> usize {
        self.fits
            .iter()
            .filter(|fit| matches!(fit.copy, Drawn::Copy(_)))
            .count()
    }

    /// The id a paint names `node`'s picture by, if it was laid with one.
    pub fn fitted(&self, node: NodeId) -> Option<u64> {
        self.fits
            .binary_search_by_key(&node.index(), |fit| fit.node)
            .ok()
            .map(|at| at as u64)
    }

    /// Whether the picture a paint names by `id` is opaque throughout.
    pub fn opaque(&self, id: u64) -> bool {
        let fit = usize::try_from(id).ok().and_then(|at| self.fits.get(at));
        match fit.map(|fit| &fit.copy) {
            Some(Drawn::Scaled(inline, _)) => inline.opaque(),
            Some(Drawn::Copy(copy)) => copy.opaque(),
            None => false,
        }
    }

    /// Each picture the layout lays, in node order: its node's index and
    /// its box's size in device pixels. For tests and measurements.
    pub fn fits(&self) -> impl Iterator<Item = (usize, usize, usize)> + '_ {
        self.fits
            .iter()
            .map(|fit| (fit.node, fit.width, fit.height))
    }

    /// The copy fitted to its box for the picture of the node of index
    /// `node`, when it is drawn from one. For tests and measurements.
    pub fn fit_copy(&self, node: usize) -> Option<&BudgetShared<FitCopy>> {
        match &self.fits.iter().find(|fit| fit.node == node)?.copy {
            Drawn::Copy(copy) => Some(copy),
            Drawn::Scaled(..) => None,
        }
    }

    /// The whole multiple of its size the picture of the node of index
    /// `node` is drawn at, when it is drawn with no copy. For tests and
    /// measurements.
    pub fn factor(&self, node: usize) -> Option<usize> {
        match &self.fits.iter().find(|fit| fit.node == node)?.copy {
            Drawn::Scaled(_, factor) => Some(*factor),
            Drawn::Copy(_) => None,
        }
    }

    /// The picture a paint names by `id` and the whole multiple of its
    /// size it is drawn at.
    pub fn fitted_by(&self, id: u64) -> Option<(&Fitted, usize)> {
        let fit = self.fits.get(usize::try_from(id).ok()?)?;
        Some(match &fit.copy {
            Drawn::Scaled(inline, factor) => (inline.picture(), *factor),
            Drawn::Copy(copy) => (copy.fitted(), 1),
        })
    }

    pub fn styled(&self) -> &Styled {
        &self.styled
    }

    pub fn styled_ref(&self) -> &BudgetShared<Styled> {
        &self.styled
    }

    pub fn tree(&self) -> &Tree {
        self.styled.tree()
    }

    pub fn lines(&self) -> &[Line] {
        &self.layout.lines
    }

    pub fn line_spans(&self) -> &[Span] {
        &self.line_spans
    }

    pub fn marker_spans(&self) -> &[Span] {
        &self.marker_spans
    }

    pub fn box_spans(&self) -> &[Span] {
        &self.box_spans
    }

    /// Geometry indexes in paint order: tree order, floats after the
    /// boxes of their context.
    pub fn painted(&self) -> &[u32] {
        &self.painted
    }

    pub fn geometry(&self, at: u32) -> Option<&BoxGeometry> {
        self.layout.boxes.get(at as usize)
    }

    /// The clip a geometry's content is drawn in.
    pub fn clip(&self, at: u32) -> Clip {
        self.clips.get(at as usize).copied().unwrap_or(Clip::NONE)
    }

    /// The geometry of the nearest ancestor that has one.
    pub fn parent(&self, at: u32) -> Option<u32> {
        let parent = *self.parents.get(at as usize)?;
        (parent != NONE).then_some(parent)
    }

    /// A node's geometry.
    pub fn geometry_of(&self, node: NodeId) -> Option<u32> {
        let at = *self.node_geometry.get(node.index())?;
        (at != NONE).then_some(at)
    }

    /// The clip a line's content is drawn in: its block's.
    pub fn line_clip(&self, line: &Line) -> Clip {
        line.block()
            .and_then(|block| self.styled.node_of(block.index()))
            .and_then(|b| self.geometry_of(b.node))
            .map_or(Clip::NONE, |at| self.clip(at))
    }

    /// The links laid, in tree order, each with its first line.
    pub fn links(&self) -> &[(NodeId, u32)] {
        &self.links
    }

    /// The output's charge and td-web's indexes beside it.
    pub fn bytes(&self) -> usize {
        let spans =
            (self.line_spans.capacity() + self.box_spans.capacity() + self.marker_spans.capacity())
                * std::mem::size_of::<Span>();
        self.layout
            .output_bytes
            .saturating_add(spans)
            .saturating_add(
                (self.painted.capacity() + self.parents.capacity() + self.node_geometry.capacity())
                    * 4
                    + self.clips.capacity() * std::mem::size_of::<Clip>()
                    + self.links.capacity() * std::mem::size_of::<(NodeId, u32)>()
                    + self.outer.capacity() * 4
                    + self.joins.capacity() * 8
                    + self.fits.capacity() * std::mem::size_of::<Fit>()
                    + self.short.capacity() * std::mem::size_of::<usize>(),
            )
    }
}

// The window thread paints and the render thread lays out from these.
const _: () = {
    const fn shared<T: Send + Sync>() {}
    shared::<Laid>();
    shared::<Styled>();
};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
pub(crate) mod tests {
    use super::*;
    use crate::sheets;
    use crate::tree::Source;
    use std::cell::Cell;
    use td_ui::prose::Metrics;
    use td_ui::raster::Scale;

    thread_local! {
        /// Whether `Laid::new` sets its flag once laid out, before the walk.
        static SET_BEFORE_WALK: Cell<bool> = const { Cell::new(false) };
    }

    pub(super) fn before_walk(cancel: &AtomicBool) {
        if SET_BEFORE_WALK.with(Cell::take) {
            cancel.store(true, Ordering::SeqCst);
        }
    }

    pub(crate) const WANT: Want = Want {
        width: 400.0,
        height: 300.0,
        scale: 1,
        body: TextFamily::Sans,
        styles: true,
        pictures: 0,
    };

    /// `markup` parsed, styled and laid for `want` on the cell.
    pub(crate) fn laid_for(markup: &str, source: Source, want: Want) -> Laid {
        let page = Budget::new(256 << 20).unwrap();
        let tree = BudgetShared::new(&page, Tree::parse(markup, source, &page).unwrap()).unwrap();
        let environment = sheets::environment(want.width, want.height, u32::from(want.scale));
        let (set, omitted) =
            sheets::compile(&tree, None, want.styles, &environment, &page).unwrap();
        let limits = Limits {
            input: td_layout::MAX_INPUT,
            style_attributes: want.styles,
            ..Limits::default()
        };
        let coloured = sheets::coloured(&tree, None);
        let styled =
            Styled::new(tree, &set, markup.len(), limits, omitted, coloured, &page).unwrap();
        let styled = BudgetShared::new(&page, styled).unwrap();
        let scale = Scale::new(want.scale).unwrap();
        let mut measure = Measure::new(Metrics::new(None, scale), want.body);
        Laid::new(
            styled,
            &mut measure,
            want,
            0,
            &AtomicBool::new(false),
            &page,
            None,
            None,
        )
        .unwrap()
    }

    pub(crate) fn laid(markup: &str) -> Laid {
        laid_for(markup, Source::Markup, WANT)
    }

    /// A float's box, and those in it, paint after the in-flow boxes
    /// of the page, whose backgrounds would otherwise cover it; the rest
    /// keep tree order, an ancestor before its descendants.
    #[test]
    fn floats_paint_after_the_boxes_in_flow() {
        let page = laid(
            "<body style=margin:0><div id=f style=float:left;width:40px;height:40px>\
             <p id=in style=margin:0>x</p><div id=g style=float:left>y</div></div>\
             <div id=a style=background:blue>text</div>",
        );
        let at = |id: &str| {
            let node = page.tree().target(id).unwrap();
            let geometry = page
                .layout
                .boxes
                .iter()
                .position(|g| g.node == node)
                .unwrap() as u32;
            page.painted().iter().position(|&p| p == geometry).unwrap()
        };
        assert!(at("a") < at("f"));
        assert!(at("f") < at("in") && at("in") < at("g"));
        // A float within a float after its float's in-flow boxes.
        let page = laid(
            "<body style=margin:0><div id=f style=float:left;width:200px>\
             <div id=g style=float:left;width:50px;height:50px>y</div>\
             <p id=in style=margin:0;background:blue>text text</p></div>",
        );
        let at = |id: &str| {
            let node = page.tree().target(id).unwrap();
            let geometry = page
                .layout
                .boxes
                .iter()
                .position(|g| g.node == node)
                .unwrap() as u32;
            page.painted().iter().position(|&p| p == geometry).unwrap()
        };
        assert!(at("f") < at("in") && at("in") < at("g"));
        // A caption laid after the rows holds a float: the rows are not
        // in it.
        let page = laid(
            "<body><table><tr><td>r1</td></tr><caption>\
             <div style=float:left>x</div>cap</caption>\
             <tr><td id=r2>r2</td></tr></table><p id=after>after</p>",
        );
        let at = |id: &str| {
            let node = page.tree().target(id).unwrap();
            let geometry = page
                .layout
                .boxes
                .iter()
                .position(|g| g.node == node)
                .unwrap() as u32;
            page.painted().iter().position(|&p| p == geometry).unwrap()
        };
        assert!(at("r2") < at("after"));
        let plain = laid("<body><div id=a>a</div><div id=b>b</div>");
        let node = |id: &str| plain.tree().target(id).unwrap();
        let order: Vec<_> = plain
            .painted()
            .iter()
            .map(|&p| plain.layout.boxes[p as usize].node)
            .filter(|&n| n == node("a") || n == node("b"))
            .collect();
        assert_eq!(order, [node("a"), node("b")]);
    }

    /// An inline-block's lines lie within the line holding it, one in it
    /// within its; its box paints after the in-flow boxes around it,
    /// whose backgrounds would otherwise cover it, and one in it after it.
    #[test]
    fn inline_blocks_nest_their_lines_and_paint_after_the_blocks_around_them() {
        let page = laid(
            "<body style=margin:0><div id=a style=background:blue>one \
             <span id=b style=display:inline-block;background:red>in it<br>\
             two <span id=i style=display:inline-block>three</span></span> after</div>\
             <div id=c style=background:green>next</div>",
        );
        assert!(page.styled().styled().has_inline_blocks());
        let texts: Vec<String> = page
            .lines()
            .iter()
            .map(|line| {
                line.fragments
                    .iter()
                    .map(|f| crate::text::fragment_text(&page, f))
                    .collect()
            })
            .collect();
        assert_eq!(texts, ["one  after", "in it", "two ", "three", "next"]);
        let outer: Vec<_> = (0..5).map(|line| page.outer(line)).collect();
        assert_eq!(outer, [None, Some(0), Some(0), Some(2), None]);
        let at = |id: &str| {
            let node = page.tree().target(id).unwrap();
            let geometry = page
                .layout
                .boxes
                .iter()
                .position(|g| g.node == node)
                .unwrap() as u32;
            page.painted().iter().position(|&p| p == geometry).unwrap()
        };
        assert!(at("a") < at("c") && at("c") < at("b") && at("b") < at("i"));
        // Its placeholder is no picture to fit.
        assert!(page.fits().next().is_none());
    }

    /// An inline table is laid as an inline-block: its cells' lines lie
    /// within the line holding it, and it and its cells paint after the
    /// in-flow boxes around it.
    #[test]
    fn inline_tables_nest_their_lines_and_paint_after_the_blocks_around_them() {
        let page = laid(
            "<body style=margin:0><div id=a style=background:blue>one \
             <table id=t style=display:inline-table;background:red><tr>\
             <td id=d style=background:white>in it<td>two</table> after</div>\
             <div id=c style=background:green>next</div>",
        );
        assert!(page.styled().styled().has_inline_blocks());
        let texts: Vec<String> = page
            .lines()
            .iter()
            .map(|line| {
                line.fragments
                    .iter()
                    .map(|f| crate::text::fragment_text(&page, f))
                    .collect()
            })
            .collect();
        assert_eq!(texts, ["one  after", "in it", "two", "next"]);
        let outer: Vec<_> = (0..4).map(|line| page.outer(line)).collect();
        assert_eq!(outer, [None, Some(0), Some(0), None]);
        let at = |id: &str| {
            let node = page.tree().target(id).unwrap();
            let geometry = page
                .layout
                .boxes
                .iter()
                .position(|g| g.node == node)
                .unwrap() as u32;
            page.painted().iter().position(|&p| p == geometry).unwrap()
        };
        assert!(at("a") < at("c") && at("c") < at("t") && at("t") < at("d"));
        assert!(page.fits().next().is_none());
    }

    #[test]
    fn boxes_are_painted_in_tree_order_and_clip_their_descendants() {
        let laid = laid(
            "<body style=margin:0><div id=a style='overflow:hidden;height:20px;\
             border-left:5px solid'><p id=b style=margin:0>x<p id=c>y</div><p id=d>z",
        );
        let tree = laid.tree();
        let ranks: Vec<u32> = laid
            .painted()
            .iter()
            .map(|&at| tree.rank(laid.geometry(at).unwrap().node))
            .collect();
        assert!(ranks.windows(2).all(|w| w[0] <= w[1]), "{ranks:?}");
        let b = laid.geometry_of(tree.target("b").unwrap()).unwrap();
        let clip = laid.clip(b);
        assert_eq!((clip.left, clip.top, clip.bottom), (5.0, 0.0, 20.0));
        let d = laid.geometry_of(tree.target("d").unwrap()).unwrap();
        assert_eq!(laid.clip(d), Clip::NONE);
        assert_eq!(
            laid.geometry(laid.parent(b).unwrap()).unwrap().node,
            tree.target("a").unwrap()
        );
    }

    #[test]
    fn links_are_listed_once_in_tree_order_with_their_first_line() {
        let laid = laid(
            "<p><a href=1>one</a><p><a href=2>two<br>more</a><a href=hidden style=display:none>h</a>\
             <p><img alt=pic><a href=3><img alt=x></a>",
        );
        let hrefs: Vec<_> = laid
            .links()
            .iter()
            .map(|&(link, line)| (laid.tree().href(link).unwrap(), line))
            .collect();
        assert_eq!(hrefs, [("1", 0), ("2", 1), ("3", 3)]);
    }

    #[test]
    fn an_image_lays_its_alt_text_line_or_nothing() {
        let laid = laid("<body style=margin:0><img alt=abcd><img>");
        let replaced: Vec<_> = laid.lines()[0]
            .replaced
            .iter()
            .map(|r| (r.width, r.height))
            .collect();
        assert_eq!(replaced, [(32.0, 16.0), (0.0, 0.0)]);
    }

    #[test]
    fn the_walk_up_to_an_ancestors_geometry_reads_the_flag_every_interval() {
        // Each block's walk passes the inline elements over it, which have
        // no geometry of their own.
        let markup = format!("{}{}", "<span>".repeat(200), "<p>x".repeat(100));
        let laid = laid(&markup);
        let memory = Budget::new(64 << 20).unwrap();
        let walk = |cancelled: &mut dyn FnMut() -> bool| {
            ancestry(
                &laid.layout,
                laid.styled(),
                laid.painted(),
                cancelled,
                &memory,
            )
        };
        let mut reads = 0usize;
        let whole = walk(&mut || {
            reads += 1;
            false
        })
        .unwrap();
        assert_eq!(whole.parents.as_slice(), laid.parents.as_slice());
        assert_eq!(whole.clips.as_slice(), laid.clips.as_slice());
        assert_eq!(
            whole.node_geometry.as_slice(),
            laid.node_geometry.as_slice()
        );
        // 100 walks of at least 200 steps.
        assert!(reads >= 100 * 200 / WALK_INTERVAL, "{reads}");
        assert!(reads <= (laid.painted().len() * 260).div_ceil(WALK_INTERVAL));
        // Cancelled at the first read, and partway.
        assert!(matches!(walk(&mut || true), Err(Error::Cancelled)));
        let mut reads = 0usize;
        let partway = walk(&mut || {
            reads += 1;
            reads == 3
        });
        assert!(matches!(partway, Err(Error::Cancelled)));
        assert_eq!(reads, 3);
    }

    #[test]
    fn a_layout_cancelled_before_its_walk_gives_cancelled() {
        let page = Budget::new(64 << 20).unwrap();
        let tree =
            BudgetShared::new(&page, Tree::parse("<p>a", Source::Markup, &page).unwrap()).unwrap();
        let environment = sheets::environment(400.0, 300.0, 1);
        let (set, omitted) = sheets::compile(&tree, None, true, &environment, &page).unwrap();
        let styled = Styled::new(tree, &set, 4, Limits::default(), omitted, false, &page).unwrap();
        drop(set);
        let styled = BudgetShared::new(&page, styled).unwrap();
        let used = page.used();
        let mut measure = Measure::new(Metrics::new(None, Scale::new(1).unwrap()), WANT.body);
        let cancel = AtomicBool::new(false);
        SET_BEFORE_WALK.with(|set| set.set(true));
        let laid = Laid::new(
            styled.try_clone().unwrap(),
            &mut measure,
            WANT,
            0,
            &cancel,
            &page,
            None,
            None,
        );
        assert!(matches!(laid, Err(Error::Cancelled)));
        assert!(!SET_BEFORE_WALK.with(Cell::get));
        assert_eq!(page.used(), used);
    }

    #[test]
    fn decorations_propagate_to_descendants() {
        let laid = laid("<u><b>x</b></u><s>y</s>");
        let styled = laid.styled();
        let tree = laid.tree();
        let at = |name: &str| {
            let mut node = Some(tree.document().root());
            while let Some(id) = node {
                if tree
                    .document()
                    .node(id)
                    .unwrap()
                    .element()
                    .is_some_and(|e| e.name.as_str() == name)
                {
                    return styled.box_of(id).unwrap();
                }
                node = tree.next(id, false);
            }
            panic!("{name}");
        };
        assert_eq!(styled.decorations(at("b")), UNDERLINE);
        assert_eq!(styled.decorations(at("s")), LINE_THROUGH);
    }
}
