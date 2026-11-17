//! A laid page painted (DESIGN.md §10, "Painting"): the canvas, each box's
//! background and borders in tree order, each line's content, then the
//! marks over it, all clipped to the pane, the damage and the boxes that
//! clip their descendants. Glyphs are drawn ordered by family and size so
//! td-ui remakes no atlas page per glyph. A paint is bounded by draws and
//! visits, by pixels and by other work; past any it stops and says it
//! was cut.

use crate::fonts::{Font, Measure};
use crate::page::{
    marker_box, meeting, Clip, Laid, Styled, CHUNK, LINE_THROUGH, OVERLINE, UNDERLINE,
};
use crate::search::Hit;
use crate::text::{self, Place};
use std::ops::ControlFlow;
use td_alloc::{Budget, BudgetError, BudgetVec};
use td_css::values::{ComputedValue, Property};
use td_css::ComputedStyle;
use td_html::document::{Namespace, NodeId};
use td_layout::block::BoxGeometry;
use td_layout::inline::{DisplayText, Line, TextFragment};
use td_ui::prose;
use td_ui::raster::{
    Draw, GlyphStyle, Primitive, Rect, Scale, TextFamily, BORDER, INACTIVE_SELECTION, INK, PAPER,
    SELECTED,
};
use td_ui::theme::KEYS;

/// The most fills and glyphs one paint draws, and boxes, lines and
/// markers meeting the damage it visits; the rest is skipped and the
/// status row says so. A visit counts so that boxes, lines or markers
/// laid on one another, every one in the pane, end a paint however few
/// pixels they cover. Four times the glyphs of a 3840x2160 pane of 8 px
/// cells. Ordered by family and size, a frame remakes at most one atlas
/// page per pair.
pub const MAX_PAINT: usize = 262_144;
/// The glyphs' order's scratch at first, and the most glyphs a paint
/// draws unordered, past what that scratch could be grown to hold: each
/// could remake an atlas page, 47 µs at the largest sizes, so about 3 s
/// at worst.
pub const MAX_UNORDERED: usize = 65_536;
/// The most steps of other work one paint takes: each fragment, replaced
/// box and entry of the index looked at, each scalar replayed or
/// measured (and a tabbed fragment's bytes, which td-layout scans before
/// replaying it), and each ancestor stepped through for a ground. Four
/// steps a draw: a line replays at most td-layout's
/// `MAX_FRAGMENT_SCALARS` before its clip, so a pane of text meets
/// `MAX_PAINT` first.
pub const MAX_PAINT_WORK: usize = 4 * MAX_PAINT;
/// The most glyphs drawn at one pen: combining marks past it are not
/// drawn, so a run of them costs no more than this a scalar.
pub const MAX_STACKED: usize = 8;
/// The most pixels one paint fills, in panes: each fill charges what it
/// covers and each glyph its em square, within the clip, and a paint
/// stops at this many times the pane's area. A pane of text charges about
/// two panes (ems overlap at their advances), the canvas and a few
/// nested backgrounds one each, find's and the selection's grounds one;
/// the corpus charges at most 2.9 panes, so 8 leaves it room while
/// td-ui's fill rate (about 2 Gpx/s) holds a paint to tens of ms.
pub const PANES: u64 = 8;
/// Fills held back from the sink so a later fill that covers one leaves
/// it undrawn; past this many the oldest is drawn.
const HELD: usize = 16;

/// Where the page lies in the window: its pane, and the device pixel the
/// page's origin falls on.
#[derive(Clone, Copy, Debug)]
pub struct Frame {
    pub pane: Rect,
    pub x: i64,
    pub y: i64,
    pub scale: i64,
}

impl Frame {
    fn x(&self, css: f64) -> i64 {
        self.x
            .saturating_add((css * self.scale as f64).round() as i64)
    }
    fn y(&self, css: f64) -> i64 {
        self.y
            .saturating_add((css * self.scale as f64).round() as i64)
    }
    fn rect(&self, x: f64, y: f64, width: f64, height: f64) -> Option<Rect> {
        let left = self.x(x);
        let top = self.y(y);
        let right = self.x(x + width);
        let bottom = self.y(y + height);
        (right > left && bottom > top).then(|| Rect {
            x: left,
            y: top,
            width: u32::try_from(right - left).unwrap_or(u32::MAX),
            height: u32::try_from(bottom - top).unwrap_or(u32::MAX),
        })
    }
    fn clip(&self, clip: Clip) -> Option<Rect> {
        if clip == Clip::NONE {
            return Some(self.pane);
        }
        let left = if clip.left.is_finite() {
            self.x(f64::from(clip.left))
        } else {
            i64::MIN / 4
        };
        let top = if clip.top.is_finite() {
            self.y(f64::from(clip.top))
        } else {
            i64::MIN / 4
        };
        let right = if clip.right.is_finite() {
            self.x(f64::from(clip.right))
        } else {
            i64::MAX / 4
        };
        let bottom = if clip.bottom.is_finite() {
            self.y(f64::from(clip.bottom))
        } else {
            i64::MAX / 4
        };
        if right <= left || bottom <= top {
            return None;
        }
        Rect {
            x: left,
            y: top,
            width: u32::try_from(right - left).unwrap_or(u32::MAX),
            height: u32::try_from(bottom - top).unwrap_or(u32::MAX),
        }
        .intersection(self.pane)
    }
    fn surface_scale(&self) -> Scale {
        Scale::new(u8::try_from(self.scale).unwrap_or(1)).unwrap_or_default()
    }
}

/// What a paint marks over the page.
#[derive(Clone, Copy, Debug, Default)]
pub struct Marks<'a> {
    pub hits: &'a [Hit],
    pub selection: Option<(Place, Place)>,
    /// The focused or hovered link, drawn as selected.
    pub link: Option<NodeId>,
}

/// A glyph waiting to be drawn in its family and size's turn.
#[derive(Clone, Copy, Debug)]
struct Waiting {
    /// Family, size, then the order drawn.
    key: (u8, u16, u32),
    draw: Draw,
}

/// The glyphs of a paint, kept between paints so their storage is taken
/// once.
pub struct Scratch {
    glyphs: Option<BudgetVec<Waiting>>,
}

impl Scratch {
    /// None taken yet: `reserve` takes it at the first paint.
    pub fn new_empty() -> Self {
        Self { glyphs: None }
    }
    #[cfg(test)]
    pub fn new(memory: &Budget) -> Self {
        Self {
            glyphs: td_alloc::BudgetVec::with_capacity(memory, MAX_UNORDERED).ok(),
        }
    }
}

/// How a paint went.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Painted {
    /// Draws given to the sink.
    pub drawn: usize,
    /// The pixels charged against `PANES` panes.
    pub pixels: u64,
    /// `MAX_PAINT` or the pixel bound was reached and the rest skipped.
    pub cut: bool,
}

fn area(rect: Rect) -> u64 {
    u64::from(rect.width) * u64::from(rect.height)
}

/// Whether `outer` holds every pixel of `inner`.
fn covers(outer: Rect, inner: Rect) -> bool {
    let right = |r: Rect| r.x.saturating_add(i64::from(r.width));
    let bottom = |r: Rect| r.y.saturating_add(i64::from(r.height));
    inner.x >= outer.x
        && inner.y >= outer.y
        && right(inner) <= right(outer)
        && bottom(inner) <= bottom(outer)
}

/// The pens of a run of glyphs, so at most `MAX_STACKED` are drawn at one.
#[derive(Clone, Copy, Debug)]
struct Stack {
    x: i64,
    count: usize,
}

impl Stack {
    const NEW: Self = Self {
        x: i64::MIN,
        count: 0,
    };
    /// Whether a glyph at `x` is among the first `MAX_STACKED` there.
    fn admits(&mut self, x: i64) -> bool {
        if self.x == x {
            self.count = self.count.saturating_add(1);
        } else {
            *self = Self { x, count: 1 };
        }
        self.count <= MAX_STACKED
    }
}

const NOTHING: Rect = Rect {
    x: 0,
    y: 0,
    width: 0,
    height: 0,
};

struct Painter<'s, 'k> {
    sink: &'s mut dyn FnMut(Draw),
    damage: Rect,
    scale: Scale,
    /// Fills, glyphs and visits taken, against `MAX_PAINT`.
    taken: usize,
    /// Glyphs drawn unordered, against `MAX_UNORDERED`.
    unordered: usize,
    /// Other work, against `MAX_PAINT_WORK`.
    worked: usize,
    drawn: usize,
    /// Pixels charged, against `most`.
    pixels: u64,
    most: u64,
    cut: bool,
    coloured: bool,
    /// Fills not yet drawn, oldest first, each with its charge.
    held: [(Draw, u64); HELD],
    holding: usize,
    waiting: Option<&'k mut BudgetVec<Waiting>>,
}

impl<'s, 'k> Painter<'s, 'k> {
    fn new(
        sink: &'s mut dyn FnMut(Draw),
        frame: &Frame,
        damage: Rect,
        coloured: bool,
        waiting: Option<&'k mut BudgetVec<Waiting>>,
    ) -> Self {
        let nothing = Draw {
            clip: NOTHING,
            primitive: Primitive::Fill {
                rect: NOTHING,
                color: 0,
            },
        };
        Self {
            sink,
            damage,
            scale: frame.surface_scale(),
            taken: 0,
            unordered: 0,
            worked: 0,
            drawn: 0,
            pixels: 0,
            most: area(frame.pane).saturating_mul(PANES),
            cut: false,
            coloured,
            held: [(nothing, 0); HELD],
            holding: 0,
            waiting,
        }
    }
    fn room(&mut self) -> bool {
        if self.cut || self.taken >= MAX_PAINT {
            self.cut = true;
            return false;
        }
        true
    }
    /// Takes a box's, line's or marker's visit against `MAX_PAINT`, or
    /// one passed by as work.
    fn pass(&mut self, meets: bool) -> bool {
        if meets {
            self.visit()
        } else {
            self.work(1)
        }
    }
    /// Takes a box's, line's or marker's visit against `MAX_PAINT`.
    fn visit(&mut self) -> bool {
        if !self.room() {
            return false;
        }
        self.taken += 1;
        true
    }
    /// Takes `steps` of other work against `MAX_PAINT_WORK`; past it the
    /// paint is cut.
    fn work(&mut self, steps: usize) -> bool {
        let spent = self.worked.saturating_add(steps);
        if self.cut || spent > MAX_PAINT_WORK {
            self.cut = true;
            return false;
        }
        self.worked = spent;
        true
    }
    /// Takes one draw charging `pixels`, `refund` of those charged before
    /// given back; past the bound the paint is cut.
    fn charge(&mut self, pixels: u64, refund: u64) -> bool {
        let spent = self.pixels.saturating_sub(refund).saturating_add(pixels);
        if spent > self.most {
            self.cut = true;
            return false;
        }
        self.pixels = spent;
        self.taken += 1;
        true
    }
    fn emit(&mut self, draw: Draw) {
        self.drawn += 1;
        (self.sink)(draw);
    }
    /// Draws the fills held back, in order.
    fn release(&mut self) {
        for at in 0..self.holding {
            if let Some(&(draw, _)) = self.held.get(at) {
                self.emit(draw);
            }
        }
        self.holding = 0;
    }
    /// A colour as td-web draws it: off the palette's keys once the page
    /// sets colours, so no theme maps them.
    fn colour(&self, c: u32) -> u32 {
        let c = c & 0xff_ffff;
        if self.coloured && KEYS.iter().any(|&key| key & 0xff_ffff == c) {
            c ^ 1
        } else {
            c
        }
    }
    /// An opaque fill. The fills held back that it covers are not drawn:
    /// it would cover them.
    fn fill(&mut self, rect: Rect, color: u32, clip: Rect) {
        let Some(clip) = clip
            .intersection(self.damage)
            .and_then(|c| c.intersection(rect))
        else {
            return;
        };
        if !self.room() {
            return;
        }
        let held = self.held.get(..self.holding).unwrap_or(&[]);
        let refund = held
            .iter()
            .filter(|(d, _)| covers(clip, d.clip))
            .fold(0u64, |sum, &(_, pixels)| sum.saturating_add(pixels));
        let pixels = area(clip);
        if !self.charge(pixels, refund) {
            return;
        }
        let mut kept = 0;
        for at in 0..self.holding {
            let Some(&entry) = self.held.get(at) else {
                break;
            };
            if covers(clip, entry.0.clip) {
                continue;
            }
            if let Some(slot) = self.held.get_mut(kept) {
                *slot = entry;
            }
            kept += 1;
        }
        self.holding = kept;
        if self.holding >= HELD {
            if let Some(&(oldest, _)) = self.held.first() {
                self.emit(oldest);
            }
            self.held.copy_within(1.., 0);
            self.holding = HELD - 1;
        }
        let draw = Draw {
            clip,
            primitive: Primitive::Fill { rect, color },
        };
        if let Some(slot) = self.held.get_mut(self.holding) {
            *slot = (draw, pixels);
            self.holding += 1;
        }
    }
    fn glyph(&mut self, x: i64, y: i64, scalar: char, style: GlyphStyle, clip: Rect) {
        let Some(clip) = clip.intersection(self.damage) else {
            return;
        };
        if !self.room() {
            return;
        }
        let em = u32::from(style.size.pixels(self.scale));
        let square = Rect {
            x,
            y,
            width: em,
            height: em,
        };
        let pixels = square.intersection(clip).map_or(0, area);
        if !self.charge(pixels, 0) {
            return;
        }
        let draw = Draw {
            clip,
            primitive: Primitive::Glyph {
                x,
                y,
                scalar,
                style,
            },
        };
        let family = match style.family {
            TextFamily::Cell => 0,
            TextFamily::Sans => 1,
            TextFamily::Serif => 2,
            TextFamily::Mono => 3,
        };
        let size = style.size.pixels(Scale::default());
        let waiting = Waiting {
            key: (family, size, u32::try_from(self.taken).unwrap_or(u32::MAX)),
            draw,
        };
        if self.keep(waiting) {
            return;
        }
        if self.unordered >= MAX_UNORDERED {
            self.cut = true;
            return;
        }
        self.unordered += 1;
        self.release();
        self.emit(draw);
    }
    /// Puts a glyph in order, the scratch grown by doubling to
    /// `MAX_PAINT` as it fills; false where it is full and not grown.
    fn keep(&mut self, waiting: Waiting) -> bool {
        let Some(list) = self.waiting.as_deref_mut() else {
            return false;
        };
        if list.len() == list.capacity() {
            let target = list.capacity().saturating_mul(2).clamp(1, MAX_PAINT);
            if target <= list.capacity() {
                return false;
            }
            let Ok(mut grown) = td_alloc::BudgetVec::with_capacity(list.budget(), target) else {
                return false;
            };
            for &kept in list.iter() {
                if grown.push_within_capacity(kept).is_err() {
                    return false;
                }
            }
            *list = grown;
        }
        list.push_within_capacity(waiting).is_ok()
    }
    /// Draws what is held and waiting, the glyphs by family, size, then
    /// the order they came in.
    fn finish(mut self) -> Painted {
        self.release();
        if let Some(glyphs) = self.waiting.take() {
            glyphs.sort_unstable_by_key(|w| w.key);
            for w in glyphs.iter() {
                self.drawn += 1;
                (self.sink)(w.draw);
            }
            glyphs.clear();
        }
        Painted {
            drawn: self.drawn,
            pixels: self.pixels,
            cut: self.cut,
        }
    }
}

fn rgb(c: td_css::values::Color) -> u32 {
    u32::from(c.red) << 16 | u32::from(c.green) << 8 | u32::from(c.blue)
}

/// `c` over `ground`, by its alpha; none for a transparent colour.
fn blend(c: td_css::values::Color, ground: u32) -> Option<u32> {
    match c.alpha {
        0 => None,
        255 => Some(rgb(c)),
        a => {
            let a = u32::from(a);
            let mut out = 0;
            for shift in [0, 8, 16] {
                let top = (rgb(c) >> shift) & 255;
                let under = (ground >> shift) & 255;
                out |= ((top * a + under * (255 - a) + 127) / 255) << shift;
            }
            Some(out)
        }
    }
}

fn colour_of(style: &ComputedStyle, property: Property) -> Option<td_css::values::Color> {
    match style.get(property) {
        Some(ComputedValue::Color(c)) => Some(*c),
        Some(ComputedValue::Keyword(k)) if k.eq_ignore_ascii_case("currentcolor") => {
            Some(style.color())
        }
        _ => None,
    }
}

fn hidden(style: &ComputedStyle) -> bool {
    matches!(
        style.get(Property::Visibility),
        Some(ComputedValue::Keyword("hidden" | "collapse"))
    )
}

/// The longest path a `Chain` holds: td-layout nests boxes at most
/// `MAX_DEPTH` deep, so every path from the root is shorter.
const DEEPEST: usize = td_layout::MAX_DEPTH + 2;

/// Values worked out down one path from the root, each entry the parent
/// of the next, kept through a paint: asked in tree order, a paint steps
/// through each ancestor once, not once an item. Keys follow tree order,
/// an ancestor's below its descendants'.
struct Chain<V> {
    held: [(u64, V); DEEPEST],
    len: usize,
    /// The keys a walk stepped through, nearest first.
    path: [u64; DEEPEST],
}

impl<V: Copy> Chain<V> {
    fn new(empty: V) -> Self {
        Self {
            held: [(0, empty); DEEPEST],
            len: 0,
            path: [0; DEEPEST],
        }
    }
    /// The value at `key`: `over(key, under)`, `under` its parent's (`up`
    /// names it) and `root` above the root. Each key stepped through to
    /// one held is charged to `p`'s work; none once that cuts the paint.
    fn value(
        &mut self,
        p: &mut Painter,
        key: u64,
        up: impl Fn(u64) -> Option<u64>,
        over: impl Fn(u64, V) -> V,
        root: V,
    ) -> Option<V> {
        let mut walked = 0usize;
        let mut at = Some(key);
        let mut under = None;
        'walk: while let Some(here) = at {
            // What is not `here` or its ancestor is no ancestor of
            // anything asked for after, in tree order.
            while let Some(top) = self.len.checked_sub(1) {
                let Some(&(held, value)) = self.held.get(top) else {
                    break;
                };
                if held == here {
                    under = Some(value);
                    break 'walk;
                }
                if held < here {
                    break;
                }
                self.len = top;
            }
            let Some(slot) = self.path.get_mut(walked) else {
                break;
            };
            if !p.work(1) {
                return None;
            }
            *slot = here;
            walked += 1;
            at = up(here);
        }
        let mut value = match under {
            Some(value) => value,
            None => {
                self.len = 0;
                root
            }
        };
        for &here in self.path.get(..walked).unwrap_or(&[]).iter().rev() {
            value = over(here, value);
            if let Some(slot) = self.held.get_mut(self.len) {
                *slot = (here, value);
                self.len += 1;
            }
        }
        Some(value)
    }
}

/// A geometry's key in tree order: its node's rank, then, of one node's,
/// the outermost first, as `Laid::painted` orders them.
fn geometry_key(laid: &Laid, at: u32) -> u64 {
    let rank = laid
        .geometry(at)
        .map_or(u32::MAX, |g| laid.tree().rank(g.node));
    u64::from(rank) << 32 | u64::from(u32::MAX - at)
}

fn geometry_of_key(key: u64) -> u32 {
    u32::MAX - (key & u64::from(u32::MAX)) as u32
}

/// A geometry's background, if opaque.
fn opaque(laid: &Laid, at: u32) -> Option<u32> {
    laid.geometry(at)
        .and_then(|g| laid.styled().style(g.style))
        .and_then(|s| colour_of(s, Property::BackgroundColor))
        .filter(|c| c.alpha == 255)
        .map(rgb)
}

/// The opaque colour under a geometry: the nearest ancestor's, itself
/// included, with an opaque background, else the canvas's. None once the
/// walk cuts the paint.
fn ground_of(
    p: &mut Painter,
    grounds: &mut Chain<u32>,
    laid: &Laid,
    at: Option<u32>,
    canvas: u32,
) -> Option<u32> {
    let Some(at) = at else {
        return Some(canvas);
    };
    grounds.value(
        p,
        geometry_key(laid, at),
        |key| {
            laid.parent(geometry_of_key(key))
                .map(|up| geometry_key(laid, up))
        },
        |key, under| opaque(laid, geometry_of_key(key)).unwrap_or(under),
        canvas,
    )
}

/// The canvas (CSS 2.2 §14.2): the root element's background, else the
/// HTML body element's (the root's first `body` or `frameset` child),
/// else `PAPER`.
fn canvas(laid: &Laid) -> u32 {
    let document = laid.tree().document();
    let html = |id: NodeId, names: &[&str]| {
        document
            .node(id)
            .and_then(|n| n.element())
            .is_some_and(|e| e.namespace == Namespace::Html && names.contains(&e.name.as_str()))
    };
    let root = document
        .children(document.root())
        .find(|(_, n)| n.element().is_some())
        .map(|(id, _)| id);
    let body = root.filter(|&id| html(id, &["html"])).and_then(|root| {
        document
            .children(root)
            .map(|(id, _)| id)
            .find(|&id| html(id, &["body", "frameset"]))
    });
    for node in [root, body].into_iter().flatten() {
        let style = laid
            .geometry_of(node)
            .and_then(|at| laid.geometry(at))
            .and_then(|g| laid.styled().style(g.style));
        if let Some(c) = style
            .and_then(|s| colour_of(s, Property::BackgroundColor))
            .and_then(|c| blend(c, PAPER))
        {
            return c;
        }
    }
    PAPER
}

const SIDES: &[(Property, Property)] = &[
    (Property::BorderTopStyle, Property::BorderTopColor),
    (Property::BorderRightStyle, Property::BorderRightColor),
    (Property::BorderBottomStyle, Property::BorderBottomColor),
    (Property::BorderLeftStyle, Property::BorderLeftColor),
];

#[allow(clippy::too_many_arguments)]
fn paint_box(
    p: &mut Painter,
    grounds: &mut Chain<u32>,
    laid: &Laid,
    frame: &Frame,
    at: u32,
    g: &BoxGeometry,
    canvas: u32,
) {
    let Some(style) = laid.styled().style(g.style) else {
        return;
    };
    if hidden(style) {
        return;
    }
    let clip = laid
        .parent(at)
        .map_or(Some(frame.pane), |parent| frame.clip(laid.clip(parent)));
    let Some(clip) = clip else {
        return;
    };
    let b = g.border;
    // The ground is walked to only for a translucent colour.
    let mut under = None;
    let mut shown = |p: &mut Painter, c: td_css::values::Color| match c.alpha {
        0 => None,
        255 => Some(rgb(c)),
        _ => {
            if under.is_none() {
                under = ground_of(p, grounds, laid, laid.parent(at), canvas);
            }
            blend(c, under?)
        }
    };
    if let Some(c) = colour_of(style, Property::BackgroundColor).and_then(|c| shown(p, c)) {
        if let Some(rect) = frame.rect(b.x, b.y, b.width, b.height) {
            let color = p.colour(c);
            p.fill(rect, color, clip);
        }
    }
    let w = g.border_widths;
    let sides = [
        (b.x, b.y, b.width, w.top),
        (b.x + b.width - w.right, b.y, w.right, b.height),
        (b.x, b.y + b.height - w.bottom, b.width, w.bottom),
        (b.x, b.y, w.left, b.height),
    ];
    for (&(style_of, colour), &(x, y, width, height)) in SIDES.iter().zip(sides.iter()) {
        if width <= 0.0 || height <= 0.0 {
            continue;
        }
        let drawn = !matches!(
            style.get(style_of),
            None | Some(ComputedValue::Keyword("none" | "hidden"))
        );
        if !drawn {
            continue;
        }
        let c = colour_of(style, colour).unwrap_or_else(|| style.color());
        if let (Some(c), Some(rect)) = (shown(p, c), frame.rect(x, y, width, height)) {
            let color = p.colour(c);
            p.fill(rect, color, clip);
        }
    }
}

/// Backgrounds laid over one another as one, premultiplied.
#[derive(Clone, Copy, Debug)]
struct Tint {
    sum: [f64; 3],
    alpha: f64,
    /// Some background was not wholly transparent.
    found: bool,
}

impl Tint {
    const CLEAR: Self = Self {
        sum: [0.0; 3],
        alpha: 0.0,
        found: false,
    };
    /// These with `c` laid over them.
    fn with(self, c: td_css::values::Color) -> Self {
        let share = f64::from(c.alpha) / 255.0;
        let mut sum = self.sum;
        for (total, channel) in sum.iter_mut().zip([c.red, c.green, c.blue]) {
            *total = share * f64::from(channel) + (1.0 - share) * *total;
        }
        Self {
            sum,
            alpha: share + (1.0 - share) * self.alpha,
            found: self.found || c.alpha > 0,
        }
    }
    /// The tint over `ground` as one colour; none when nothing showed.
    fn over(self, ground: u32) -> Option<u32> {
        if !self.found {
            return None;
        }
        let mut out = 0u32;
        for (total, shift) in self.sum.iter().zip([16u32, 8, 0]) {
            let under = f64::from((ground >> shift) & 255);
            let channel = (total + (1.0 - self.alpha) * under)
                .round()
                .clamp(0.0, 255.0) as u32;
            out |= channel << shift;
        }
        Some(out)
    }
}

/// The backgrounds of the inline ancestors of box `index` as one tint:
/// all lie over the same extent, so what shows is the innermost opaque
/// one and the translucent ones inside it, up to the first box that is
/// not inline. None once the walk cuts the paint.
fn inline_tint(
    p: &mut Painter,
    tints: &mut Chain<Tint>,
    styled: &Styled,
    index: usize,
) -> Option<Tint> {
    let Some(parent) = styled.node_of(index).and_then(|b| b.parent) else {
        return Some(Tint::CLEAR);
    };
    // Box indexes follow tree order, a parent's below its children's.
    let node = |key: u64| usize::try_from(key).ok().and_then(|at| styled.node_of(at));
    tints.value(
        p,
        parent.index() as u64,
        |key| node(key)?.parent.map(|up| up.index() as u64),
        |key, under| {
            let Some(b) = node(key).filter(|b| b.kind == td_layout::BoxKind::Inline) else {
                return Tint::CLEAR;
            };
            match styled
                .style(b.style)
                .and_then(|s| colour_of(s, Property::BackgroundColor))
            {
                Some(c) => under.with(c),
                None => under,
            }
        },
        Tint::CLEAR,
    )
}

/// What a scalar is marked as, if anything.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mark {
    None,
    Hit,
    Selected,
}

fn mark_of(marks: &Marks, link: Option<NodeId>, start: Place, hits: &mut usize) -> Mark {
    if let Some((s, e)) = marks.selection {
        if start >= s && start < e {
            return Mark::Selected;
        }
    }
    if link.is_some() && link == marks.link {
        return Mark::Selected;
    }
    while marks.hits.get(*hits).is_some_and(|hit| hit.end <= start) {
        *hits += 1;
    }
    if marks.hits.get(*hits).is_some_and(|hit| hit.start <= start) {
        return Mark::Hit;
    }
    Mark::None
}

#[allow(clippy::too_many_arguments)]
fn paint_text(
    p: &mut Painter,
    laid: &Laid,
    measure: &mut Measure,
    frame: &Frame,
    line: &Line,
    f: &TextFragment,
    clip: Rect,
    ground: u32,
    marks: &Marks,
    tints: &mut Chain<Tint>,
    stack: &mut Stack,
) {
    if !p.work(1) {
        return;
    }
    let styled = laid.styled();
    let Some(style) = styled.style(f.style) else {
        return;
    };
    if hidden(style) {
        return;
    }
    let font = measure.font(style);
    let scale = frame.scale;
    let overhang = i64::from(prose::overhang(font.size, frame.surface_scale()));
    // Ink reaches an em past a pen at most: none past these is drawn.
    let left_edge = clip.x - overhang;
    let right_edge = clip.x + i64::from(clip.width) + overhang;
    if frame.x(f.x + f.width) < left_edge || frame.x(f.x) > right_edge {
        return;
    }
    let box_index = styled.box_of(f.node);
    let mut ground = ground;
    if let Some(index) = box_index {
        let Some(tint) = inline_tint(p, tints, styled, index) else {
            return;
        };
        if let Some(c) = tint.over(ground) {
            if let Some(rect) = frame.rect(f.x, line.y, f.width, line.height) {
                let color = p.colour(c);
                p.fill(rect, color, clip);
            }
            ground = c;
        }
    }
    let Some(ink) = blend(style.color(), ground) else {
        return;
    };
    let ink = p.colour(ink);
    let shown_ground = p.colour(ground);
    let baseline = frame.y(line.y + line.baseline);
    let top = measure.metrics().top_at(font.size, baseline);
    let link = laid.tree().link(f.node);
    let line_top = frame.y(line.y);
    let line_bottom = frame.y(line.y + line.height);
    let mut hits = marks
        .hits
        .partition_point(|hit| hit.end <= text::Place::new(laid, f.node, f.range.start));
    let mut run: Option<(i64, i64, Mark)> = None;
    let mut glyphs = [(0i64, ' ', Mark::None); 64];
    let mut pending = 0usize;
    // Marked glyphs in the theme's pairs, which no page colour moves: the
    // selection's paper on `SELECTED`, a match's ink on
    // `INACTIVE_SELECTION`, legible whatever the page's own colours.
    let flush = |p: &mut Painter, run: Option<(i64, i64, Mark)>, glyphs: &[(i64, char, Mark)]| {
        if let Some((from, to, mark)) = run {
            let color = match mark {
                Mark::Selected => SELECTED,
                Mark::Hit => INACTIVE_SELECTION,
                Mark::None => 0,
            };
            if mark != Mark::None && to > from {
                let rect = Rect {
                    x: from,
                    y: line_top,
                    width: u32::try_from(to - from).unwrap_or(0),
                    height: u32::try_from(line_bottom - line_top).unwrap_or(0),
                };
                p.fill(rect, color, clip);
            }
        }
        for &(x, c, mark) in glyphs {
            let (ink, ground) = match mark {
                Mark::Selected => (PAPER, SELECTED),
                Mark::Hit => (INK, INACTIVE_SELECTION),
                Mark::None => (ink, shown_ground),
            };
            p.glyph(x, top, c, font.glyph(ink, ground), clip);
        }
    };
    // td-layout scans a tabbed fragment whole before it replays it.
    if f.display == DisplayText::Source {
        let tabbed = laid
            .tree()
            .document()
            .node(f.node)
            .and_then(|n| n.text())
            .and_then(|text| text.get(f.range.start..f.range.end))
            .filter(|text| text.contains('\t'));
        if tabbed.is_some_and(|text| !p.work(text.len())) {
            return;
        }
    }
    let ok = text::extents_until(laid, measure, line, f, |e| {
        if !p.work(1) {
            return ControlFlow::Break(());
        }
        let x = frame.x(e.x);
        let end = frame.x(e.x + e.width);
        // Pens only go right but for a negative advance: past the clip's
        // right edge, the rest of the fragment is past it too.
        if x > right_edge && e.width >= 0.0 {
            return ControlFlow::Break(());
        }
        if end < left_edge || x > right_edge {
            return ControlFlow::Continue(());
        }
        let mark = mark_of(marks, link, e.start, &mut hits);
        match run {
            Some((from, _, m)) if m == mark && pending < glyphs.len() => run = Some((from, end, m)),
            _ => {
                flush(p, run, glyphs.get(..pending).unwrap_or(&[]));
                pending = 0;
                run = Some((x, end, mark));
            }
        }
        if !e.shown.is_whitespace() && stack.admits(x) {
            if let Some(slot) = glyphs.get_mut(pending) {
                *slot = (x, e.shown, mark);
                pending += 1;
            }
        }
        ControlFlow::Continue(())
    });
    flush(p, run, glyphs.get(..pending).unwrap_or(&[]));
    if !ok {
        return;
    }
    // Decorations: this text's and its ancestors', a pixel at scale one.
    let decorations = box_index.map_or(0, |index| styled.decorations(index));
    let thick = u32::try_from(scale.max(1)).unwrap_or(1);
    let size = i64::from(font.size.pixels(frame.surface_scale()));
    let x = frame.x(f.x);
    let width = u32::try_from(frame.x(f.x + f.width) - x).unwrap_or(0);
    for (bit, y) in [
        (UNDERLINE, baseline + scale),
        (OVERLINE, line_top),
        (LINE_THROUGH, baseline - size * 3 / 10),
    ] {
        if decorations & bit != 0 {
            let rect = Rect {
                x,
                y,
                width,
                height: thick,
            };
            p.fill(rect, ink, clip);
        }
    }
}

/// Text drawn in `font` from `x` on a baseline, clipped, at most
/// `MAX_STACKED` glyphs at one pen and `MAX_ALT_SCALARS` scalars; for alt
/// text and markers, whose advances td-web measures itself.
#[allow(clippy::too_many_arguments)]
fn paint_run(
    p: &mut Painter,
    measure: &mut Measure,
    frame: &Frame,
    font: Font,
    text: &str,
    x: f64,
    baseline: i64,
    ink: u32,
    ground: u32,
    clip: Rect,
) {
    let top = measure.metrics().top_at(font.size, baseline);
    let mut pen = x;
    let mut stack = Stack::NEW;
    for c in text.chars().take(crate::page::MAX_ALT_SCALARS) {
        if !p.work(1) {
            break;
        }
        if c.is_control() {
            continue;
        }
        let at = frame.x(pen);
        if !c.is_whitespace() && stack.admits(at) {
            p.glyph(at, top, c, font.glyph(ink, ground), clip);
        }
        pen += crate::page::px(measure.advance_in(font, c));
        if frame.x(pen) > clip.x + i64::from(clip.width) {
            break;
        }
    }
}

/// A line's replaced boxes: each a frame, its `alt` text in it.
#[allow(clippy::too_many_arguments)]
fn paint_replaced(
    p: &mut Painter,
    laid: &Laid,
    measure: &mut Measure,
    frame: &Frame,
    line: &Line,
    clip: Rect,
    ground: u32,
    marks: &Marks,
) {
    let styled = laid.styled();
    for r in line.replaced.iter() {
        if !p.work(1) {
            return;
        }
        let Some(style) = styled.style(r.style) else {
            continue;
        };
        if hidden(style) {
            continue;
        }
        let Some(rect) = frame.rect(r.x, r.y, r.width, r.height) else {
            continue;
        };
        let Some(inner) = rect.intersection(clip) else {
            continue;
        };
        let one = u32::try_from(frame.scale.max(1)).unwrap_or(1);
        let selected = laid
            .tree()
            .link(r.node)
            .is_some_and(|l| Some(l) == marks.link);
        let frame_colour = if selected { SELECTED } else { BORDER };
        for side in [
            Rect {
                height: one.min(rect.height),
                ..rect
            },
            Rect {
                y: rect.y + i64::from(rect.height.saturating_sub(one)),
                height: one.min(rect.height),
                ..rect
            },
            Rect {
                width: one.min(rect.width),
                ..rect
            },
            Rect {
                x: rect.x + i64::from(rect.width.saturating_sub(one)),
                width: one.min(rect.width),
                ..rect
            },
        ] {
            p.fill(side, frame_colour, clip);
        }
        if let Some(alt) = laid.tree().alt(r.node).filter(|alt| !alt.is_empty()) {
            let font = measure.font(style);
            let ink = blend(style.color(), ground).map_or(ground, |c| p.colour(c));
            let baseline = frame.y(line.y + line.baseline);
            let ground = p.colour(ground);
            paint_run(
                p, measure, frame, font, alt, r.x, baseline, ink, ground, inner,
            );
        }
    }
}

/// Paints `laid` into `frame`'s pane within `damage`.
pub fn paint(
    laid: &Laid,
    measure: &mut Measure,
    frame: &Frame,
    damage: Rect,
    marks: &Marks,
    scratch: &mut Scratch,
    sink: &mut dyn FnMut(Draw),
) -> Painted {
    let Some(damage) = damage.intersection(frame.pane) else {
        return Painted::default();
    };
    let styled = laid.styled();
    if let Some(glyphs) = scratch.glyphs.as_mut() {
        glyphs.clear();
    }
    let mut p = Painter::new(
        sink,
        frame,
        damage,
        styled.coloured,
        scratch.glyphs.as_mut(),
    );
    let canvas = canvas(laid);
    let mut grounds = Chain::new(canvas);
    let mut tints = Chain::new(Tint::CLEAR);
    let shown_canvas = p.colour(canvas);
    p.fill(frame.pane, shown_canvas, frame.pane);
    let scale = frame.scale.max(1) as f64;
    let top = (damage.y - frame.y) as f64 / scale;
    let bottom = (damage.y + i64::from(damage.height) - frame.y) as f64 / scale;
    // Boxes in tree order.
    // Only what meets the damage is visited: what a chunk holds beside it
    // is passed by as work.
    let painted = laid.painted();
    let spans = laid.box_spans();
    'boxes: for chunk in meeting(spans, top, bottom) {
        let from = chunk * CHUNK;
        for &at in painted
            .get(from..(from + CHUNK).min(painted.len()))
            .unwrap_or(&[])
        {
            let g = laid.geometry(at);
            let meets =
                g.is_some_and(|g| g.border.y < bottom && g.border.y + g.border.height > top);
            if !p.pass(meets) {
                break 'boxes;
            }
            if let Some(g) = g.filter(|_| meets) {
                paint_box(&mut p, &mut grounds, laid, frame, at, g, canvas);
            }
        }
    }
    // Each line's content.
    let lines = laid.lines();
    let spans = laid.line_spans();
    'lines: for chunk in meeting(spans, top, bottom) {
        let Some(&span) = spans.get(chunk) else {
            break;
        };
        let from = chunk * CHUNK;
        for line in lines
            .get(from..(from + CHUNK).min(lines.len()))
            .unwrap_or(&[])
        {
            // A line's ink reaches past its box at most as far as its
            // chunk's.
            let meets = span.reaches(line.y, line.y + line.height, top, bottom);
            if !p.pass(meets) {
                break 'lines;
            }
            if !meets {
                continue;
            }
            let Some(clip) = frame.clip(laid.line_clip(line)) else {
                continue;
            };
            let block = line
                .block()
                .and_then(|b| styled.node_of(b.index()))
                .and_then(|b| laid.geometry_of(b.node));
            let Some(ground) = ground_of(&mut p, &mut grounds, laid, block, canvas) else {
                break 'lines;
            };
            // At most MAX_STACKED glyphs at a pen, across the fragments
            // td-layout splits a run into.
            let mut stack = Stack::NEW;
            for f in line.fragments.iter() {
                if p.cut {
                    break 'lines;
                }
                paint_text(
                    &mut p, laid, measure, frame, line, f, clip, ground, marks, &mut tints,
                    &mut stack,
                );
            }
            paint_replaced(&mut p, laid, measure, frame, line, clip, ground, marks);
        }
    }
    // List markers, in their items' style.
    let markers = &laid.layout.markers;
    let spans = laid.marker_spans();
    'markers: for chunk in meeting(spans, top, bottom) {
        let Some(&span) = spans.get(chunk) else {
            break;
        };
        let from = chunk * CHUNK;
        for marker in markers
            .get(from..(from + CHUNK).min(markers.len()))
            .unwrap_or(&[])
        {
            let (box_top, box_bottom) = marker_box(marker);
            let meets = span.reaches(box_top, box_bottom, top, bottom);
            if !p.pass(meets) {
                break 'markers;
            }
            if !meets {
                continue;
            }
            let Some(style) = styled.style(marker.style) else {
                continue;
            };
            if hidden(style) {
                continue;
            }
            let item = laid.geometry_of(marker.item);
            let clip = item
                .and_then(|at| laid.parent(at))
                .map_or(Some(frame.pane), |parent| frame.clip(laid.clip(parent)));
            let Some(clip) = clip else {
                continue;
            };
            let Some(ground) = ground_of(&mut p, &mut grounds, laid, item, canvas) else {
                break 'markers;
            };
            let Some(ink) = blend(style.color(), ground) else {
                continue;
            };
            let font = measure.font(style);
            let text = marker.text();
            let ink = p.colour(ink);
            let ground = p.colour(ground);
            paint_run(
                &mut p,
                measure,
                frame,
                font,
                text.as_str(),
                marker.x,
                frame.y(marker.baseline),
                ink,
                ground,
                clip,
            );
        }
    }
    p.finish()
}

pub fn reserve(scratch: &mut Scratch, memory: &Budget) -> Result<(), BudgetError> {
    if scratch.glyphs.is_none() {
        scratch.glyphs = Some(td_alloc::BudgetVec::with_capacity(memory, MAX_UNORDERED)?);
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::page::tests::{laid, WANT};
    use crate::pipeline::tests::measure;

    fn frame() -> Frame {
        Frame {
            pane: Rect {
                x: 0,
                y: 0,
                width: 400,
                height: 300,
            },
            x: 0,
            y: 0,
            scale: 1,
        }
    }

    fn draws(markup: &str, marks: Marks) -> (Vec<Draw>, Painted) {
        draws_in(markup, marks, true)
    }

    /// `draws`, the glyphs in order only when `ordered`.
    fn draws_in(markup: &str, marks: Marks, ordered: bool) -> (Vec<Draw>, Painted) {
        let laid = laid(markup);
        let memory = Budget::new(64 << 20).unwrap();
        let mut scratch = if ordered {
            Scratch::new(&memory)
        } else {
            Scratch::new_empty()
        };
        let mut out = Vec::new();
        let painted = paint(
            &laid,
            &mut measure(WANT),
            &frame(),
            frame().pane,
            &marks,
            &mut scratch,
            &mut |d| out.push(d),
        );
        (out, painted)
    }

    fn glyphs(draws: &[Draw]) -> String {
        draws
            .iter()
            .filter_map(|d| match d.primitive {
                Primitive::Glyph { scalar, .. } => Some(scalar),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_canvas_boxes_and_glyphs_are_drawn_glyphs_after_fills() {
        let (out, painted) = draws(
            "<body style='background:#102030;margin:0'><p style='border:2px solid #405060'>hi",
            Marks::default(),
        );
        assert_eq!(painted.drawn, out.len());
        assert!(!painted.cut);
        match out[0].primitive {
            Primitive::Fill { color, .. } => assert_eq!(color, 0x102030),
            _ => panic!("the canvas first"),
        }
        assert!(out.iter().any(|d| matches!(
            d.primitive,
            Primitive::Fill {
                color: 0x405060,
                ..
            }
        )));
        assert_eq!(glyphs(&out), "hi");
        let first_glyph = out
            .iter()
            .position(|d| matches!(d.primitive, Primitive::Glyph { .. }))
            .unwrap();
        assert!(out[first_glyph..]
            .iter()
            .all(|d| matches!(d.primitive, Primitive::Glyph { .. })));
    }

    #[test]
    fn colours_on_the_palette_keys_move_off_them_once_a_page_sets_colours() {
        let ink = td_ui::raster::INK;
        let (plain, _) = draws("<p>x", Marks::default());
        assert!(plain
            .iter()
            .any(|d| matches!(d.primitive, Primitive::Glyph { style, .. } if style.ink == ink)));
        let (styled, _) = draws("<p style=color:#48453f>x", Marks::default());
        assert!(styled.iter().any(
            |d| matches!(d.primitive, Primitive::Glyph { style, .. } if style.ink == ink ^ 1)
        ));
        assert!(matches!(styled[0].primitive, Primitive::Fill { color, .. } if color == PAPER ^ 1));
    }

    #[test]
    fn hidden_text_and_transparent_ink_draw_nothing_and_alpha_blends() {
        let (out, _) = draws(
            "<p style=visibility:hidden>gone<p style='color:rgba(0,0,0,0)'>clear",
            Marks::default(),
        );
        assert_eq!(glyphs(&out), "");
        assert_eq!(
            blend(
                td_css::values::Color {
                    red: 255,
                    green: 255,
                    blue: 255,
                    alpha: 128
                },
                0
            ),
            Some(0x808080)
        );
    }

    #[test]
    fn a_selection_is_drawn_selected_in_paper_ink() {
        let laid = laid("<p>abc");
        let (start, end) = text::page_bounds(&laid);
        let (out, _) = draws(
            "<p>abc",
            Marks {
                selection: Some((start, end)),
                ..Marks::default()
            },
        );
        assert!(out.iter().any(|d| matches!(
            d.primitive,
            Primitive::Fill {
                color: SELECTED,
                ..
            }
        )));
        assert!(out
            .iter()
            .all(|d| !matches!(d.primitive, Primitive::Glyph { style, .. } if style.ink != PAPER)));
    }

    fn fills(draws: &[Draw]) -> Vec<(Rect, u32)> {
        draws
            .iter()
            .filter_map(|d| match d.primitive {
                Primitive::Fill { color, .. } => Some((d.clip, color)),
                _ => None,
            })
            .collect()
    }

    /// A painter over a 400x300 pane into `out`.
    fn painter<'s>(out: &'s mut dyn FnMut(Draw)) -> Painter<'s, 'static> {
        Painter::new(out, &frame(), frame().pane, false, None)
    }

    fn dot(x: i64, y: i64) -> Rect {
        Rect {
            x,
            y,
            width: 1,
            height: 1,
        }
    }

    fn font_style() -> GlyphStyle {
        let laid = laid("<p>x");
        let style = laid
            .styled()
            .style(laid.lines()[0].fragments[0].style)
            .unwrap();
        measure(WANT).font(style).glyph(0, PAPER)
    }

    #[test]
    fn glyphs_are_drawn_by_family_then_size_then_the_order_they_came_in() {
        let (out, _) = draws(
            "<p style=margin:0>a<span style=font-size:30px>b</span>\
             <span style=font-family:monospace>c</span>d\
             <span style=font-family:serif>e</span><span style=font-size:30px>f</span>",
            Marks::default(),
        );
        // Sans at its size (a, d), Sans at 30px (b, f), Serif, Mono.
        assert_eq!(glyphs(&out), "adbfec");
        let families: Vec<TextFamily> = out
            .iter()
            .filter_map(|d| match d.primitive {
                Primitive::Glyph { style, .. } => Some(style.family),
                _ => None,
            })
            .collect();
        assert_eq!(
            families,
            [
                TextFamily::Sans,
                TextFamily::Sans,
                TextFamily::Sans,
                TextFamily::Sans,
                TextFamily::Serif,
                TextFamily::Mono
            ]
        );
    }

    #[test]
    fn without_the_scratch_glyphs_are_drawn_as_they_come_after_the_fills_before_them() {
        let laid =
            laid("<p style='margin:0;background:#102030'>a<span style=font-size:30px>b</span>c");
        let mut out = Vec::new();
        paint(
            &laid,
            &mut measure(WANT),
            &frame(),
            frame().pane,
            &Marks::default(),
            &mut Scratch::new_empty(),
            &mut |d| out.push(d),
        );
        assert_eq!(glyphs(&out), "abc");
        let first = out
            .iter()
            .position(|d| matches!(d.primitive, Primitive::Glyph { .. }))
            .unwrap();
        assert!(out[..first].iter().any(|d| matches!(
            d.primitive,
            Primitive::Fill {
                color: 0x102030,
                ..
            }
        )));
    }

    #[test]
    fn at_most_max_stacked_glyphs_are_drawn_at_one_pen() {
        let (out, _) = draws(
            &format!("<p style=letter-spacing:-100px>{}", "a".repeat(20)),
            Marks::default(),
        );
        let pens: Vec<i64> = out
            .iter()
            .filter_map(|d| match d.primitive {
                Primitive::Glyph { x, .. } => Some(x),
                _ => None,
            })
            .collect();
        assert_eq!(pens.len(), MAX_STACKED);
        assert!(pens.iter().all(|&x| x == pens[0]));
        // Counted a pen at a time: another pen starts again.
        let mut stack = Stack::NEW;
        assert!((0..MAX_STACKED).all(|_| stack.admits(5)));
        assert!(!stack.admits(5));
        assert!(stack.admits(6));
        assert!(stack.admits(5));
    }

    #[test]
    fn a_run_of_alt_text_stacks_at_most_max_stacked_glyphs_at_one_pen() {
        let mut measure = measure(WANT);
        let laid = laid("<p>x");
        let style = laid
            .styled()
            .style(laid.lines()[0].fragments[0].style)
            .unwrap();
        let font = measure.font(style);
        let mut out = Vec::new();
        let mut sink = |d| out.push(d);
        let mut p = painter(&mut sink);
        // Every scalar advances a cell on the cell grid: a frame of scale
        // zero puts every pen on one pixel.
        let one = Frame {
            scale: 0,
            ..frame()
        };
        paint_run(
            &mut p,
            &mut measure,
            &one,
            font,
            &"a".repeat(20),
            0.0,
            20,
            0,
            PAPER,
            frame().pane,
        );
        // Each scalar measured is work, drawn or not.
        assert_eq!(p.worked, 20);
        p.finish();
        assert_eq!(glyphs(&out), "a".repeat(MAX_STACKED));
    }

    /// A fragment and a replaced box looked at are work, though wholly
    /// outside the clip.
    #[test]
    fn fragments_and_replaced_boxes_looked_at_are_work() {
        let laid = laid("<p style=margin:0>ab<img width=10 height=10><img width=10 height=10>");
        let line = &laid.lines()[0];
        assert_eq!((line.fragments.len(), line.replaced.len()), (1, 2));
        let far = Frame {
            x: -10_000,
            ..frame()
        };
        let mut sink = |_| {};
        let mut p = painter(&mut sink);
        let mut tints = Chain::new(Tint::CLEAR);
        let mut measure = measure(WANT);
        let mut stack = Stack::NEW;
        paint_text(
            &mut p,
            &laid,
            &mut measure,
            &far,
            line,
            &line.fragments[0],
            frame().pane,
            PAPER,
            &Marks::default(),
            &mut tints,
            &mut stack,
        );
        assert_eq!(p.worked, 1);
        paint_replaced(
            &mut p,
            &laid,
            &mut measure,
            &far,
            line,
            frame().pane,
            PAPER,
            &Marks::default(),
        );
        assert_eq!(p.worked, 3);
        assert_eq!(p.taken, 0);
    }

    #[test]
    fn past_max_paint_draws_a_paint_is_cut_and_draws_nothing_more() {
        let mut count = 0usize;
        let mut sink = |_| count += 1;
        let mut p = painter(&mut sink);
        for n in 0..MAX_PAINT as i64 + 5 {
            p.fill(dot(n % 400, n / 400 % 300), 0x123456, frame().pane);
        }
        assert!(p.cut);
        assert_eq!(p.taken, MAX_PAINT);
        p.glyph(0, 0, 'a', font_style(), frame().pane);
        assert_eq!(p.taken, MAX_PAINT);
        let painted = p.finish();
        assert!(painted.cut);
        assert_eq!(painted.drawn, MAX_PAINT);
        assert_eq!(count, MAX_PAINT);
    }

    /// The glyphs' order's scratch is grown as a paint fills it, to
    /// `MAX_PAINT`, and kept; where it is not grown the glyphs past it are
    /// drawn as they come, at most `MAX_UNORDERED` of them.
    #[test]
    fn the_glyphs_order_grows_to_max_paint_and_past_it_few_are_unordered() {
        let style = font_style();
        // Glyphs in order are drawn at the finish; others as they come.
        let run = |memory: &Budget, glyphs: usize| {
            let mut scratch = Scratch::new(memory);
            let mut drawn = 0usize;
            let mut sink = |_| drawn += 1;
            let mut p = Painter::new(
                &mut sink,
                &frame(),
                frame().pane,
                false,
                scratch.glyphs.as_mut(),
            );
            // Left of the pane, so no pixels bound them.
            for _ in 0..glyphs {
                p.glyph(-1000, 0, 'a', style, frame().pane);
            }
            let unordered = p.drawn;
            let painted = p.finish();
            let kept = scratch.glyphs.as_ref().map_or(0, |g| g.capacity());
            (unordered, painted, kept)
        };
        let memory = Budget::new(64 << 20).unwrap();
        let (unordered, painted, kept) = run(&memory, 3 * MAX_UNORDERED);
        assert_eq!((unordered, painted.drawn), (0, 3 * MAX_UNORDERED));
        assert!(!painted.cut);
        assert_eq!(kept, MAX_PAINT);
        // A budget with room for the first scratch only.
        let size = std::mem::size_of::<Waiting>();
        let memory = Budget::new(MAX_UNORDERED * size + (64 << 10)).unwrap();
        let (unordered, painted, kept) = run(&memory, 2 * MAX_UNORDERED + 5);
        assert_eq!(unordered, MAX_UNORDERED);
        assert_eq!(painted.drawn, 2 * MAX_UNORDERED);
        assert!(painted.cut);
        assert_eq!(kept, MAX_UNORDERED);
    }

    #[test]
    fn past_panes_times_the_pane_s_pixels_a_paint_is_cut() {
        let most = PANES * 400 * 300;
        let mut out = Vec::new();
        let mut sink = |d| out.push(d);
        let mut p = painter(&mut sink);
        // Each a row lower, so none covers another.
        for n in 0..PANES as i64 + 2 {
            let rect = Rect {
                y: n,
                ..frame().pane
            };
            p.fill(rect, 0x123456, frame().pane);
        }
        assert!(p.cut);
        assert!(p.pixels <= most);
        // Refused, a fill that would cover what is held leaves it held.
        let held = p.holding;
        assert!(held > 0);
        p.fill(frame().pane, 0x654321, frame().pane);
        assert_eq!(p.holding, held);
        let painted = p.finish();
        assert!(painted.cut);
        let drawn: u64 = fills(&out).iter().map(|&(clip, _)| area(clip)).sum();
        assert_eq!(drawn, painted.pixels);
        assert!(drawn <= most);
        // A glyph charges its em square within its clip.
        let mut sink = |_| {};
        let mut p = painter(&mut sink);
        let style = font_style();
        let em = u64::from(style.size.pixels(Scale::default()));
        p.glyph(10, 10, 'a', style, frame().pane);
        assert_eq!(p.pixels, em * em);
        p.glyph(-2, 10, 'a', style, frame().pane);
        assert_eq!(p.pixels, em * em + (em - 2) * em);
    }

    #[test]
    fn a_page_of_overlapping_fills_is_cut_by_pixels_and_draws_nothing_after() {
        let page = format!(
            "<style>body{{margin:0}}div{{height:300px;margin-bottom:-299px;\
             background:#123456}}</style>{}<p>after",
            "<div></div>".repeat(40)
        );
        let (out, painted) = draws(&page, Marks::default());
        assert!(painted.cut);
        assert!(painted.pixels <= PANES * 400 * 300);
        assert_eq!(painted.drawn, out.len());
        assert_eq!(glyphs(&out), "");
    }

    /// Each box, line and marker visited counts against `MAX_PAINT`, so a
    /// paint of more of them laid on one another than that is cut though
    /// none draws: blank lines, hidden items.
    #[test]
    fn visits_that_draw_nothing_end_a_paint_at_max_paint() {
        let pages = |n: usize| {
            [
                format!("<pre style=line-height:0> {}</pre>", "\n ".repeat(n)),
                // An item is a box and a marker: half as many items cut a
                // paint only if both count.
                format!(
                    "<ul style='visibility:hidden;line-height:0'>{}",
                    "<li style=margin:0>".repeat(n / 2)
                ),
            ]
        };
        for (page, small) in pages(MAX_PAINT + 10).iter().zip(pages(100)) {
            let laid = laid(page);
            assert!(
                laid.lines().len() > MAX_PAINT
                    || laid.painted().len() + laid.layout.markers.len() > MAX_PAINT
            );
            assert!(laid.painted().len() < MAX_PAINT || laid.lines().is_empty());
            let (out, painted) = draws(page, Marks::default());
            assert!(painted.cut, "{}", &page[..40]);
            assert_eq!(painted.drawn, 1, "the canvas alone");
            assert_eq!(out.len(), 1);
            let (_, painted) = draws(&small, Marks::default());
            assert!(!painted.cut);
        }
    }

    /// A held fill that pokes past any edge of a later fill is drawn; one
    /// it wholly covers is not.
    #[test]
    fn a_fill_past_any_edge_of_a_later_one_is_drawn() {
        let later = Rect {
            x: 10,
            y: 10,
            width: 10,
            height: 10,
        };
        let poking = [
            Rect { x: 9, ..later },
            Rect { y: 9, ..later },
            Rect { width: 11, ..later },
            Rect {
                height: 11,
                ..later
            },
        ];
        for earlier in poking {
            assert!(!covers(later, earlier), "{earlier:?}");
            let mut out = Vec::new();
            let mut sink = |d| out.push(d);
            let mut p = painter(&mut sink);
            p.fill(earlier, 1, frame().pane);
            p.fill(later, 2, frame().pane);
            p.finish();
            let colours: Vec<u32> = fills(&out).iter().map(|&(_, c)| c).collect();
            assert_eq!(colours, [1, 2], "{earlier:?}");
        }
        let inside = Rect {
            x: 11,
            y: 11,
            width: 9,
            height: 9,
        };
        let mut out = Vec::new();
        let mut sink = |d| out.push(d);
        let mut p = painter(&mut sink);
        p.fill(inside, 1, frame().pane);
        p.fill(later, 2, frame().pane);
        p.finish();
        let colours: Vec<u32> = fills(&out).iter().map(|&(_, c)| c).collect();
        assert_eq!(colours, [2]);
    }

    /// A fill the pixel bound refuses, with room left for a draw, is not
    /// drawn and drops none of the fills it would cover.
    #[test]
    fn a_fill_refused_by_pixels_drops_nothing_it_covers() {
        let mut out = Vec::new();
        let mut sink = |d| out.push(d);
        let mut p = painter(&mut sink);
        p.most = 10;
        p.fill(dot(0, 0), 1, frame().pane);
        let covering = Rect {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        };
        assert!(covers(covering, dot(0, 0)));
        // 16 pixels less the dot's one is past 10.
        p.fill(covering, 2, frame().pane);
        assert!(p.cut);
        assert_eq!((p.taken, p.holding, p.pixels), (1, 1, 1));
        let painted = p.finish();
        let colours: Vec<u32> = fills(&out).iter().map(|&(_, c)| c).collect();
        assert_eq!(colours, [1]);
        assert_eq!(painted.pixels, 1);
    }

    /// A translucent box is blended over its nearest opaque ancestor's
    /// background, not the canvas.
    #[test]
    fn a_translucent_box_is_blended_over_its_ground() {
        let (out, _) = draws(
            "<body style=margin:0><div style=background:#000080>\
             <div style='background:rgba(255,0,0,0.5);height:10px'></div>",
            Marks::default(),
        );
        let red = td_css::values::Color {
            red: 255,
            green: 0,
            blue: 0,
            alpha: 128,
        };
        let over = blend(red, 0x00_0080).unwrap();
        assert_ne!(Some(over), blend(red, PAPER));
        let colours: Vec<u32> = fills(&out).iter().map(|&(_, c)| c).collect();
        assert_eq!(colours.last(), Some(&over), "{colours:x?}");
    }

    /// Lines and markers are culled by their glyphs' ink, drawn from
    /// td-ui's `top_at` down its line box at their size, which a tight or
    /// overlapping `line-height` makes pass td-layout's line box: what a
    /// paint of the whole page draws over the pane, a culled one draws,
    /// at either edge, at several sizes, and few more.
    #[test]
    fn culled_lines_draw_every_glyph_whose_ink_meets_the_pane() {
        let mut measure = measure(WANT);
        let height = i64::from(measure.metrics().line().height);
        let glyphs_in = |laid: &Laid, frame: &Frame, measure: &mut Measure| {
            let mut out = Vec::new();
            let memory = Budget::new(64 << 20).unwrap();
            let painted = paint(
                laid,
                measure,
                frame,
                frame.pane,
                &Marks::default(),
                &mut Scratch::new(&memory),
                &mut |d| {
                    if let Primitive::Glyph { x, y, scalar, .. } = d.primitive {
                        out.push((y, x, scalar));
                    }
                },
            );
            assert!(!painted.cut);
            out.sort_unstable();
            out
        };
        let mut pages = Vec::new();
        for size in ["8px", "16px", "30px"] {
            for line_height in ["0", "2px", "8px", "1", "1.2"] {
                pages.push(format!(
                    "<pre style='margin:0;font-size:{size};line-height:{line_height}'>{}</pre>",
                    "abcd\n".repeat(200)
                ));
            }
            pages.push(format!(
                "<style>p{{margin:0 0 -6px;line-height:4px;font-size:{size}}}</style>{}",
                "<p>abcd".repeat(200)
            ));
            pages.push(format!(
                "<ul style='margin:0;line-height:2px;font-size:{size}'>{}</ul>",
                "<li>ab".repeat(200)
            ));
        }
        for page in &pages {
            let laid = laid(page);
            let end = laid.lines().last().map_or(0, |l| (l.y + l.height) as i64);
            let mut any = false;
            // Past the top edge, centred, and the page's end past the bottom.
            for y in [-100, 150 - end / 2, 290 - end] {
                let scrolled = Frame { y, ..frame() };
                let drawn = glyphs_in(&laid, &scrolled, &mut measure);
                let whole = Frame {
                    pane: Rect {
                        height: 1 << 20,
                        y: -(1 << 19),
                        ..frame().pane
                    },
                    ..scrolled
                };
                let pane = frame().pane;
                let meets = |&(y, _, _): &(i64, i64, char)| {
                    y < pane.y + i64::from(pane.height) && y + height > pane.y
                };
                let all = glyphs_in(&laid, &whole, &mut measure);
                let wanted: Vec<_> = all.iter().filter(|g| meets(g)).collect();
                let seen: Vec<_> = drawn.iter().filter(|g| meets(g)).collect();
                assert_eq!(seen, wanted, "{} at {y}", &page[..60]);
                // Past them, the lines whose boxes meet it, a few at each
                // edge.
                assert!(drawn.len() - seen.len() <= 16, "{} at {y}", &page[..60]);
                any |= !wanted.is_empty();
            }
            assert!(any, "{}", &page[..60]);
        }
    }

    /// A list marker is drawn on its item's first line's baseline,
    /// td-layout's absolute `MarkerFragment::baseline`, however far down
    /// the page.
    #[test]
    fn markers_are_drawn_on_their_items_baselines() {
        let (out, _) = draws(
            "<ul style=margin:0>\
             <li>a<li>b<li>c<li>d<li>e<li>f<li>g<li>h<li>i<li>j",
            Marks::default(),
        );
        let mut tops: Vec<(char, i64)> = Vec::new();
        for d in &out {
            if let Primitive::Glyph { y, scalar, .. } = d.primitive {
                tops.push((scalar, y));
            }
        }
        let bullets: Vec<i64> = tops.iter().filter(|g| g.0 == '•').map(|g| g.1).collect();
        let letters: Vec<i64> = tops.iter().filter(|g| g.0 != '•').map(|g| g.1).collect();
        assert_eq!(bullets.len(), 10);
        assert_eq!(bullets, letters);
    }

    /// At most `MAX_STACKED` glyphs are drawn at one pen in a line, across
    /// the fragments td-layout splits a long run into.
    #[test]
    fn glyphs_stacked_at_one_pen_are_bounded_across_a_line_s_fragments() {
        let page = format!(
            "<pre style='margin:0;letter-spacing:-100px'>x{}</pre>",
            "a".repeat(2 * td_layout::MAX_FRAGMENT_SCALARS)
        );
        let laid = laid(&page);
        assert!(laid.lines()[0].fragments.len() > 1);
        let (out, _) = draws(&page, Marks::default());
        assert_eq!(glyphs(&out).chars().count(), MAX_STACKED);
    }

    /// Of a chunk of lines meeting the damage, only those whose ink can
    /// meet it are visited and drawn, not the chunk's lines above or
    /// below it.
    #[test]
    fn only_the_lines_meeting_the_damage_are_drawn() {
        let laid = laid(&format!(
            "<pre style='margin:0;line-height:10px'>{}</pre>",
            "xxxxxxxxxx\n".repeat(64)
        ));
        // Lines 60 to 63 at the pane's top.
        let at = Frame { y: -600, ..frame() };
        let mut glyphs = 0;
        let memory = Budget::new(64 << 20).unwrap();
        paint(
            &laid,
            &mut measure(WANT),
            &at,
            frame().pane,
            &Marks::default(),
            &mut Scratch::new(&memory),
            &mut |d| {
                if let Primitive::Glyph { .. } = d.primitive {
                    glyphs += 1;
                }
            },
        );
        // The four, and a line on either side its ink may reach from.
        assert!((40..=60).contains(&glyphs), "{glyphs}");
    }

    /// The boxes of a chunk meeting the damage that do not meet it are
    /// passed by, not visited: with them, the lines visited after them
    /// pass `MAX_PAINT`, and the text after those is drawn.
    #[test]
    fn boxes_beside_the_damage_are_passed_by() {
        let unit = format!(
            "<div style='height:2000px;margin-bottom:-2000px'></div>{}",
            "<div></div>".repeat(31)
        );
        let units = 6_500;
        let lines = 70_000;
        assert!(32 * units + lines > MAX_PAINT);
        let page = format!(
            "<style>body,div{{margin:0}}</style>{}<pre style=line-height:0>{}hello",
            unit.repeat(units),
            " \n".repeat(lines)
        );
        let (out, painted) = draws(&page, Marks::default());
        assert!(!painted.cut);
        assert_eq!(glyphs(&out), "hello");
    }

    /// A line's fragments wholly before its clip are passed by their x,
    /// so it replays at most td-layout's `MAX_FRAGMENT_SCALARS` before
    /// the clip: lines whose scalars together pass `MAX_PAINT_WORK`,
    /// scrolled to their ends, are painted whole. Lines laid on one
    /// another, each replaying that much to meet the clip, are still cut.
    #[test]
    fn wide_lines_scrolled_to_their_ends_paint_whole_and_stacked_ones_are_cut() {
        let paint_right = |page: &str| {
            let laid = laid(page);
            let width = laid.lines().iter().map(|l| l.width).fold(0.0, f64::max) as i64;
            let at = Frame {
                x: 400 - width,
                ..frame()
            };
            let mut drawn = String::new();
            let memory = Budget::new(64 << 20).unwrap();
            let painted = paint(
                &laid,
                &mut measure(WANT),
                &at,
                frame().pane,
                &Marks::default(),
                &mut Scratch::new(&memory),
                &mut |d| {
                    if let Primitive::Glyph { scalar, .. } = d.primitive {
                        drawn.push(scalar);
                    }
                },
            );
            (painted, drawn)
        };
        let long = 60_000;
        let lines = MAX_PAINT_WORK / long + 2;
        let wide = format!(
            "<pre style='margin:0;line-height:10px'>{}</pre>",
            format!("{}z\n", "a".repeat(long - 1)).repeat(lines)
        );
        let (painted, drawn) = paint_right(&wide);
        assert!(!painted.cut);
        assert_eq!(drawn.matches('z').count(), lines);
        let most = td_layout::MAX_FRAGMENT_SCALARS;
        let stacked = format!(
            "<pre style='margin:0;line-height:0'>{}</pre>",
            format!("{}z\n", " ".repeat(2 * most - 1)).repeat(MAX_PAINT_WORK / most + 64)
        );
        let (painted, drawn) = paint_right(&stacked);
        assert!(painted.cut);
        assert!(drawn.len() < MAX_PAINT_WORK / most);
    }

    /// Grounds asked for in any order are the nearest opaque ancestor's,
    /// and asked for in tree order a paint steps through each geometry
    /// once; inline tints the same.
    #[test]
    fn grounds_and_tints_are_walked_once_a_paint_in_tree_order() {
        let mut page = String::from("<style>*{margin:0}</style>");
        for n in 0..200 {
            page.push_str(match n % 7 {
                0 => "<div style=background:#000010>",
                3 => "<div style='background:rgba(0,0,0,0.5)'>",
                _ => "<div>",
            });
            for _ in 0..3 {
                page.push_str(match n % 5 {
                    0 => "<p style=background:#100000></p>",
                    _ => "<p></p>",
                });
            }
        }
        let laid = laid(&page);
        let canvas = canvas(&laid);
        let walked = |at: u32| {
            let mut at = Some(at);
            while let Some(here) = at {
                if let Some(c) = opaque(&laid, here) {
                    return c;
                }
                at = laid.parent(here);
            }
            canvas
        };
        let mut sink = |_| {};
        let mut p = painter(&mut sink);
        let mut grounds = Chain::new(canvas);
        let painted = laid.painted();
        assert!(painted.len() > 800);
        for &at in painted {
            let ground = ground_of(&mut p, &mut grounds, &laid, Some(at), canvas);
            assert_eq!(ground, Some(walked(at)));
        }
        // Each a step to its parent, held.
        assert_eq!(p.worked, painted.len());
        let mut grounds = Chain::new(canvas);
        for &at in painted.iter().rev() {
            let ground = ground_of(&mut p, &mut grounds, &laid, Some(at), canvas);
            assert_eq!(ground, Some(walked(at)));
        }
        // 200 spans deep, translucent and opaque, each over a fragment.
        let mut page = String::from("<p style=margin:0>");
        for n in 0..200 {
            page.push_str(match n % 9 {
                0 => "<span style=background:#000010>a",
                4 => "<span style='background:rgba(255,0,0,0.5)'>b",
                _ => "<span>c",
            });
        }
        let inline = crate::page::tests::laid(&page);
        let styled = inline.styled();
        let mut p = painter(&mut sink);
        let mut tints = Chain::new(Tint::CLEAR);
        let mut fragments = 0;
        for line in inline.lines() {
            for f in line.fragments.iter() {
                let index = styled.box_of(f.node).unwrap();
                let tint = inline_tint(&mut p, &mut tints, styled, index).unwrap();
                // As the backgrounds walked up from the innermost.
                let mut want = Tint::CLEAR;
                let mut chain = Vec::new();
                let mut at = styled.node_of(index).unwrap().parent;
                while let Some(id) = at {
                    let b = styled.node_of(id.index()).unwrap();
                    if b.kind != td_layout::BoxKind::Inline {
                        break;
                    }
                    chain.push(b.style);
                    at = b.parent;
                }
                for style in chain.iter().rev() {
                    if let Some(c) =
                        colour_of(styled.style(*style).unwrap(), Property::BackgroundColor)
                    {
                        want = want.with(c);
                    }
                }
                assert_eq!(tint.over(0x123456), want.over(0x123456));
                fragments += 1;
            }
        }
        assert_eq!(fragments, 200);
        // Each span a step, and the first the block's ancestors.
        assert!((200..210).contains(&p.worked), "{}", p.worked);
    }

    #[test]
    fn a_fill_under_a_later_one_that_covers_it_is_not_drawn() {
        // Every box at one place, each covering the last.
        let page = format!(
            "<style>body{{margin:0}}div{{height:3000px;margin-bottom:-3000px;\
             background:#123456}}</style>{}",
            "<div></div>".repeat(200)
        );
        let (out, painted) = draws(&page, Marks::default());
        assert!(!painted.cut);
        assert_eq!(fills(&out), [(frame().pane, 0x123456)]);
        assert_eq!(painted.pixels, 400 * 300);
        // Borders the same: each side drawn once.
        let page = format!(
            "<style>body{{margin:0}}div{{height:3000px;margin-bottom:-3020px;\
             border:10px solid #654321}}</style>{}",
            "<div></div>".repeat(200)
        );
        let (out, painted) = draws(&page, Marks::default());
        assert!(!painted.cut);
        let sides = fills(&out).iter().filter(|&&(_, c)| c == 0x654321).count();
        assert_eq!(sides, 3, "{:?}", fills(&out));
        // What a fill does not wholly cover is drawn.
        let mut out = Vec::new();
        let mut sink = |d| out.push(d);
        let mut p = painter(&mut sink);
        p.fill(dot(0, 0), 1, frame().pane);
        p.fill(dot(1, 0), 2, frame().pane);
        p.fill(
            Rect {
                x: 1,
                y: 0,
                width: 5,
                height: 5,
            },
            3,
            frame().pane,
        );
        p.finish();
        let colours: Vec<u32> = fills(&out).iter().map(|&(_, c)| c).collect();
        assert_eq!(colours, [1, 3]);
    }

    #[test]
    fn past_held_fills_the_oldest_is_drawn_first() {
        let mut out = Vec::new();
        let mut sink = |d| out.push(d);
        let mut p = painter(&mut sink);
        for n in 0..HELD as i64 + 3 {
            p.fill(dot(n, 0), n as u32, frame().pane);
        }
        assert_eq!(p.holding, HELD);
        p.finish();
        let colours: Vec<u32> = fills(&out).iter().map(|&(_, c)| c).collect();
        assert_eq!(colours, (0..HELD as u32 + 3).collect::<Vec<_>>());
    }

    #[test]
    fn a_line_s_glyphs_stop_at_its_box_s_clip() {
        let (out, painted) = draws(
            &format!(
                "<body style=margin:0><div style='width:40px;overflow:hidden;\
                 white-space:nowrap'>{}",
                "a".repeat(200)
            ),
            Marks::default(),
        );
        assert!(!painted.cut);
        let drawn: Vec<(i64, Rect)> = out
            .iter()
            .filter_map(|d| match d.primitive {
                Primitive::Glyph { x, .. } => Some((x, d.clip)),
                _ => None,
            })
            .collect();
        let em = i64::from(font_style().size.pixels(Scale::default()));
        assert!(drawn.len() >= 5, "{drawn:?}");
        for &(x, clip) in &drawn {
            assert!(x <= 40 + em, "{x}");
            assert!(clip.x + i64::from(clip.width) <= 40);
        }
    }

    #[test]
    fn a_find_match_on_a_dark_page_is_drawn_in_the_theme_s_ink_on_its_ground() {
        let page = "<body style='background:#000;color:#fff'><p>abc";
        let laid = laid(page);
        let f = &laid.lines()[0].fragments[0];
        let b = text::Place::new(&laid, f.node, f.range.start + 1);
        let c = text::Place::new(&laid, f.node, f.range.start + 2);
        let hits = [Hit { start: b, end: c }];
        let (out, _) = draws(
            page,
            Marks {
                hits: &hits,
                ..Marks::default()
            },
        );
        let inks: Vec<(char, u32, u32)> = out
            .iter()
            .filter_map(|d| match d.primitive {
                Primitive::Glyph { scalar, style, .. } => {
                    Some((scalar, style.ink, style.background))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            inks,
            [
                ('a', 0xffffff, 0),
                ('b', INK, INACTIVE_SELECTION),
                ('c', 0xffffff, 0)
            ]
        );
        assert!(fills(&out).iter().any(|&(_, c)| c == INACTIVE_SELECTION));
    }

    #[test]
    fn the_canvas_is_the_root_s_background_else_the_body_s() {
        let canvas_of = |page: &str| canvas(&laid(page));
        assert_eq!(canvas_of("<p>x"), PAPER);
        assert_eq!(canvas_of("<body style=background:#333333>"), 0x333333);
        assert_eq!(
            canvas_of("<html style=background:#111111><body style=background:#333333>"),
            0x111111
        );
        // A box shown before the body is not the body.
        assert_eq!(
            canvas_of(
                "<head style='display:block;background:#222222'><title>t</title></head>\
                 <body style=background:#333333>"
            ),
            0x333333
        );
        assert_eq!(
            canvas_of("<head style='display:block;background:#222222'></head><p>x"),
            PAPER
        );
    }

    #[test]
    fn every_inline_ancestor_s_background_shows_through_the_translucent_ones() {
        // Ten transparent spans inside an opaque one: the opaque one shows.
        let page = format!(
            "<p style=margin:0><span style=background:#ff0000>{}x",
            "<span style=background:transparent>".repeat(10)
        );
        let (out, _) = draws(&page, Marks::default());
        assert!(fills(&out).iter().any(|&(_, c)| c == 0xff0000));
        let ground = out.iter().find_map(|d| match d.primitive {
            Primitive::Glyph { style, .. } => Some(style.background),
            _ => None,
        });
        assert_eq!(ground, Some(0xff0000));
        // A translucent one over an opaque one, as blended in turn.
        let page = "<p style=margin:0><span style=background:#0000ff>\
                    <span style='background:rgba(255,0,0,0.5)'>x";
        let (out, _) = draws(page, Marks::default());
        let red = td_css::values::Color {
            red: 255,
            green: 0,
            blue: 0,
            alpha: 128,
        };
        let want = blend(red, 0x0000ff).unwrap();
        assert!(fills(&out).iter().any(|&(_, c)| c == want), "{want:06x}");
        // None, no fill.
        let plain = laid("<p>x");
        let index = plain
            .styled()
            .box_of(plain.lines()[0].fragments[0].node)
            .unwrap();
        let mut sink = |_| {};
        let mut p = painter(&mut sink);
        let mut tints = Chain::new(Tint::CLEAR);
        let tint = inline_tint(&mut p, &mut tints, plain.styled(), index).unwrap();
        assert_eq!(tint.over(0x123456), None);
    }
}
