use crate::bar::{self, BAR_HEIGHT};
use crate::help::Help;
use crate::launcher::{LaunchRequest, Launcher, LauncherAction};
use crate::layout::{Axis, Command, DropKind, Layout, Placement, Presentation, Rect, ViewLayout};
use crate::ui;
use crate::MAX_UI_DIMENSION;
use std::collections::BTreeMap;
use std::sync::Arc;

pub const SHM_ARGB8888: u32 = 0;
pub const SHM_XRGB8888: u32 = 1;
pub(crate) const GAP: usize = 24;
const BORDER: usize = 4;
/// One line of 2x glyphs with a little air, as the status bar is sized. The
/// tile keeps this band and the client gets what is left, so the number is
/// layout rather than decoration.
pub(crate) const TITLE_HEIGHT: usize = 20;
const TITLE_SCALE: usize = 2;
const TITLE_TEXT_TOP: usize = 3;
const TITLE_TEXT_LEFT: usize = 6;
const MAX_SCENE_BYTES: usize = 128 * 1024 * 1024;
pub const MAX_INPUT_REGION_OPERATIONS: usize = 256;
/// Longer than any title bar can show at any plausible width, and short
/// enough that a client cannot spend the compositor's memory on one. Counted
/// in CHARACTERS rather than bytes so the truncation cannot split a UTF-8
/// sequence and leave a string the renderer walks off the end of.
pub const MAX_TITLE_CHARS: usize = 256;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SurfaceKey {
    pub client: u64,
    pub object: u32,
}

pub struct Surface {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u8>,
    pub format: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SurfacePoint {
    pub key: SurfaceKey,
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InputRegion {
    operations: Vec<InputRegionOperation>,
}

pub type SharedInputRegion = Arc<InputRegion>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InputRegionOperation {
    add: bool,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl InputRegion {
    pub fn new() -> InputRegion {
        InputRegion {
            operations: Vec::new(),
        }
    }

    pub fn add(&mut self, x: i32, y: i32, width: i32, height: i32) -> bool {
        self.push(true, x, y, width, height)
    }

    pub fn subtract(&mut self, x: i32, y: i32, width: i32, height: i32) -> bool {
        self.push(false, x, y, width, height)
    }

    pub fn len(&self) -> usize {
        self.operations.len()
    }

    fn push(&mut self, add: bool, x: i32, y: i32, width: i32, height: i32) -> bool {
        if width <= 0 || height <= 0 || self.operations.len() >= MAX_INPUT_REGION_OPERATIONS {
            return false;
        }
        self.operations.push(InputRegionOperation {
            add,
            x,
            y,
            width,
            height,
        });
        true
    }

    fn contains(&self, x: i32, y: i32) -> bool {
        let x = i64::from(x);
        let y = i64::from(y);
        let mut contains = false;
        for operation in &self.operations {
            let left = i64::from(operation.x);
            let top = i64::from(operation.y);
            let right = left.saturating_add(i64::from(operation.width));
            let bottom = top.saturating_add(i64::from(operation.height));
            if x >= left && x < right && y >= top && y < bottom {
                contains = operation.add;
            }
        }
        contains
    }
}

const TITLE_FOCUSED: [u8; 4] = [0x50, 0x28, 0x60, 0];
const TITLE_UNFOCUSED: [u8; 4] = [0x38, 0x34, 0x3c, 0];
const TITLE_TEXT: [u8; 4] = [0xf0, 0xe8, 0xf8, 0];
/// The presentation buttons at a band's right end, and the ink they draw in.
/// `ON` marks the presentation the container is ALREADY in, so the band says
/// which of the three it is as well as offering the other two.
const BUTTON_WIDTH: usize = 16;
const BUTTON_INSET: usize = 3;
const BUTTON_INK: [u8; 4] = [0xc8, 0xc0, 0xd0, 0];
const BUTTON_INK_ON: [u8; 4] = [0x60, 0xd0, 0x60, 0];
/// The least height an icon can be drawn in. The stack is the demanding one:
/// two lines, a gap after each, and the body — five units of a line's own
/// thickness, of which three are inked. SIX pixels rather than five, because
/// at five the body comes out as thin as the lines above it and the picture
/// stops being two titles over a window.
const BUTTON_ICON_LEAST: usize = 6;

/// What a bare press on a title band landed on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BandPress {
    /// One of the presentation buttons, and the window whose band carries it.
    /// The window comes with it because the command acts on the container the
    /// FOCUSED leaf is in: a button pressed on an unfocused band has to move
    /// focus there first, or it would present some other container entirely.
    Button(SurfaceKey, Presentation),
    /// The band itself, which is the drag handle.
    Handle(SurfaceKey),
}

/// What a band's buttons do, left to right. Ordered so the two GROUPED
/// presentations sit together and the ungrouped one is LAST, nearest the
/// band's end, which is where a pointer travelling right lands soonest and
/// undoing is the commoner ask.
pub(crate) const BUTTONS: [Presentation; 3] = [
    Presentation::Stacked,
    Presentation::Tabbed,
    Presentation::Split,
];

/// A button's icon: what the container would LOOK like in that presentation,
/// drawn from the same rectangles the layout would use. A stack is two
/// collapsed titles over the window they leave, tabs are one row divided
/// across with a body under it, and a split is two tiles side by side. Nothing here is a glyph — a letter would name the chord
/// rather than the arrangement, and the chords are already on the help sheet.
fn draw_button(
    frame: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    slot: Rect,
    shows: Presentation,
    ink: [u8; 4],
) {
    // The icon is inset from the slot on every side, so adjacent buttons have a
    // gap between their marks without the slots themselves overlapping — the
    // slots are what the hit test uses, and a gap in THOSE would be a press
    // that lands on nothing between two buttons.
    let pad = BUTTON_INSET;
    let Some(icon) = inset(slot, pad) else {
        return;
    };
    let mut mark = |x: usize, y: usize, w: usize, h: usize| {
        ui::fill(frame, width, height, stride, (x, y, w, h), ink);
    };
    match shows {
        // Two collapsed titles and the window under them, which is what a
        // stack IS: bands in a run and one leaf given the rest. Drawn as two
        // thin lines over a THICK body rather than three equal bars — three
        // equal bars is a hamburger, a menu everywhere else, and it says
        // nothing about which of the three is shown.
        //
        // It fills the icon top to bottom, as the other two do. An earlier
        // form centred a short run inside the icon and left blank rows above
        // and below, which read as a smaller button beside its neighbours
        // rather than as a different picture.
        Presentation::Stacked => {
            let line = (icon.height / BUTTON_ICON_LEAST).max(1);
            let step = line.saturating_mul(2);
            mark(icon.x, icon.y, icon.width, line);
            mark(icon.x, icon.y.saturating_add(step), icon.width, line);
            // Exactly what the two lines and their gaps leave, with no floor
            // under it. A floor would be the wrong failure: `ui::fill` bounds
            // a mark to the FRAME rather than to the slot, so a body forced
            // thicker than the room left would land on the band or on the
            // neighbouring button. It cannot bind at any height the gate
            // admits — four units are spent and at least two remain — and
            // below the gate an unpainted body beats an escaped one.
            let body = icon.y.saturating_add(step.saturating_mul(2));
            mark(
                icon.x,
                body,
                icon.width,
                icon.y.saturating_add(icon.height).saturating_sub(body),
            );
        }
        // One row divided across, and the body it leaves below.
        Presentation::Tabbed => {
            let strip = icon.height / 3;
            let tab = icon.width.saturating_sub(1) / 2;
            mark(icon.x, icon.y, tab, strip);
            mark(
                icon.x.saturating_add(tab).saturating_add(1),
                icon.y,
                icon.width.saturating_sub(tab).saturating_sub(1),
                strip,
            );
            mark(
                icon.x,
                icon.y.saturating_add(strip).saturating_add(1),
                icon.width,
                icon.height.saturating_sub(strip).saturating_sub(1),
            );
        }
        // Two tiles side by side: the arrangement with no run at all.
        Presentation::Split => {
            let tile = icon.width.saturating_sub(1) / 2;
            mark(icon.x, icon.y, tile, icon.height);
            mark(
                icon.x.saturating_add(tile).saturating_add(1),
                icon.y,
                icon.width.saturating_sub(tile).saturating_sub(1),
                icon.height,
            );
        }
    }
}

/// `rect` shrunk by `pad` on every side, or `None` when that leaves nothing.
fn inset(rect: Rect, pad: usize) -> Option<Rect> {
    let width = rect.width.checked_sub(pad.saturating_mul(2))?;
    let height = rect.height.checked_sub(pad.saturating_mul(2))?;
    if width == 0 || height == 0 {
        return None;
    }
    Some(Rect {
        x: rect.x.saturating_add(pad),
        y: rect.y.saturating_add(pad),
        width,
        height,
    })
}

/// Where the buttons sit in `band`, in `BUTTONS` order, or `None` when the band
/// is too narrow to hold them beside a readable name.
///
/// Narrow rather than merely small: a tabbed run divides ONE strip between its
/// leaves, so a column of eight gives each tab a few dozen pixels, and buttons
/// drawn there would be the whole tab with the title squeezed out. Answering
/// `None` is what keeps that band a title; the keys still reach every
/// presentation, and one wider band elsewhere still carries the buttons.
///
/// Too SHORT counts as too narrow, and for a sharper reason than tidiness: a
/// tile clipped down to a sliver keeps a band and loses its client, and an icon
/// that cannot be drawn in it would leave the band's last 48 pixels answering a
/// press with nothing on screen to say they would.
///
/// One function rather than two, because the painter and the hit test must
/// agree exactly: a button drawn where nothing answers is a button that does
/// nothing when pressed, and there is nothing on screen to say so.
pub(crate) fn band_buttons(band: Rect) -> Option<[Rect; BUTTONS.len()]> {
    let strip = BUTTON_WIDTH.saturating_mul(BUTTONS.len());
    // Room for the buttons AND a name beside them. `TITLE_TEXT_LEFT` is where
    // the text starts; one CELL past it is the least that reads as a title
    // rather than as a clipped smear — a cell being a glyph at the scale the
    // titles are actually drawn, not at 1x, or the reserve is half of what it
    // was meant to be and the smear is what the band shows.
    let cell = ui::GLYPH_ADVANCE.saturating_mul(TITLE_SCALE);
    let least = strip.saturating_add(TITLE_TEXT_LEFT).saturating_add(cell);
    // The same height `draw_button` needs, asked HERE so the two cannot
    // disagree about whether this band has buttons at all.
    let tall_enough = band
        .height
        .checked_sub(BUTTON_INSET.saturating_mul(2))
        .is_some_and(|icon| icon >= BUTTON_ICON_LEAST);
    if band.width < least || !tall_enough {
        return None;
    }
    let left = band.x.saturating_add(band.width).saturating_sub(strip);
    let mut rects = [Rect {
        x: 0,
        y: 0,
        width: 0,
        height: 0,
    }; BUTTONS.len()];
    for (index, slot) in rects.iter_mut().enumerate() {
        *slot = Rect {
            x: left.saturating_add(index.saturating_mul(BUTTON_WIDTH)),
            y: band.y,
            width: BUTTON_WIDTH,
            height: band.height,
        };
    }
    Some(rects)
}

/// The rectangle a border wraps: the client's area and its band together. Only
/// decoration reads this — a border around the client area alone would leave a
/// window's own title bar outside its own frame, which is why `Placement` keeps
/// the two rects and this composes them rather than the other way round.
///
/// The two abut by construction — the band is carved off the top of the tile
/// — so the frame runs from the band's top to the client's bottom.
///
/// A STACKED leaf is the exception, and the placement is ASKED rather than
/// measured. Every band there sits in the run at the container's top while the
/// content is below all of them, so the run's LAST band abuts the content
/// exactly as an ordinary band does and adjacency cannot tell the two apart.
/// Joining it would put that leaf's border four pixels ABOVE its own band —
/// over the band of the leaf before it — and only when the last of a stack is
/// shown, so a stack's frame is the client area alone whichever leaf that is.
/// The band is then its own strip, which is how a stack reads: one title per
/// window, with the shown one framed beneath them all.
/// The placement a point is in: a band, or a client area that is actually
/// shown. A stacked-away leaf is its band and nothing else, so the two
/// rectangles are asked SEPARATELY rather than as one union — their union
/// would swallow the bands lying between them.
fn tile_at(placements: &[Placement], x: usize, y: usize) -> Option<usize> {
    placements.iter().position(|placement| {
        contains(placement.band, x, y) || (placement.visible && contains(placement.rect, x, y))
    })
}

/// What a release would do, and the block that says so. Held for the whole
/// gesture so the release applies exactly what was drawn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DropHint {
    target: SurfaceKey,
    kind: DropKind,
    area: Rect,
}

/// How thick the bar is for a drop that lands BETWEEN two windows rather than
/// over one. A run drop has no area of its own — it names a position in a list
/// — so the block marks the edge it would go in at.
const HINT_BAR: usize = 12;

/// The half of `frame` a `Beside` drop would take.
fn hint_half(frame: Rect, axis: Axis, before: bool) -> Rect {
    let half = |whole: usize| whole.saturating_add(1) / 2;
    match (axis, before) {
        (Axis::Horizontal, true) => Rect {
            width: half(frame.width),
            ..frame
        },
        (Axis::Horizontal, false) => {
            let width = half(frame.width);
            Rect {
                x: frame.x.saturating_add(frame.width.saturating_sub(width)),
                width,
                ..frame
            }
        }
        (Axis::Vertical, true) => Rect {
            height: half(frame.height),
            ..frame
        },
        (Axis::Vertical, false) => {
            let height = half(frame.height);
            Rect {
                y: frame.y.saturating_add(frame.height.saturating_sub(height)),
                height,
                ..frame
            }
        }
    }
}

/// The bar marking the edge of `frame` a leaf would slide in at, drawn along
/// the direction the run actually travels rather than along the container.
fn hint_bar(frame: Rect, run: Axis, before: bool) -> Rect {
    match run {
        Axis::Horizontal => {
            let width = HINT_BAR.min(frame.width);
            Rect {
                x: if before {
                    frame.x
                } else {
                    frame.x.saturating_add(frame.width.saturating_sub(width))
                },
                width,
                ..frame
            }
        }
        Axis::Vertical => {
            let height = HINT_BAR.min(frame.height);
            Rect {
                y: if before {
                    frame.y
                } else {
                    frame.y.saturating_add(frame.height.saturating_sub(height))
                },
                height,
                ..frame
            }
        }
    }
}

/// The block a drop kind promises over the target, given the direction that
/// target's own run travels.
///
/// A swap covers the target's whole frame: the two windows trade places, so the
/// dragged one really does end up there. Everything else is one of two shapes,
/// and which one is decided by the TREE rather than by the zone that was aimed,
/// because `insert_beside` only SPLITS where the asked-for axis differs from
/// the target's own container and the target is not in a group. Anywhere else
/// it is a plain insert into a run, where the dragged window takes a whole slot
/// and every sibling shrinks — so promising the half the pointer was over would
/// be a picture the release cannot keep. A split gets that half; an insert gets
/// a bar on the edge it goes in at.
///
/// A GROUPED target then differs once more in WHERE the bar goes. Its leaves
/// all share one content rectangle, which `frame_rect` hands back, while the
/// run itself is the BANDS at the container's top — down them for a stack,
/// across them for tabs — so a bar on the content rectangle would mark a place
/// the new band does not appear. It is drawn on the target's own band instead,
/// along the direction the placement says its run travels. The swap stays the
/// exception in the tree too, keeping the content rectangle it really does
/// take.
fn hint_area(placement: &Placement, kind: DropKind, run: Option<Axis>) -> Rect {
    let frame = frame_rect(placement);
    let bar = |before| match placement.run {
        Some(along) => hint_bar(placement.band, along, before),
        None => hint_bar(frame, run.unwrap_or(Axis::Vertical), before),
    };
    match kind {
        DropKind::Swap => frame,
        DropKind::InRun { before } => bar(before),
        // A grouped target DISCARDS the drop's axis, which is what the tree
        // does: `insert_beside` refuses the cross-axis split inside a group,
        // so every edge drop onto one joins the run and the bar goes the way
        // the run does, not the way the pointer asked.
        DropKind::Beside { axis, before } if placement.run.is_some() || run == Some(axis) => {
            bar(before)
        }
        DropKind::Beside { axis, before } => hint_half(frame, axis, before),
    }
}

/// A tile's five zones. The middle NINTH is the swap, and outside it the
/// nearest edge picks the side — so every point in the tile answers, and the
/// answer changes only at a boundary rather than at a distance from one.
///
/// A ninth rather than a half-sized centre because the edges are what the
/// operator aims at: a drop that means "put it below this" wants most of the
/// bottom of the tile to say so, and a swap is the one gesture with a
/// deliberate target to hit. Ties go to the earlier edge in the order left,
/// right, top, bottom. Distances are scaled by the tile's own size, so the
/// tie locus is that tile's DIAGONALS whatever its proportions rather than a
/// square one's, and a tie has to go somewhere.
///
/// Degenerate tiles answer `Swap`: a rect with no width or height has no
/// edges to be nearer one of, and a swap is the drop that needs no room.
fn zone_of(rect: Rect, x: usize, y: usize) -> DropKind {
    if rect.width == 0 || rect.height == 0 {
        return DropKind::Swap;
    }
    // Thirds by multiplication rather than division, so a tile whose width is
    // not divisible by three still splits at the same two boundaries the
    // comparisons below use.
    let dx = x.saturating_sub(rect.x);
    let dy = y.saturating_sub(rect.y);
    let left = dx.saturating_mul(3) < rect.width;
    let right = dx.saturating_mul(3) >= rect.width.saturating_mul(2);
    let top = dy.saturating_mul(3) < rect.height;
    let bottom = dy.saturating_mul(3) >= rect.height.saturating_mul(2);
    if !left && !right && !top && !bottom {
        return DropKind::Swap;
    }
    // Scaled to the tile's own size before comparing, or the nearest edge of
    // a wide short tile would always be one of the tall pair.
    let to_left = dx.saturating_mul(rect.height);
    let to_right = rect
        .width
        .saturating_sub(1)
        .saturating_sub(dx.min(rect.width.saturating_sub(1)))
        .saturating_mul(rect.height);
    let to_top = dy.saturating_mul(rect.width);
    let to_bottom = rect
        .height
        .saturating_sub(1)
        .saturating_sub(dy.min(rect.height.saturating_sub(1)))
        .saturating_mul(rect.width);
    let nearest = to_left.min(to_right).min(to_top).min(to_bottom);
    if to_left == nearest {
        return DropKind::Beside {
            axis: Axis::Horizontal,
            before: true,
        };
    }
    if to_right == nearest {
        return DropKind::Beside {
            axis: Axis::Horizontal,
            before: false,
        };
    }
    if to_top == nearest {
        return DropKind::Beside {
            axis: Axis::Vertical,
            before: true,
        };
    }
    DropKind::Beside {
        axis: Axis::Vertical,
        before: false,
    }
}

fn frame_rect(placement: &Placement) -> Rect {
    if placement.run.is_some() {
        return placement.rect;
    }
    Rect {
        x: placement.rect.x,
        y: placement.band.y,
        width: placement.rect.width,
        height: placement.band.height.saturating_add(placement.rect.height),
    }
}

/// The shortest output whose tiling area leaves a client `rows` tall: the
/// bar, the two GAP insets, the title band, and the rows themselves. Written
/// out rather than as a number so a caller cannot silently lose its client
/// area when one of those changes — which is what carving the band out of
/// every tile did to the dozen places across this crate that had a literal
/// here, the `selftest` subcommand the recipe runs among them.
pub(crate) fn least_output_height(rows: usize) -> usize {
    BAR_HEIGHT
        .saturating_add(GAP.saturating_mul(2))
        .saturating_add(TITLE_HEIGHT)
        .saturating_add(rows)
}

pub struct Scene {
    surfaces: BTreeMap<SurfaceKey, Surface>,
    input_regions: BTreeMap<SurfaceKey, SharedInputRegion>,
    titles: BTreeMap<SurfaceKey, String>,
    layout: Layout,
    pointer_x: i32,
    pointer_y: i32,
    surface_bytes: usize,
    /// What a drag is drawing INSTEAD of `layout`. Not a second source of
    /// truth: it is derived from `layout` on every pointer frame and dropped
    /// by every mutation of it, so nothing here can outlive what it was
    /// computed from.
    hint: Option<DropHint>,
    launcher: Launcher,
    help: Help,
    status: String,
}

impl Scene {
    pub fn new() -> Scene {
        Scene {
            surfaces: BTreeMap::new(),
            input_regions: BTreeMap::new(),
            titles: BTreeMap::new(),
            layout: Layout::new(),
            pointer_x: 0,
            pointer_y: 0,
            surface_bytes: 0,
            hint: None,
            launcher: Launcher::new(),
            help: Help::default(),
            status: String::new(),
        }
    }

    pub fn commit(&mut self, key: SurfaceKey, surface: Surface) -> Result<bool, String> {
        let is_new = !self.surfaces.contains_key(&key);
        let prior = self
            .surfaces
            .get(&key)
            .map(|current| current.pixels.len())
            .unwrap_or(0);
        let retained = self
            .surface_bytes
            .checked_sub(prior)
            .ok_or_else(|| "scene byte accounting underflow".to_string())?;
        let next = retained
            .checked_add(surface.pixels.len())
            .ok_or_else(|| "scene byte accounting overflow".to_string())?;
        if next > MAX_SCENE_BYTES {
            return Err(format!(
                "scene surfaces need {next} bytes, exceeding {MAX_SCENE_BYTES}"
            ));
        }
        self.surfaces.insert(key, surface);
        self.surface_bytes = next;
        if is_new {
            self.layout.map(key);
            self.hint = None;
        }
        Ok(is_new)
    }

    pub fn set_input_region(&mut self, key: SurfaceKey, region: Option<SharedInputRegion>) -> bool {
        if !self.surfaces.contains_key(&key) {
            return false;
        }
        if let Some(region) = region {
            self.input_regions.insert(key, region);
        } else {
            self.input_regions.remove(&key);
        }
        true
    }

    /// The toplevel's title, as `xdg_toplevel.set_title` gave it. Accepted for
    /// a surface that has NOT committed yet, unlike an input region: a client
    /// sets its title before its first buffer and need never send it again, so
    /// refusing it there would lose most titles there are. Answers whether the
    /// stored text changed, since a repaint is only owed when it did.
    ///
    /// Its lifetime is the xdg_toplevel OBJECT's, not the mapped pixels': it
    /// survives every unmap and is dropped by `remove`, `forget_title` and
    /// client teardown. An input region can ride `discard_pixels` because the
    /// client re-supplies one on every commit; nothing re-supplies a title.
    pub fn set_title(&mut self, key: SurfaceKey, title: String) -> bool {
        // The string is a client's, so it is bounded here rather than trusted.
        // It outlives the request in a map that lives as long as the client,
        // and nothing downstream would ever shorten it.
        let title = match title.char_indices().nth(MAX_TITLE_CHARS) {
            Some((offset, _)) => title.get(..offset).unwrap_or_default().to_string(),
            None => title,
        };
        // An EMPTY title is no title rather than a title that is blank, so
        // whatever draws one has a single absent case to answer rather than
        // two that look the same on screen.
        if title.is_empty() {
            return self.titles.remove(&key).is_some();
        }
        if self.titles.get(&key) == Some(&title) {
            return false;
        }
        self.titles.insert(key, title);
        true
    }

    /// Drop a title because its ROLE OBJECT went, which is the one teardown
    /// `remove` does not cover: destroying an xdg_toplevel leaves its
    /// wl_surface alive, and a toplevel created on that surface next would
    /// otherwise inherit the dead one's name until it set its own.
    pub fn forget_title(&mut self, key: SurfaceKey) -> bool {
        self.titles.remove(&key).is_some()
    }

    /// Test-only: the renderer reads the map directly, inside this type, so a
    /// non-test accessor would still have no caller.
    #[cfg(test)]
    pub fn title(&self, key: SurfaceKey) -> Option<&str> {
        self.titles.get(&key).map(String::as_str)
    }

    /// Whether the surface has pixels, which is what decides whether a band is
    /// drawn for it at all. The caller is the repaint decision on a rename: a
    /// title on a surface that has never committed is on no screen yet.
    pub fn is_mapped(&self, key: SurfaceKey) -> bool {
        self.surfaces.contains_key(&key)
    }

    fn discard_pixels(&mut self, key: SurfaceKey) -> bool {
        self.input_regions.remove(&key);
        if let Some(surface) = self.surfaces.remove(&key) {
            self.surface_bytes = self.surface_bytes.saturating_sub(surface.pixels.len());
            return true;
        }
        false
    }

    pub fn unmap(&mut self, key: SurfaceKey) -> bool {
        let layout_changed = self.layout.contains(key);
        self.discard_pixels(key);
        self.layout.unmap(key);
        // A block is drawn over the arrangement rather than replacing it, so
        // one dropped here owes a repaint and nothing else. Every caller
        // repaints unconditionally, which is why this reports the LAYOUT alone
        // — saying otherwise would ask for a round of configures for pixels no
        // client was ever told about.
        self.clear_hint();
        layout_changed
    }

    pub fn remove(&mut self, key: SurfaceKey) -> bool {
        let layout_changed = self.layout.contains(key);
        self.discard_pixels(key);
        self.titles.remove(&key);
        self.layout.forget(key);
        self.clear_hint();
        layout_changed
    }

    pub fn remove_client(&mut self, client: u64) -> bool {
        let layout_changed = self.surfaces.keys().any(|key| key.client == client);
        let removed = self
            .surfaces
            .iter()
            .filter(|(key, _)| key.client == client)
            .fold(0usize, |total, (_, surface)| {
                total.saturating_add(surface.pixels.len())
            });
        self.surfaces.retain(|key, _| key.client != client);
        self.input_regions.retain(|key, _| key.client != client);
        self.titles.retain(|key, _| key.client != client);
        self.surface_bytes = self.surface_bytes.saturating_sub(removed);
        self.layout.unmap_client(client);
        self.clear_hint();
        layout_changed
    }

    pub fn command(&mut self, command: Command) {
        self.layout.apply(command);
        self.hint = None;
    }

    pub fn focus_key(&mut self, key: SurfaceKey) -> bool {
        self.layout.focus_key(key)
    }

    pub fn launcher(&mut self, action: LauncherAction) -> Option<LaunchRequest> {
        self.launcher.apply(action)
    }

    pub fn launcher_visible(&self) -> bool {
        self.launcher.visible()
    }

    /// Set the sheet's one bit outright rather than by an action, so a
    /// restore cannot be about a DIRECTION. Refuses to raise it behind the
    /// launcher, which is where "the two are never both up" now lives —
    /// before, that held only because nothing happened to call it.
    pub fn set_help(&mut self, visible: bool) -> bool {
        self.help.set(visible && !self.launcher.visible());
        self.help.visible()
    }

    pub fn help_visible(&self) -> bool {
        self.help.visible()
    }

    /// Either overlay is modal: it owns the keyboard, withdraws pointer
    /// hover, and must not be clicked through to the tiles it covers.
    pub fn modal(&self) -> bool {
        self.launcher.visible() || self.help.visible()
    }

    pub fn launcher_checkpoint(&self) -> Launcher {
        self.launcher.clone()
    }

    pub fn restore_launcher(&mut self, launcher: Launcher) {
        self.launcher = launcher;
    }

    pub fn views(&self, width: usize, height: usize) -> Vec<ViewLayout> {
        let mut views = self
            .layout
            .views(width, self.tiled_height(height), GAP, TITLE_HEIGHT);
        for view in &mut views {
            view.rect.y = view.rect.y.saturating_add(BAR_HEIGHT);
        }
        views
    }

    /// The output minus the status bar. EVERY consumer of tiling geometry
    /// goes through this and `tiled_placements` — the renderer, the layout
    /// published to clients, and the pointer hit test — because two of them
    /// disagreeing is a click that lands on a different tile than the one
    /// under the cursor, with nothing on screen to say so.
    fn tiled_height(&self, height: usize) -> usize {
        height.saturating_sub(BAR_HEIGHT)
    }

    /// Every tile on screen, the bar's height already added. There is one
    /// arrangement to ask about since a drag draws a block instead of
    /// re-flowing, so this takes no tree: it used to, for a second geometry
    /// that no longer exists.
    pub(crate) fn tiled_placements(&self, width: usize, height: usize) -> Vec<Placement> {
        let mut placements =
            self.layout
                .placements(width, self.tiled_height(height), GAP, TITLE_HEIGHT);
        for placement in &mut placements {
            placement.rect.y = placement.rect.y.saturating_add(BAR_HEIGHT);
            placement.band.y = placement.band.y.saturating_add(BAR_HEIGHT);
        }
        placements
    }

    /// The line the bar paints. Owned here so the sampler thread hands over
    /// text and nothing in the render path reads a clock or a file. Answers
    /// the text it REPLACED when the line changed, so a caller whose paint
    /// fails can put it back: otherwise the scene holds a line the screen
    /// never showed, and the next identical sample repaints nothing.
    pub fn set_status(&mut self, status: String) -> Option<String> {
        if self.status == status {
            return None;
        }
        Some(std::mem::replace(&mut self.status, status))
    }

    pub fn focused(&self) -> Option<SurfaceKey> {
        self.layout.focused()
    }

    #[cfg(test)]
    pub fn surface_size(&self, key: SurfaceKey) -> Option<(usize, usize)> {
        self.surfaces
            .get(&key)
            .map(|surface| (surface.width, surface.height))
    }

    /// Answers whether the pointer actually MOVED, which a nonzero delta does
    /// not settle: a delta pointing off the edge of the output is clamped
    /// away, leaving a report that asked to move and did not.
    pub fn move_pointer(&mut self, dx: i32, dy: i32, width: usize, height: usize) -> bool {
        let max_x = i32::try_from(width.saturating_sub(1)).unwrap_or(i32::MAX);
        let max_y = i32::try_from(height.saturating_sub(1)).unwrap_or(i32::MAX);
        let was = (self.pointer_x, self.pointer_y);
        self.pointer_x = self.pointer_x.saturating_add(dx).clamp(0, max_x);
        self.pointer_y = self.pointer_y.saturating_add(dy).clamp(0, max_y);
        was != (self.pointer_x, self.pointer_y)
    }

    /// Put the pointer where an ABSOLUTE device says it is. BOTH coordinates,
    /// always: a device that named only one axis this frame is still somewhere
    /// on the other, and the reader carries that position rather than leaving
    /// the cursor's — which after a relative device moved it is nowhere the
    /// absolute one is pointing.
    ///
    /// Answers whether the pointer moved, as `move_pointer` does and for a
    /// sharper reason: an absolute device re-sends the position it already has
    /// far more readily than a relative one sends a zero delta, and every one
    /// of those would otherwise cost a paint and re-answer focus.
    pub fn place_pointer(&mut self, x: Fraction, y: Fraction, width: usize, height: usize) -> bool {
        let was = (self.pointer_x, self.pointer_y);
        self.pointer_x = across(x, width);
        self.pointer_y = across(y, height);
        was != (self.pointer_x, self.pointer_y)
    }

    #[cfg(test)]
    pub fn pointer_target(&self, width: usize, height: usize) -> Option<SurfacePoint> {
        let placements = self.tiled_placements(width, height);
        self.pointer_target_from(&placements)
    }

    pub(crate) fn pointer_at(&self) -> (i32, i32) {
        (self.pointer_x, self.pointer_y)
    }

    #[cfg(test)]
    pub(crate) fn layout(&self) -> &Layout {
        &self.layout
    }

    /// Open a window BELOW another, which is the drop an operator makes on a
    /// bottom edge. Test-only, and it exists because a new window now JOINS
    /// the container it opens in: there is no mode to set beforehand, so a
    /// test that wants a column says so at the moment it makes one.
    #[cfg(test)]
    pub(crate) fn drop_below(&mut self, moved: SurfaceKey, over: SurfaceKey) -> bool {
        self.layout.drop_onto(
            moved,
            over,
            DropKind::Beside {
                axis: Axis::Vertical,
                before: false,
            },
        )
    }

    /// Work out where a release would land the dragged window and hold the
    /// block that says so. The arrangement does NOT move while a drag is in
    /// flight: an operator aiming at a tile that slid out from under the
    /// pointer as they approached it could not hit anything, which is what a
    /// live re-flow did. The screen stays put and the answer is drawn ON it.
    ///
    /// Read off the SCREEN geometry, which is now the only geometry there is.
    /// The aim used to be computed against the layout with the dragged window
    /// taken out, and that existed solely so the picture could not push its own
    /// target away; with nothing re-flowing, aiming at what the operator can
    /// see is both simpler and what they mean.
    ///
    /// A press alone must not move the window; that is the CALLER's threshold
    /// from the press point, not a region here. Answers whether the BLOCK
    /// moved, so a pointer crossing no boundary costs no repaint.
    pub fn aim_drop(&mut self, dragged: SurfaceKey, width: usize, height: usize) -> bool {
        let hint = self.drop_hint(dragged, width, height);
        let changed = self.hint != hint;
        self.hint = hint;
        changed
    }

    fn drop_hint(&self, dragged: SurfaceKey, width: usize, height: usize) -> Option<DropHint> {
        let placements = self.tiled_placements(width, height);
        let (placement, kind) = self.drop_target_in(&placements)?;
        // Its own tile lands nothing — a window cannot be moved beside itself
        // — so there is no block to promise one. This is also why no dead zone
        // is needed any more: with the picture static, a window's own tile is
        // its own tile rather than a neighbour's grown into its place.
        let target = placement.key;
        if target == dragged {
            return None;
        }
        let area = hint_area(placement, kind, self.layout.run_direction(target));
        Some(DropHint { target, kind, area })
    }

    #[cfg(test)]
    pub fn hint_is_live(&self) -> bool {
        self.hint.is_some()
    }

    #[cfg(test)]
    pub(crate) fn hint_area(&self) -> Option<Rect> {
        self.hint.map(|hint| hint.area)
    }

    /// Apply the drop the block was promising. Computed ONCE, when the block
    /// was drawn, and kept: a release that re-derived it could answer
    /// differently from what the operator was looking at.
    ///
    /// `None` when there was no block; otherwise whether the ARRANGEMENT
    /// moved. Those are two answers rather than one because they buy different
    /// things: a block that came down owes a repaint whatever the drop did,
    /// and only a drop that moved something owes the clients their configures.
    /// Folding them into one bool stranded the block for a drop that landed
    /// where the window already was — the commonest gesture there is — since
    /// the caller then had nothing left to clear and no reason to paint.
    pub fn commit_drop(&mut self, dragged: SurfaceKey) -> Option<bool> {
        let hint = self.hint.take()?;
        Some(self.layout.drop_onto(dragged, hint.target, hint.kind))
    }

    /// Take the block down. Answers whether the screen moves, which it does
    /// whenever one was up — but no client is owed a configure, since the
    /// arrangement underneath never changed.
    pub fn clear_hint(&mut self) -> bool {
        self.hint.take().is_some()
    }

    /// The window under the pointer, band or client area. `None` over the
    /// status bar, a gap or a border — nowhere that belongs to a window, so
    /// nothing to answer. A stacked-away leaf answers only for its own BAND:
    /// every leaf of a stack shares one content rectangle, so that rectangle
    /// is the SHOWN leaf's and `tile_at`'s `visible` guard is what keeps a
    /// hidden one from claiming it.
    ///
    /// The TILE is the window, not the client's pixels within it: an
    /// undersized buffer or a narrow input region delivers nothing over the
    /// rest of the tile, and this answers there anyway. Same decision the
    /// band already is, and deliberate — a client cannot decline the keyboard
    /// by shrinking, which would leave part of a tile nothing could focus.
    /// The cost is that a press and a hover over such a spot no longer name
    /// the same target.
    pub fn window_at_pointer(&self, width: usize, height: usize) -> Option<SurfaceKey> {
        let placements = self.tiled_placements(width, height);
        let (x, y) = self.pointer_at_usize()?;
        Some(placements.get(tile_at(&placements, x, y)?)?.key)
    }

    /// The window an ALT press picks up: the one under the pointer, and only
    /// where a drag of it could reach somewhere. A press that could move
    /// nothing must not be taken from the client — a fullscreen one would
    /// lose every Alt click it has.
    pub fn draggable_at_pointer(&self, width: usize, height: usize) -> Option<SurfaceKey> {
        let key = self.window_at_pointer(width, height)?;
        self.layout.can_drag(key).then_some(key)
    }

    fn pointer_at_usize(&self) -> Option<(usize, usize)> {
        Some((
            usize::try_from(self.pointer_x).ok()?,
            usize::try_from(self.pointer_y).ok()?,
        ))
    }

    /// What a bare press on a TITLE BAND lands on: one of the presentation
    /// buttons at its right end, or the band itself, which is the handle a
    /// drag takes. A band reaches no client — the hit test above knows only
    /// client areas — so this is a question only the compositor answers, and
    /// the seam that makes a band draggable without a client seeing it.
    ///
    /// One lookup rather than two, and the ORDER is the answer's shape rather
    /// than a rule the caller has to remember: both gestures open with a press
    /// on the same strip, so a caller that asked about the handle first would
    /// make every button a drag handle that never fires.
    pub fn band_press_at_pointer(&self, width: usize, height: usize) -> Option<BandPress> {
        let placements = self.tiled_placements(width, height);
        let (x, y) = self.pointer_at_usize()?;
        let index = placements
            .iter()
            .position(|placement| contains(placement.band, x, y))?;
        let placement = placements.get(index)?;
        if let Some((rects, _)) = self.band_buttons_of(placement) {
            if let Some(at) = rects.iter().position(|slot| contains(*slot, x, y)) {
                return Some(BandPress::Button(placement.key, *BUTTONS.get(at)?));
            }
        }
        Some(BandPress::Handle(placement.key))
    }

    /// The buttons a band actually carries, with the presentation its container
    /// is already in. `None` where it carries none, which is a band with no
    /// room for them.
    ///
    /// The painter and the hit test both come through here rather than each
    /// asking `band_buttons`: a button drawn where nothing answers is a button
    /// that does nothing when pressed, with nothing on screen to say so.
    ///
    /// The presentation is READ OFF the placement rather than walked out of the
    /// tree, and off the placement's own `presented` rather than derived from
    /// the `run` beside it: the two are the same fact for a leaf in a container
    /// and are not for a leaf its WORKSPACE presents, which is in no run at
    /// all. Asking the layout per band
    /// instead would search the tree once for every window on every repaint,
    /// which is quadratic in a flat row of them, and would be a SECOND reading
    /// of the rule `presented_mut` owns: a band could then mark one container
    /// while its buttons changed another.
    ///
    /// Every window has a presenting container, so there is no case here that
    /// answers nothing: a leaf with no container of its own is presented by its
    /// WORKSPACE, which is what puts these buttons on the very first window
    /// rather than leaving the one band an operator sees first saying least.
    fn band_buttons_of(
        &self,
        placement: &Placement,
    ) -> Option<([Rect; BUTTONS.len()], Presentation)> {
        Some((band_buttons(placement.band)?, placement.presented))
    }

    /// Where a drop lands: the window under the pointer, and what landing
    /// there means. Read off the screen, which a drag no longer disturbs.
    ///
    /// A tile has FIVE zones. Over the middle the two windows trade places;
    /// over an edge the dragged one lands on that side, in a column for the
    /// top and bottom edges and in a row for the left and right — whatever
    /// the target's own container runs as, since the drop names its axis and
    /// `insert_beside` makes the container it needs. Inside a STACK it does
    /// not: that container presents a list, so `insert_beside` puts the leaf
    /// in the list rather than building a container the stack would not draw.
    ///
    /// A title BAND is the exception and keeps two zones along the run it is
    /// part of. Five will not fit in a strip a line of text tall, and the run
    /// is a list rather than an area: the only thing a drop onto one can
    /// sensibly mean is a position in it.
    #[cfg(test)]
    pub fn drop_target(&self, width: usize, height: usize) -> Option<(SurfaceKey, DropKind)> {
        let placements = self.tiled_placements(width, height);
        self.drop_target_in(&placements)
            .map(|(placement, kind)| (placement.key, kind))
    }

    /// The same over placements already in hand, which is how the aim asks:
    /// it wants the target's own rectangle as well as the kind, and building
    /// the arrangement twice a frame to get them separately is a per-motion
    /// allocation for an answer one pass already has.
    fn drop_target_in<'a>(&self, placements: &'a [Placement]) -> Option<(&'a Placement, DropKind)> {
        let (x, y) = self.pointer_at_usize()?;
        let placement = placements.get(tile_at(placements, x, y)?)?;
        if contains(placement.band, x, y) {
            let band = placement.band;
            let before = match self.layout.run_direction(placement.key) {
                Some(Axis::Horizontal) => x < band.x.saturating_add(band.width / 2),
                Some(Axis::Vertical) | None => y < band.y.saturating_add(band.height / 2),
            };
            return Some((placement, DropKind::InRun { before }));
        }
        Some((placement, zone_of(placement.rect, x, y)))
    }

    fn pointer_target_from(&self, placements: &[Placement]) -> Option<SurfacePoint> {
        let x = usize::try_from(self.pointer_x).ok()?;
        let y = usize::try_from(self.pointer_y).ok()?;
        for placement in placements {
            if !placement.visible {
                continue;
            }
            let Some(surface) = self.surfaces.get(&placement.key) else {
                continue;
            };
            let rect = placement.rect;
            let surface_width = surface.width.min(rect.width);
            let surface_height = surface.height.min(rect.height);
            let Some(end_x) = rect.x.checked_add(surface_width) else {
                continue;
            };
            let Some(end_y) = rect.y.checked_add(surface_height) else {
                continue;
            };
            if x < rect.x || x >= end_x || y < rect.y || y >= end_y {
                continue;
            }
            let local_x = i32::try_from(x.saturating_sub(rect.x)).ok()?;
            let local_y = i32::try_from(y.saturating_sub(rect.y)).ok()?;
            if self
                .input_regions
                .get(&placement.key)
                .is_some_and(|region| !region.contains(local_x, local_y))
            {
                continue;
            }
            return Some(SurfacePoint {
                key: placement.key,
                x: local_x,
                y: local_y,
            });
        }
        None
    }

    #[cfg(test)]
    pub fn pointer_target_for(
        &self,
        key: SurfaceKey,
        width: usize,
        height: usize,
    ) -> Option<SurfacePoint> {
        let placements = self.tiled_placements(width, height);
        self.pointer_target_for_from(key, &placements)
    }

    fn pointer_target_for_from(
        &self,
        key: SurfaceKey,
        placements: &[Placement],
    ) -> Option<SurfacePoint> {
        placements
            .iter()
            .filter(|placement| placement.key == key && placement.visible)
            .map(|placement| placement.rect)
            .next()
            .and_then(|rect| {
                self.surfaces.get(&key)?;
                let origin_x = i32::try_from(rect.x).ok()?;
                let origin_y = i32::try_from(rect.y).ok()?;
                Some(SurfacePoint {
                    key,
                    x: self.pointer_x.saturating_sub(origin_x),
                    y: self.pointer_y.saturating_sub(origin_y),
                })
            })
    }

    pub fn pointer_targets(
        &self,
        grab: Option<SurfaceKey>,
        width: usize,
        height: usize,
    ) -> (Option<SurfacePoint>, Option<SurfacePoint>) {
        let placements = self.tiled_placements(width, height);
        (
            self.pointer_target_from(&placements),
            grab.and_then(|key| self.pointer_target_for_from(key, &placements)),
        )
    }

    /// The band across the top of one tile. Painted in the BORDER pass, before
    /// any client pixels, for the reason the borders are: a tile's decoration
    /// must not be able to overwrite a neighbour's buffer.
    ///
    /// A window with no title gets a bare band rather than a placeholder. The
    /// band is what says the window is there and is the handle a drag will
    /// take; inventing a name for it would put a word on screen no client
    /// chose.
    fn draw_title(
        &self,
        frame: &mut [u8],
        width: usize,
        height: usize,
        stride: usize,
        placement: &Placement,
    ) {
        let band = placement.band;
        if band.width == 0 || band.height == 0 {
            return;
        }
        let rect = (band.x, band.y, band.width, band.height);
        let fill = if placement.focused {
            TITLE_FOCUSED
        } else {
            TITLE_UNFOCUSED
        };
        ui::fill(frame, width, height, stride, rect, fill);
        // Before the name and independently of it: a window that set no title
        // still sits in a container, and its band is the only handle the
        // pointer has on one.
        let buttons = self.band_buttons_of(placement);
        if let Some((rects, current)) = buttons {
            for (slot, wanted) in rects.iter().zip(BUTTONS) {
                let ink = if wanted == current {
                    BUTTON_INK_ON
                } else {
                    BUTTON_INK
                };
                draw_button(frame, width, height, stride, *slot, wanted, ink);
            }
        }
        let Some(title) = self.titles.get(&placement.key) else {
            return;
        };
        // The name is clipped to what the buttons leave, not to the band: a
        // title running under them would read as part of an icon.
        let text_clip = match buttons.as_ref().and_then(|(rects, _)| rects.first()) {
            Some(first) => (band.x, band.y, first.x.saturating_sub(band.x), band.height),
            None => rect,
        };
        ui::draw_text_clipped(
            frame,
            width,
            height,
            stride,
            band.x.saturating_add(TITLE_TEXT_LEFT),
            band.y.saturating_add(TITLE_TEXT_TOP),
            TITLE_SCALE,
            title,
            TITLE_TEXT,
            text_clip,
        );
    }

    pub fn render(&self, frame: &mut [u8], width: usize, height: usize, stride: usize) {
        // `take(height)`: every other painter here clips to the output, and
        // the shadow buffer being exactly that tall is the framebuffer's
        // arithmetic rather than this function's contract.
        for row in frame.chunks_mut(stride).take(height) {
            let visible = width.saturating_mul(4).min(row.len());
            if let Some(pixels) = row.get_mut(..visible) {
                for [blue, green, red, unused] in pixels.as_chunks_mut::<4>().0 {
                    *blue = 0x30;
                    *green = 0x25;
                    *red = 0x20;
                    *unused = 0;
                }
            }
        }

        let placements = self.tiled_placements(width, height);
        // EVERY band before ANY border, rather than the two per placement.
        // They are separate rectangles that overlap in a stack — the shown
        // leaf's border rides four pixels up into the run's last band — so
        // interleaving them lets a band drawn later erase a border drawn
        // earlier, and only when the shown leaf is not the last of its run.
        // Same argument one pass down as decoration before client pixels.
        for placement in &placements {
            if !self.surfaces.contains_key(&placement.key) {
                continue;
            }
            // Whether or not the client is shown: a leaf stacked away behind a
            // sibling draws nothing else, and its band says it is there.
            self.draw_title(frame, width, height, stride, placement);
        }
        for placement in &placements {
            if !self.surfaces.contains_key(&placement.key) || !placement.visible {
                continue;
            }
            // The FRAME, not the client area: a tile too short to hold both is
            // all band and no client, and guarding on the client alone would
            // drop the one thing left to draw for it.
            let outline = frame_rect(placement);
            if outline.width == 0 || outline.height == 0 {
                continue;
            }
            let x = i64::try_from(outline.x).unwrap_or(i64::MAX);
            let y = i64::try_from(outline.y).unwrap_or(i64::MAX);
            draw_border(
                frame,
                width,
                height,
                stride,
                x,
                y,
                outline.width,
                outline.height,
                placement.focused,
            );
        }
        for placement in placements {
            // The CLIENT area here, where the border pass above wants the
            // frame: a tile too short to hold a band has no client pixels to
            // draw, and `draw_surface` was already a no-op for one.
            if !placement.visible || placement.rect.width == 0 || placement.rect.height == 0 {
                continue;
            }
            let Some(surface) = self.surfaces.get(&placement.key) else {
                continue;
            };
            draw_surface(frame, width, height, stride, placement.rect, surface);
        }
        // Over the windows and under everything that is not one: the block
        // says where a release would land, and a bar or an overlay it hid
        // would be a worse lie than the one it answers.
        if let Some(hint) = self.hint {
            draw_hint(frame, width, height, stride, hint.area);
        }
        let active = self.layout.active_workspace();
        let desks = bar::desks(self.layout.occupied_workspaces(), active);
        bar::paint(frame, width, height, stride, &desks, active, &self.status);
        self.launcher.paint(frame, width, height, stride);
        self.help.paint(frame, width, height, stride);
        draw_pointer(frame, width, height, stride, self.pointer_x, self.pointer_y);
    }
}

/// Where along an axis an absolute device is pointing, as a fraction of its
/// own span. This is the form a report crosses in, because neither end can do
/// the arithmetic alone: the device's range is meaningless outside the reader
/// that asked for it, and the output's size is unknown there.
///
/// It is the EXACT ratio — the device's offset over the device's span, neither
/// reduced nor rescaled. A fixed-point intermediate would floor it once before
/// `across` floors it again, and two floorings put a report as much as a pixel
/// from where the operator was pointing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Fraction {
    pub numerator: u32,
    pub denominator: u32,
}

/// A fraction of an extent, in pixels. ROUNDED and then clamped to the last
/// pixel, which is what makes both edges reachable — and it is the rounding
/// that does the work rather than the clamp.
///
/// The device is not the only thing quantising. Under QEMU the host pointer is
/// scaled onto `0..=0x7fff` by an integer division that FLOORS, so the
/// rightmost column of an 800-wide surface arrives as 32726 rather than 32767,
/// and every other column arrives a shade low too. Flooring again here would
/// turn "a shade low" into a whole pixel low almost everywhere and put the far
/// edge permanently out of reach; rounding recovers the column the operator is
/// actually on, the last one included.
///
/// A zero denominator answers 0 rather than dividing: it means the device
/// declared no span, and the near edge is the only defensible reading of a
/// position that cannot have one.
///
/// Rounding recovers EVERY column rather than most of them, and that is a
/// property of the output width as much as of the arithmetic: it holds for all
/// of `0..w` exactly while `w <= 16384`. The bound below is where the two are
/// tied, because widening the output is what would quietly break it — the
/// resolutions any test samples would still pass.
const _: () = assert!(
    MAX_UI_DIMENSION <= 16384,
    "a wider output loses columns to QEMU's flooring; see `across`"
);

fn across(fraction: Fraction, extent: usize) -> i32 {
    if fraction.denominator == 0 {
        return 0;
    }
    let extent = i64::try_from(extent).unwrap_or(i64::MAX);
    let last = i32::try_from(extent.saturating_sub(1)).unwrap_or(i32::MAX);
    let denominator = i64::from(fraction.denominator);
    let scaled = i64::from(fraction.numerator)
        .saturating_mul(extent)
        .saturating_add(denominator / 2)
        / denominator;
    i32::try_from(scaled)
        .unwrap_or(i32::MAX)
        .clamp(0, last.max(0))
}

fn contains(rect: Rect, x: usize, y: usize) -> bool {
    x >= rect.x
        && y >= rect.y
        && x < rect.x.saturating_add(rect.width)
        && y < rect.y.saturating_add(rect.height)
}

/// The drop block: sway's translucent blue, mixed into whatever is already
/// there rather than replacing it, so the window underneath stays readable and
/// the block reads as an overlay instead of as a hole in the screen.
///
/// Blended in integer thirds — two parts what is there, one part blue. There
/// is no alpha channel in this framebuffer to carry it, and mixing at draw
/// time is the whole of what "semi-transparent" can mean for a surface that
/// gets composited once.
fn draw_hint(frame: &mut [u8], width: usize, height: usize, stride: usize, area: Rect) {
    // B, G, R — the framebuffer is XRGB8888 and little-endian, so this is
    // blue rather than the orange the bytes read as.
    const HINT: [u8; 3] = [0xf0, 0x90, 0x30];
    let last_row = area.y.saturating_add(area.height).min(height);
    let last_column = area.x.saturating_add(area.width).min(width);
    for row in area.y..last_row {
        let Some(line) = frame.get_mut(row.saturating_mul(stride)..) else {
            continue;
        };
        let Some(pixels) = line.get_mut(..width.saturating_mul(4).min(line.len())) else {
            continue;
        };
        for column in area.x..last_column {
            let at = column.saturating_mul(4);
            let Some(pixel) = pixels.get_mut(at..at.saturating_add(3)) else {
                continue;
            };
            for (channel, tint) in pixel.iter_mut().zip(HINT.iter()) {
                *channel = (u16::from(*channel)
                    .saturating_mul(2)
                    .saturating_add(u16::from(*tint))
                    / 3) as u8;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_border(
    frame: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    x: i64,
    y: i64,
    surface_width: usize,
    surface_height: usize,
    focused: bool,
) {
    let color = if focused {
        [0xc0, 0x70, 0xf0, 0]
    } else {
        [0x70, 0x70, 0x70, 0]
    };
    let border = i64::try_from(BORDER).unwrap_or(0);
    let start_x = x.saturating_sub(border);
    let start_y = y.saturating_sub(border);
    let outer_width = surface_width.saturating_add(BORDER.saturating_mul(2));
    fill_rect(
        frame,
        width,
        height,
        stride,
        start_x,
        start_y,
        outer_width,
        BORDER,
        color,
    );
    fill_rect(
        frame,
        width,
        height,
        stride,
        start_x,
        y.saturating_add(i64::try_from(surface_height).unwrap_or(i64::MAX)),
        outer_width,
        BORDER,
        color,
    );
    fill_rect(
        frame,
        width,
        height,
        stride,
        start_x,
        y,
        BORDER,
        surface_height,
        color,
    );
    fill_rect(
        frame,
        width,
        height,
        stride,
        x.saturating_add(i64::try_from(surface_width).unwrap_or(i64::MAX)),
        y,
        BORDER,
        surface_height,
        color,
    );
}

#[allow(clippy::too_many_arguments)]
fn fill_rect(
    frame: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    x: i64,
    y: i64,
    rect_width: usize,
    rect_height: usize,
    color: [u8; 4],
) {
    let Some((source_x, columns)) = visible_span(x, rect_width, width) else {
        return;
    };
    let Some((source_y, rows)) = visible_span(y, rect_height, height) else {
        return;
    };
    let start_x = x.saturating_add(i64::try_from(source_x).unwrap_or(i64::MAX));
    let start_y = y.saturating_add(i64::try_from(source_y).unwrap_or(i64::MAX));
    for row in 0..rows {
        let target_y = start_y.saturating_add(i64::try_from(row).unwrap_or(i64::MAX));
        for column in 0..columns {
            let target_x = start_x.saturating_add(i64::try_from(column).unwrap_or(i64::MAX));
            put_pixel(frame, width, height, stride, target_x, target_y, color);
        }
    }
}

fn visible_span(origin: i64, length: usize, limit: usize) -> Option<(usize, usize)> {
    let length = i64::try_from(length).unwrap_or(i64::MAX);
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let start = origin.max(0).min(limit);
    let end = origin.saturating_add(length).max(0).min(limit);
    if end <= start {
        return None;
    }
    let source = usize::try_from(start.saturating_sub(origin)).ok()?;
    let visible = usize::try_from(end.saturating_sub(start)).ok()?;
    Some((source, visible))
}

fn draw_surface(
    frame: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    rect: Rect,
    surface: &Surface,
) {
    let x = i64::try_from(rect.x).unwrap_or(i64::MAX);
    let y = i64::try_from(rect.y).unwrap_or(i64::MAX);
    let draw_width = surface.width.min(rect.width);
    let draw_height = surface.height.min(rect.height);
    let Some((source_x_start, visible_columns)) = visible_span(x, draw_width, width) else {
        return;
    };
    let Some((source_y_start, visible_rows)) = visible_span(y, draw_height, height) else {
        return;
    };
    for (source_y, row) in surface
        .pixels
        .chunks_exact(surface.width.saturating_mul(4))
        .take(surface.height)
        .enumerate()
        .skip(source_y_start)
        .take(visible_rows)
    {
        let target_y = y.saturating_add(i64::try_from(source_y).unwrap_or(i64::MAX));
        for (source_x, pixel) in row
            .as_chunks::<4>()
            .0
            .iter()
            .enumerate()
            .skip(source_x_start)
            .take(visible_columns)
        {
            let target_x = x.saturating_add(i64::try_from(source_x).unwrap_or(i64::MAX));
            let [blue, green, red, alpha] = pixel;
            if surface.format == SHM_ARGB8888 && *alpha < u8::MAX {
                blend_pixel(
                    frame,
                    width,
                    height,
                    stride,
                    target_x,
                    target_y,
                    [*blue, *green, *red, *alpha],
                );
            } else {
                put_pixel(
                    frame,
                    width,
                    height,
                    stride,
                    target_x,
                    target_y,
                    [*blue, *green, *red, 0],
                );
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn blend_pixel(
    frame: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    x: i64,
    y: i64,
    source: [u8; 4],
) {
    let Ok(x) = usize::try_from(x) else {
        return;
    };
    let Ok(y) = usize::try_from(y) else {
        return;
    };
    if x >= width || y >= height {
        return;
    }
    let Some(offset) = y
        .checked_mul(stride)
        .and_then(|row| x.checked_mul(4).and_then(|column| row.checked_add(column)))
    else {
        return;
    };
    let Some(end) = offset.checked_add(4) else {
        return;
    };
    let Some(pixel) = frame.get_mut(offset..end) else {
        return;
    };
    let [blue, green, red, _] = pixel else {
        return;
    };
    let [source_blue, source_green, source_red, alpha] = source;
    let inverse = u16::from(u8::MAX.saturating_sub(alpha));
    *blue = u16::from(source_blue)
        .saturating_add(u16::from(*blue) * inverse / 255)
        .min(255) as u8;
    *green = u16::from(source_green)
        .saturating_add(u16::from(*green) * inverse / 255)
        .min(255) as u8;
    *red = u16::from(source_red)
        .saturating_add(u16::from(*red) * inverse / 255)
        .min(255) as u8;
}

fn put_pixel(
    frame: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    x: i64,
    y: i64,
    color: [u8; 4],
) {
    let Ok(x) = usize::try_from(x) else {
        return;
    };
    let Ok(y) = usize::try_from(y) else {
        return;
    };
    if x >= width || y >= height {
        return;
    }
    let Some(offset) = y
        .checked_mul(stride)
        .and_then(|row| x.checked_mul(4).and_then(|column| row.checked_add(column)))
    else {
        return;
    };
    let Some(end) = offset.checked_add(4) else {
        return;
    };
    if let Some(pixel) = frame.get_mut(offset..end) {
        pixel.copy_from_slice(&color);
    }
}

fn draw_pointer(frame: &mut [u8], width: usize, height: usize, stride: usize, x: i32, y: i32) {
    for delta in -6i64..=6 {
        put_pixel(
            frame,
            width,
            height,
            stride,
            i64::from(x).saturating_add(delta),
            i64::from(y),
            [0xff, 0xff, 0xff, 0],
        );
        put_pixel(
            frame,
            width,
            height,
            stride,
            i64::from(x),
            i64::from(y).saturating_add(delta),
            [0xff, 0xff, 0xff, 0],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn surface(color: [u8; 4], width: usize, height: usize) -> Surface {
        let mut pixels = Vec::new();
        for _ in 0..width.saturating_mul(height) {
            pixels.extend_from_slice(&color);
        }
        Surface {
            width,
            height,
            pixels,
            format: SHM_XRGB8888,
        }
    }

    /// A pixel of the TILING area, whose top is the status bar's bottom.
    /// These tests are about tiling geometry, so they render into an output
    /// the bar's height taller and address it from there — otherwise every
    /// coordinate below would encode how tall the bar happens to be.
    fn tiled(frame: &[u8], stride: usize, x: usize, y: usize) -> [u8; 4] {
        pixel(frame, stride, x, y.saturating_add(BAR_HEIGHT))
    }

    fn count_color(frame: &[u8], stride: usize, rect: Rect, color: [u8; 4]) -> usize {
        let mut total = 0usize;
        for y in rect.y..rect.y.saturating_add(rect.height) {
            for x in rect.x..rect.x.saturating_add(rect.width) {
                if pixel(frame, stride, x, y) == color {
                    total = total.saturating_add(1);
                }
            }
        }
        total
    }

    fn pixel(frame: &[u8], stride: usize, x: usize, y: usize) -> [u8; 4] {
        let offset = y
            .checked_mul(stride)
            .and_then(|row| x.checked_mul(4).and_then(|column| row.checked_add(column)))
            .unwrap();
        frame
            .get(offset..offset.saturating_add(4))
            .unwrap()
            .try_into()
            .unwrap()
    }

    #[test]
    fn clipped_surface_never_grows_or_escapes_frame() {
        let mut scene = Scene::new();
        scene
            .commit(
                SurfaceKey {
                    client: 1,
                    object: 8,
                },
                surface([1, 2, 3, 0], 100, 100),
            )
            .unwrap();
        let height = least_output_height(2);
        // The "never grows" half of this test's name, made falsifiable: the
        // buffer carries rows the output does not, and `0xaa` is what nothing
        // may have touched. A `frame.len()` assertion could not fail — the
        // slice's length is not something `render` can change.
        let stride = 20 * 4;
        let mut frame = vec![0xaa; stride * (height + 4)];
        scene.render(&mut frame, 20, height, stride);
        assert!(
            frame
                .get(stride * height..)
                .is_some_and(|rows| rows.iter().all(|byte| *byte == 0xaa)),
            "the clipped surface escaped the output"
        );
        assert!(frame.as_chunks::<4>().0.contains(&[1, 2, 3, 0]));
    }

    #[test]
    fn a_title_outlives_the_request_and_dies_with_its_surface() {
        let mut scene = Scene::new();
        let key = SurfaceKey {
            client: 1,
            object: 1,
        };
        // Accepted BEFORE any commit. A client sets its title once, before its
        // first buffer, and never sends it again — refusing it here the way an
        // input region is refused would lose every title there is.
        assert!(scene.set_title(key, "FIREFOX".to_string()));
        assert_eq!(scene.title(key), Some("FIREFOX"));
        // Answers whether the text CHANGED, since a repaint is only owed then.
        assert!(!scene.set_title(key, "FIREFOX".to_string()));
        assert!(scene.set_title(key, "FIREFOX - A PAGE".to_string()));

        scene.commit(key, surface([1, 2, 3, 0], 8, 8)).unwrap();
        assert_eq!(scene.title(key), Some("FIREFOX - A PAGE"));

        // Survives an UNMAP, which an input region does not: the client
        // re-supplies a region on every commit and nothing re-supplies a
        // title. A null-buffer attach is both the transient unmap and the
        // opening of the initial handshake, so dropping it here would lose
        // the name of a window that is about to come straight back.
        scene.unmap(key);
        assert_eq!(scene.title(key), Some("FIREFOX - A PAGE"));
        scene.commit(key, surface([1, 2, 3, 0], 8, 8)).unwrap();
        assert_eq!(
            scene.title(key),
            Some("FIREFOX - A PAGE"),
            "remapped nameless"
        );

        // Gone with the surface, or a re-used key inherits a stale name.
        scene.remove(key);
        assert_eq!(scene.title(key), None);
    }

    #[test]
    fn an_empty_title_is_no_title_rather_than_a_blank_one() {
        let mut scene = Scene::new();
        let key = SurfaceKey {
            client: 1,
            object: 1,
        };
        // The wire accepts a one-byte string that is just its NUL, so this is
        // reachable. Whatever draws a title should have ONE absent case.
        assert!(!scene.set_title(key, String::new()));
        assert_eq!(scene.title(key), None);

        assert!(scene.set_title(key, "NAMED".to_string()));
        assert!(scene.set_title(key, String::new()), "clearing is a change");
        assert_eq!(scene.title(key), None);
    }

    #[test]
    fn a_destroyed_toplevel_takes_its_name_with_it() {
        // The one teardown `remove` does not cover: the role object goes and
        // its wl_surface lives on.
        let mut scene = Scene::new();
        let key = SurfaceKey {
            client: 1,
            object: 1,
        };
        scene.commit(key, surface([1, 2, 3, 0], 8, 8)).unwrap();
        scene.set_title(key, "OLD TOPLEVEL".to_string());
        assert!(scene.forget_title(key));
        assert_eq!(scene.title(key), None);
        assert!(!scene.forget_title(key), "already gone");
    }

    #[test]
    fn a_client_cannot_spend_the_compositors_memory_on_a_title() {
        let mut scene = Scene::new();
        let key = SurfaceKey {
            client: 1,
            object: 1,
        };
        // Truncated by CHARACTERS, not bytes: cutting a multi-byte sequence in
        // half would leave a string that is not UTF-8 at all.
        let long = "é".repeat(MAX_TITLE_CHARS * 4);
        assert!(scene.set_title(key, long));
        let stored = scene.title(key).unwrap();
        assert_eq!(stored.chars().count(), MAX_TITLE_CHARS);
        assert_eq!(stored.len(), MAX_TITLE_CHARS * 2, "é is two bytes");

        // A title exactly at the bound is kept whole.
        let exact = "A".repeat(MAX_TITLE_CHARS);
        assert!(scene.set_title(key, exact.clone()));
        assert_eq!(scene.title(key), Some(exact.as_str()));
    }

    #[test]
    fn client_removal_takes_its_titles_with_it() {
        let mut scene = Scene::new();
        let mine = SurfaceKey {
            client: 1,
            object: 1,
        };
        let theirs = SurfaceKey {
            client: 2,
            object: 1,
        };
        scene.set_title(mine, "MINE".to_string());
        scene.set_title(theirs, "THEIRS".to_string());
        scene.remove_client(1);
        assert_eq!(scene.title(mine), None);
        assert_eq!(scene.title(theirs), Some("THEIRS"));
    }

    #[test]
    fn client_removal_removes_only_its_surfaces() {
        let mut scene = Scene::new();
        scene
            .commit(
                SurfaceKey {
                    client: 1,
                    object: 4,
                },
                surface([1, 2, 3, 0], 1, 1),
            )
            .unwrap();
        scene
            .commit(
                SurfaceKey {
                    client: 2,
                    object: 4,
                },
                surface([4, 5, 6, 0], 1, 1),
            )
            .unwrap();
        assert!(scene.remove_client(1));
        assert!(!scene.remove_client(3));
        assert_eq!(scene.surfaces.len(), 1);
        assert!(!scene.layout.contains(SurfaceKey {
            client: 1,
            object: 4
        }));
        assert!(scene.layout.check_invariants().is_ok());
        assert!(scene.surfaces.contains_key(&SurfaceKey {
            client: 2,
            object: 4
        }));
    }

    #[test]
    fn every_consumer_of_tiling_geometry_agrees_and_none_of_it_is_under_the_bar() {
        let mut scene = Scene::new();
        for object in 1..=3 {
            scene
                .commit(
                    SurfaceKey { client: 1, object },
                    surface([1, 2, 3, 0], 8, 8),
                )
                .unwrap();
        }
        scene
            .commit(
                SurfaceKey {
                    client: 1,
                    object: 4,
                },
                surface([4, 5, 6, 0], 8, 8),
            )
            .unwrap();
        // The fourth size is the SMALL one, and it has to leave a client area
        // to be one: it was `BAR_HEIGHT + 60` until the band was carved out,
        // which took its tiles to zero client height and made every view here
        // skip the agreement assertion below. Four of four exercised, nothing
        // said so, and this is the file the helper was written for.
        let mut exercised = 0usize;
        for (width, height) in [
            (320, 200),
            (640, 400),
            (1920, 1080),
            (200, least_output_height(4)),
        ] {
            for view in scene.views(width, height) {
                // Nothing tiled may occupy a row the bar owns.
                assert!(
                    view.rect.y >= BAR_HEIGHT,
                    "{width}x{height}: view at y={}",
                    view.rect.y
                );
                assert!(
                    view.rect.y.saturating_add(view.rect.height) <= height,
                    "{width}x{height}: view runs past the output"
                );
                if view.rect.width == 0 || view.rect.height == 0 {
                    continue;
                }
                exercised = exercised.saturating_add(1);
                // The published layout and the hit test must name the same
                // surface at the same point, or a click lands on a tile other
                // than the one under the cursor with nothing to say so. Near
                // the ORIGIN, since hit testing is over the surface's own
                // pixels and these are smaller than the tiles they sit in.
                let x = view.rect.x.saturating_add(1);
                let y = view.rect.y.saturating_add(1);
                scene.pointer_x = i32::try_from(x).unwrap();
                scene.pointer_y = i32::try_from(y).unwrap();
                // Through `pointer_targets` — the PRODUCTION entry point.
                // Asking the `#[cfg(test)]` wrapper instead would let this
                // test pass while the compositor's own hit test drifted,
                // which is the exact defect the reservation is here to
                // prevent and the one that half-happened while it landed.
                assert_eq!(
                    scene
                        .pointer_targets(None, width, height)
                        .0
                        .map(|point| point.key),
                    Some(view.key),
                    "{width}x{height}: hit test disagrees with the layout at {x},{y}"
                );
            }
        }
        // Four surfaces at four output sizes, and the assertion above must
        // have RUN for every one of them. Without this the whole loop is
        // satisfied by a `continue`, which is how the smallest size stopped
        // testing anything and nothing failed.
        assert_eq!(exercised, 16);
    }

    #[test]
    fn a_title_band_tops_every_tile_partitions_it_and_swallows_its_own_clicks() {
        let mut scene = Scene::new();
        for object in 1..=3u32 {
            scene
                .commit(
                    SurfaceKey { client: 1, object },
                    surface([1, 2, 3, 0], 8, 8),
                )
                .unwrap();
        }
        scene
            .commit(
                SurfaceKey {
                    client: 1,
                    object: 4,
                },
                surface([4, 5, 6, 0], 8, 8),
            )
            .unwrap();
        below(&mut scene, 4);
        // The last is a tile SHORTER than a band: 60 tiling rows less the two
        // GAP insets is 12, so that arrangement is all band and no client and
        // the partition below has to hold there too.
        for (width, height) in [
            (320, least_output_height(8)),
            (640, 400),
            (1920, 1080),
            (200, BAR_HEIGHT + 60),
        ] {
            // The tile the split ACTUALLY produced, asked for independently:
            // band 0 leaves it uncarved. Deriving it from the band plus the
            // client instead — which is what `frame_rect` does — would make
            // the partition below agree with itself, and a client one row
            // short of its tile would pass.
            let tiles: Vec<Rect> = scene
                .layout
                .placements(width, scene.tiled_height(height), GAP, 0)
                .into_iter()
                .map(|placement| Rect {
                    y: placement.rect.y.saturating_add(BAR_HEIGHT),
                    ..placement.rect
                })
                .collect();
            let placements = scene.tiled_placements(width, height);
            assert_eq!(placements.len(), tiles.len());
            for (index, placement) in placements.iter().enumerate() {
                let band = placement.band;
                let client = placement.rect;
                let tile = *tiles.get(index).unwrap();
                assert_eq!((band.x, band.y), (tile.x, tile.y));
                assert_eq!(band.width, tile.width);
                assert_eq!((client.x, client.width), (band.x, band.width));
                // The band and the client PARTITION the tile: they meet at one
                // row and together cover it exactly, so a client rect that
                // grew back into the band — or fell one row short of it —
                // fails here rather than by painting over something.
                assert_eq!(band.height, TITLE_HEIGHT.min(tile.height));
                assert_eq!(client.y, band.y.saturating_add(band.height));
                assert_eq!(
                    client.y.saturating_add(client.height),
                    tile.y.saturating_add(tile.height),
                    "{width}x{height}: the tile is not the band plus the client"
                );
                // And `frame_rect` recovers exactly that tile, which is what
                // the border is drawn around.
                assert_eq!(frame_rect(placement), tile);
                if tile.width == 0 || band.height == 0 {
                    continue;
                }
                // And a click anywhere in the band reaches no client —
                // including the columns the surface's own pixels start in,
                // which is exactly where the hit test would answer if the
                // carve were missing.
                for y in band.y..band.y.saturating_add(band.height) {
                    for x in [band.x, band.x + 1, band.x + band.width - 1] {
                        scene.pointer_x = i32::try_from(x).unwrap();
                        scene.pointer_y = i32::try_from(y).unwrap();
                        assert_eq!(
                            scene.pointer_targets(None, width, height).0,
                            None,
                            "{width}x{height}: the band is clickable at {x},{y}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn the_five_zones_of_a_tile_answer_swap_and_the_four_sides() {
        // A tile at an offset, so a zone read from the point's ABSOLUTE
        // coordinates rather than its position within the rect is caught.
        let rect = Rect {
            x: 10,
            y: 10,
            width: 90,
            height: 90,
        };
        let left = DropKind::Beside {
            axis: Axis::Horizontal,
            before: true,
        };
        let right = DropKind::Beside {
            axis: Axis::Horizontal,
            before: false,
        };
        let above = DropKind::Beside {
            axis: Axis::Vertical,
            before: true,
        };
        let below = DropKind::Beside {
            axis: Axis::Vertical,
            before: false,
        };
        for (x, y, expected) in [
            (55, 55, DropKind::Swap),
            (40, 40, DropKind::Swap),
            (69, 69, DropKind::Swap),
            (55, 12, above),
            (55, 98, below),
            (12, 55, left),
            (98, 55, right),
            // The middle NINTH, so a point just outside it is a side even
            // though it is nowhere near an edge.
            (55, 39, above),
            (55, 70, below),
            (39, 55, left),
            (70, 55, right),
        ] {
            assert_eq!(zone_of(rect, x, y), expected, "at {x},{y}");
        }

        // Every point in the tile answers, and every zone is REACHABLE — a
        // sweep of the whole rect finds all five. Not that it finds nothing
        // else: `DropKind` has five inhabitants and a sixth is not
        // expressible, so the count is the reachability claim alone.
        let mut seen = Vec::new();
        for y in rect.y..rect.y + rect.height {
            for x in rect.x..rect.x + rect.width {
                let zone = zone_of(rect, x, y);
                if !seen.contains(&zone) {
                    seen.push(zone);
                }
            }
        }
        assert_eq!(seen.len(), 5, "{seen:?}");

        // The nearest edge is judged in PROPORTION to the tile, or a wide
        // short one would answer top or bottom almost everywhere: this point
        // is 5 pixels from the left of 300 and 1 from the top of 30, which is
        // nearer the left as a fraction and nearer the top in pixels.
        let wide = Rect {
            x: 0,
            y: 0,
            width: 300,
            height: 30,
        };
        assert_eq!(zone_of(wide, 5, 1), left);
        // And the other way: 30 of 300 across is a tenth, 1 of 30 down is a
        // thirtieth, so this one really is nearest the top.
        assert_eq!(zone_of(wide, 30, 1), above);

        // A TALL narrow tile, where the same rule bites the other way round —
        // the pair that pixels favour here is left and right, and proportion
        // has to overrule them. Two of 30 across is a fifteenth, five of 300
        // down is a sixtieth: nearer the top as a fraction, nearer the left
        // in pixels.
        let tall = Rect {
            x: 0,
            y: 0,
            width: 30,
            height: 300,
        };
        assert_eq!(zone_of(tall, 2, 5), above);
        assert_eq!(zone_of(tall, 1, 40), left);

        // A tile with no area has no edges to be nearer one of, and a swap is
        // the drop that needs no room.
        for degenerate in [
            Rect {
                x: 0,
                y: 0,
                width: 0,
                height: 10,
            },
            Rect {
                x: 0,
                y: 0,
                width: 10,
                height: 0,
            },
        ] {
            assert_eq!(zone_of(degenerate, 0, 0), DropKind::Swap);
        }
    }

    #[test]
    fn a_bands_half_is_read_along_the_run_it_would_join() {
        // A band drop names a POSITION in a list, so which half of the band
        // the pointer is on has to be read along the direction that list
        // actually runs — across for a row, down for a stack. Reading it the
        // other way round makes one side of the row unaskable.
        //
        // This used to be a test about two geometries disagreeing: the drop
        // was aimed at the layout with the dragged window taken OUT, where a
        // row of two collapses and the target is in no container at all. With
        // the picture no longer re-flowing under a drag there is one geometry
        // and the question is simply the run's.
        let mut scene = Scene::new();
        for object in 1..=2 {
            scene
                .commit(
                    SurfaceKey { client: 1, object },
                    surface([1, 1, 1, 0], 8, 8),
                )
                .unwrap();
        }
        let (width, height) = (240, 600);
        let key = |object| SurfaceKey { client: 1, object };
        assert_eq!(scene.layout.run_direction(key(2)), Some(Axis::Horizontal));
        let band = tile(&scene, width, height, 2).band;
        assert!(band.width > band.height, "the band does not run across");

        // Left and right of the band's middle, BELOW its own middle so a
        // reading taken down the band would answer the same for both.
        for (x, before) in [
            (band.x + 1, true),
            (band.x + band.width.saturating_sub(2), false),
        ] {
            scene.pointer_x = i32::try_from(x).unwrap();
            scene.pointer_y = i32::try_from(band.y + band.height - 1).unwrap();
            assert_eq!(
                scene.drop_target(width, height),
                Some((key(2), DropKind::InRun { before })),
                "band at x={x}"
            );
        }
    }

    #[test]
    fn a_tabs_half_is_read_across_its_own_share_and_its_block_stands_on_that_edge() {
        // The tabbed half of the rule above, and the case where reading it the
        // wrong way is worst: a stacked band spans the container, so a reading
        // taken across still varies with the pointer, while a tab is one Nth of
        // one strip and a reading taken DOWN answers the same for every point
        // in it — one half of the tab unreachable, silently.
        let mut scene = Scene::new();
        let key = |object| SurfaceKey { client: 1, object };
        for object in 1..=3 {
            scene
                .commit(key(object), surface([1, 1, 1, 0], 8, 8))
                .unwrap();
            if object == 2 {
                scene.command(Command::Move(crate::layout::Direction::Down));
            }
        }
        scene.command(Command::SetPresentation(
            crate::layout::Presentation::Tabbed,
        ));
        let (width, height) = (240, 600);
        assert_eq!(
            scene.layout.run_direction(key(2)),
            Some(Axis::Horizontal),
            "a tabbed leaf's run does not travel across"
        );

        let band = tile(&scene, width, height, 2).band;
        assert!(band.width < width, "the tab is the whole strip");
        // Both aimed BELOW the tab's own middle, so a reading taken down it
        // would answer `before: false` for the two of them.
        for (x, before) in [
            (band.x + 1, true),
            (band.x + band.width.saturating_sub(2), false),
        ] {
            scene.pointer_x = i32::try_from(x).unwrap();
            scene.pointer_y = i32::try_from(band.y + band.height - 1).unwrap();
            assert_eq!(
                scene.drop_target(width, height),
                Some((key(2), DropKind::InRun { before })),
                "tab at x={x}"
            );
            // And the block promising it stands on the edge the run runs to —
            // a bar down the tab's side, not across its top.
            let area = hint_area(
                &tile(&scene, width, height, 2),
                DropKind::InRun { before },
                Some(Axis::Horizontal),
            );
            assert_eq!(
                area.height, band.height,
                "the block is not the tab's height"
            );
            assert!(area.width < band.width, "the block spans the whole tab");
            assert_eq!(
                area.x,
                if before {
                    band.x
                } else {
                    band.x + band.width - area.width
                },
                "the block is on the wrong side of the tab"
            );
        }
    }

    #[test]
    fn a_drop_reads_five_zones_in_a_tile_and_two_along_a_band() {
        // A row of two and a column of two in one arrangement:
        // `H[1, V[2, 3]]`.
        let mut scene = Scene::new();
        for object in 1..=2 {
            scene
                .commit(
                    SurfaceKey { client: 1, object },
                    surface([1, 1, 1, 0], 8, 8),
                )
                .unwrap();
        }
        scene
            .commit(
                SurfaceKey {
                    client: 1,
                    object: 3,
                },
                surface([1, 1, 1, 0], 8, 8),
            )
            .unwrap();
        below(&mut scene, 3);
        // Tall enough that the COLUMN's two tiles each keep a real client
        // area. `least_output_height` reserves rows for one tile, and with two
        // the client comes out shorter than its own band — which makes a tile
        // and its band nearly the same rectangle, so a drop measured over the
        // whole tile agrees with one measured over the band and the two stop
        // being distinguishable.
        let (width, height) = (240, 600);
        let placements = scene.tiled_placements(width, height);
        assert!(
            placements
                .iter()
                .all(|placement| placement.rect.height > placement.band.height),
            "the output is too short to tell a tile from its band"
        );
        let at = |object: u32| {
            let index = placements
                .iter()
                .position(|placement| placement.key.object == object)
                .unwrap();
            *placements.get(index).unwrap()
        };
        let key = |object| SurfaceKey { client: 1, object };
        let go = |scene: &mut Scene, x: usize, y: usize| {
            scene.pointer_x = i32::try_from(x).unwrap();
            scene.pointer_y = i32::try_from(y).unwrap();
        };

        // The tile's own five zones reach the layout unchanged, and they do
        // NOT depend on the target's container: 1 sits in the ROW and still
        // answers above and below, which is the whole of what this commit
        // adds — a drop that makes the column it needs.
        let tile = at(1).rect;
        let middle = (tile.x + tile.width / 2, tile.y + tile.height / 2);
        for (x, y, expected) in [
            (middle.0, middle.1, DropKind::Swap),
            (
                middle.0,
                tile.y + 1,
                DropKind::Beside {
                    axis: Axis::Vertical,
                    before: true,
                },
            ),
            (
                middle.0,
                tile.y + tile.height - 2,
                DropKind::Beside {
                    axis: Axis::Vertical,
                    before: false,
                },
            ),
            (
                tile.x + 1,
                middle.1,
                DropKind::Beside {
                    axis: Axis::Horizontal,
                    before: true,
                },
            ),
            (
                tile.x + tile.width - 2,
                middle.1,
                DropKind::Beside {
                    axis: Axis::Horizontal,
                    before: false,
                },
            ),
        ] {
            go(&mut scene, x, y);
            assert_eq!(
                scene.drop_target(width, height),
                Some((key(1), expected)),
                "tile at {x},{y}"
            );
        }

        // A BAND keeps TWO zones along the run it is part of, because a strip
        // a line of text tall has no room for five and a run is a list rather
        // than an area. It names no axis at all — a place in the target's own
        // container, whatever that is.
        //
        // 2 sits in the COLUMN, so its band's run goes top to bottom and its
        // own top and bottom halves decide; its left and right do not.
        let band = at(2).band;
        for (x, y, before) in [
            (band.x + band.width / 2, band.y + 1, true),
            (band.x + band.width / 2, band.y + band.height - 1, false),
            (band.x + 1, band.y + 1, true),
            (band.x + band.width - 2, band.y + 1, true),
        ] {
            go(&mut scene, x, y);
            assert_eq!(
                scene.drop_target(width, height),
                Some((key(2), DropKind::InRun { before })),
                "column band at {x},{y}"
            );
        }

        // 1 sits in the ROW, where each leaf carries its own band at the top
        // of its own tile — so that run goes LEFT TO RIGHT and the half is
        // read across. Reading a band's height for this would make one side
        // of a row unreachable, every point in a 20-pixel strip answering the
        // same way.
        let row_band = at(1).band;
        for (x, before) in [
            (row_band.x + 1, true),
            (row_band.x + row_band.width - 2, false),
        ] {
            go(&mut scene, x, row_band.y + 1);
            assert_eq!(
                scene.drop_target(width, height),
                Some((key(1), DropKind::InRun { before })),
                "row band at x={x}"
            );
        }

        // A STACKED row: its bands run DOWN the container's top even though
        // the container itself is horizontal, so the half is read that way.
        scene.command(Command::Focus(crate::layout::Direction::Left));
        scene.command(Command::ToggleGrouped);
        let stacked = scene.tiled_placements(width, height);
        assert!(stacked.iter().all(|placement| placement.run.is_some()));
        let run = stacked.first().unwrap().band;
        for (y, before) in [(run.y + 1, true), (run.y + run.height - 1, false)] {
            go(&mut scene, run.x + run.width - 2, y);
            assert_eq!(
                scene.drop_target(width, height),
                Some((key(1), DropKind::InRun { before })),
                "stacked band at y={y}"
            );
        }

        // A stacked leaf's CONTENT area is a drop zone too, with the tile's
        // five, and only for the leaf that is SHOWN: the others' rectangles
        // cover the very same pixels, so without that guard a hidden one
        // would claim them. The shown leaf must not be the first, or a hit
        // test that ignored visibility would agree with this anyway.
        scene.command(Command::Focus(crate::layout::Direction::Down));
        let stacked = scene.tiled_placements(width, height);
        let shown = stacked
            .iter()
            .position(|placement| placement.visible)
            .unwrap();
        assert!(shown > 0, "the shown leaf is first, so this proves nothing");
        let shown_key = stacked.get(shown).unwrap().key;
        let content = stacked.get(shown).unwrap().rect;
        assert!(content.height > 0);
        for (y, expected) in [
            (
                content.y + 1,
                DropKind::Beside {
                    axis: Axis::Vertical,
                    before: true,
                },
            ),
            (content.y + content.height / 2, DropKind::Swap),
            (
                content.y + content.height - 2,
                DropKind::Beside {
                    axis: Axis::Vertical,
                    before: false,
                },
            ),
        ] {
            go(&mut scene, content.x + content.width / 2, y);
            assert_eq!(
                scene.drop_target(width, height),
                Some((shown_key, expected)),
                "stacked content at y={y}"
            );
        }

        // The desktop is neither: a release there is a cancelled drag.
        go(&mut scene, 0, height - 1);
        assert_eq!(scene.drop_target(width, height), None);
        assert_eq!(scene.band_press_at_pointer(width, height), None);
    }

    #[test]
    fn a_stacked_column_draws_every_band_but_only_the_focused_client() {
        let mut scene = Scene::new();
        let colors = [[1, 2, 3, 0], [4, 5, 6, 0], [7, 8, 9, 0]];
        for (index, color) in colors.iter().enumerate() {
            let object = u32::try_from(index).unwrap().saturating_add(1);
            scene
                .commit(SurfaceKey { client: 1, object }, surface(*color, 400, 400))
                .unwrap();
            // Only the SECOND makes the column; every later one joins it,
            // since a new window opens in the container the focused one is in.
            if index == 1 {
                below(&mut scene, object);
            }
        }
        let (width, height) = (320, least_output_height(200));
        let stride = width * 4;
        let mut frame = vec![0u8; stride * height];
        scene.pointer_x = 0;
        scene.pointer_y = i32::try_from(height - 1).unwrap();

        scene.command(Command::ToggleGrouped);

        // EVERY position in the stack, not just the one that happens to be
        // focused. All three leaves share the one content rectangle, so with
        // the last one shown a renderer that ignored visibility would paint
        // the other two and then overpaint them — green for the wrong reason,
        // which is what the first draft of this test did.
        for (index, shown) in colors.iter().enumerate() {
            let object = u32::try_from(index).unwrap().saturating_add(1);
            assert!(scene.focus_key(SurfaceKey { client: 1, object }));
            scene.render(&mut frame, width, height, stride);

            let placements = scene.tiled_placements(width, height);
            assert_eq!(placements.len(), 3);
            // Every band is painted, whether or not its client is shown: the
            // band is what says a stacked-away window is there.
            for placement in &placements {
                assert_eq!(
                    pixel(&frame, stride, placement.band.x + 1, placement.band.y + 1),
                    if placement.focused {
                        TITLE_FOCUSED
                    } else {
                        TITLE_UNFOCUSED
                    },
                    "a band went unpainted at {:?}",
                    placement.band
                );
            }
            // The frame a BORDER wraps is the client area ALONE in a stack.
            // The run's last band abuts the content exactly as an ordinary
            // band does, so joining it — which adjacency alone cannot refuse
            // — would draw that leaf's border four pixels above its own band,
            // inside the band before it, and only when the last of the stack
            // is shown.
            let content = placements.first().unwrap().rect;
            let run_top = placements.first().unwrap().band.y;
            let last_band = placements.last().unwrap().band;
            let above = Rect {
                x: last_band.x.saturating_sub(BORDER),
                y: run_top.saturating_sub(BORDER),
                width: last_band.width.saturating_add(BORDER * 2),
                height: last_band.y.saturating_sub(run_top.saturating_sub(BORDER)),
            };
            assert_eq!(
                count_color(&frame, stride, above, [0xc0, 0x70, 0xf0, 0]),
                0,
                "a border reached above the run's last band"
            );
            // The shown leaf's own top border SURVIVES the run. It rides four
            // pixels up into the last band, so a pass that drew each band
            // beside its border would erase it with a band belonging to a
            // later placement — every time the shown leaf is not the last.
            assert_eq!(
                pixel(&frame, stride, content.x + 10, content.y - 1),
                [0xc0, 0x70, 0xf0, 0],
                "the shown leaf's top border was painted over"
            );
            // Only the focused leaf's pixels reach the screen. The other two
            // are SIZED for the content area — so they keep their buffers
            // across a toggle — which is exactly why "not drawn" cannot be
            // inferred from the rectangle and has to be asserted here.
            assert!(
                frame.as_chunks::<4>().0.contains(shown),
                "the shown client {shown:?} never reached the screen"
            );
            for (other, hidden) in colors.iter().enumerate() {
                if other == index {
                    continue;
                }
                assert!(
                    !frame.as_chunks::<4>().0.contains(hidden),
                    "a stacked-away client painted {hidden:?}"
                );
            }

            // And a click anywhere in the content area reaches the shown leaf
            // only — never one of the two stacked behind it, whose rectangle
            // covers the very same pixels.
            for (dx, dy) in [(1, 1), (10, 20), (50, 100)] {
                scene.pointer_x = i32::try_from(content.x + dx).unwrap();
                scene.pointer_y = i32::try_from(content.y + dy).unwrap();
                assert_eq!(
                    scene
                        .pointer_targets(None, width, height)
                        .0
                        .map(|point| point.key),
                    Some(SurfaceKey { client: 1, object }),
                    "the hit test reached a stacked-away client at {dx},{dy}"
                );
            }
            // A grab is resolved the way a workspace switch resolves one: a
            // leaf that is not SHOWN is not somewhere the pointer can be, so
            // a grab held on it answers nothing rather than coordinates in a
            // window nobody can see.
            for other in 0..colors.len() {
                let held = SurfaceKey {
                    client: 1,
                    object: u32::try_from(other).unwrap().saturating_add(1),
                };
                assert_eq!(
                    scene.pointer_targets(Some(held), width, height).1.is_some(),
                    other == index,
                    "a grab on {held:?} resolved wrongly while {object} was shown"
                );
            }
            // Back to a corner, so the cursor never paints over the content
            // the next iteration is about to inspect.
            scene.pointer_x = 0;
            scene.pointer_y = i32::try_from(height - 1).unwrap();
        }
    }

    #[test]
    fn a_title_paints_into_its_own_band_and_nowhere_else() {
        let mut scene = Scene::new();
        let named = SurfaceKey {
            client: 1,
            object: 1,
        };
        let nameless = SurfaceKey {
            client: 1,
            object: 2,
        };
        scene.commit(named, surface([1, 2, 3, 0], 8, 8)).unwrap();
        scene.commit(nameless, surface([4, 5, 6, 0], 8, 8)).unwrap();
        // Longer than the tile is wide, so the clip is exercised rather than
        // assumed: 2x glyphs advance 12 pixels each, and the tile is 124.
        assert!(scene.set_title(named, "AB".repeat(40)));

        let (width, height) = (320, least_output_height(8));
        let stride = width * 4;
        let mut frame = vec![0u8; stride * height];
        // Parked outside every tile: the sprite paints last, over anything.
        scene.pointer_x = 0;
        scene.pointer_y = i32::try_from(height - 1).unwrap();
        scene.render(&mut frame, width, height, stride);

        let placements = scene.tiled_placements(width, height);
        let index = placements
            .iter()
            .position(|placement| placement.key == named)
            .unwrap();
        let band = placements.get(index).unwrap().band;
        let inside = count_color(&frame, stride, band, TITLE_TEXT);
        assert!(inside > 0, "the title never reached its band");
        // Every text pixel in the OUTPUT is one of those: the clip holds the
        // overlong title inside the band, and the untitled window's band is
        // bare rather than carrying a placeholder. One count answers both,
        // and a band drawn with no `draw_text_clipped` clip argument fails it.
        let whole = Rect {
            x: 0,
            y: 0,
            width,
            height,
        };
        assert_eq!(count_color(&frame, stride, whole, TITLE_TEXT), inside);
    }

    #[test]
    fn a_bands_buttons_sit_at_its_right_end_and_answer_only_there() {
        let mut scene = Scene::new();
        let key = |object| SurfaceKey { client: 1, object };
        for object in 1..=2 {
            scene
                .commit(key(object), surface([1, 1, 1, 0], 8, 8))
                .unwrap();
        }
        let (width, height) = (600, least_output_height(8));
        let band = tile(&scene, width, height, 1).band;
        let rects = band_buttons(band).expect("a full-width band carries buttons");

        // Flush to the band's right edge, adjacent, and inside it.
        assert_eq!(
            rects.last().map(|slot| slot.x.saturating_add(slot.width)),
            Some(band.x.saturating_add(band.width))
        );
        for pair in rects.windows(2) {
            let (left, right) = (pair.first().unwrap(), pair.get(1).unwrap());
            assert_eq!(
                left.x.saturating_add(left.width),
                right.x,
                "a gap or overlap"
            );
        }
        // A whole title CELL beside them, not merely a nonzero gap — which
        // clearing the threshold at all would guarantee, and so would prove
        // nothing. `a_bands_least_width_...` pins the threshold itself.
        assert!(
            rects.first().unwrap().x.saturating_sub(band.x)
                >= TITLE_TEXT_LEFT + ui::GLYPH_ADVANCE * TITLE_SCALE,
            "no room left for a name"
        );

        // Each button answers over its own slot and nothing answers left of
        // the first — that part of the band is still a drag handle.
        for (index, slot) in rects.iter().enumerate() {
            scene.pointer_x = i32::try_from(slot.x + slot.width / 2).unwrap();
            scene.pointer_y = i32::try_from(slot.y + slot.height / 2).unwrap();
            assert_eq!(
                scene.band_press_at_pointer(width, height),
                Some(BandPress::Button(key(1), *BUTTONS.get(index).unwrap())),
                "button {index}"
            );
        }
        scene.pointer_x = i32::try_from(rects.first().unwrap().x.saturating_sub(1)).unwrap();
        assert_eq!(
            scene.band_press_at_pointer(width, height),
            Some(BandPress::Handle(key(1))),
            "the strip's left stopped being a drag handle"
        );
    }

    #[test]
    fn the_first_window_carries_buttons_and_they_reach_its_workspace() {
        // The band an operator sees FIRST used to be the one that said least:
        // a lone window is in no container, so it had no presentation and its
        // band offered nothing until a second window existed. Its workspace is
        // its container now, so the buttons are there and pressing one is real.
        let mut scene = Scene::new();
        let key = |object| SurfaceKey { client: 1, object };
        scene.commit(key(1), surface([1, 1, 1, 0], 8, 8)).unwrap();
        let (width, height) = (600, least_output_height(8));
        assert_eq!(scene.tiled_placements(width, height).len(), 1);
        let alone = tile(&scene, width, height, 1);
        assert!(
            scene.band_buttons_of(&alone).is_some(),
            "the first window's band carries no buttons"
        );

        // Pressing one is not a no-op: the mark moves, and the choice is
        // waiting for the window that makes it visible.
        let lit = |scene: &Scene| {
            let frame = painted(scene, width, height);
            let band = tile(scene, width, height, 1).band;
            band_buttons(band)
                .unwrap()
                .iter()
                .map(|slot| count_color(&frame, width * 4, *slot, BUTTON_INK_ON) > 0)
                .collect::<Vec<_>>()
        };
        assert_eq!(lit(&scene), [false, false, true], "split is not marked");
        let slots = band_buttons(alone.band).unwrap();
        // Pressed where the pointer would press it, not by command: the hit
        // test is the half of a button an operator uses, and a lone window is
        // the case that answered nothing. Aimed before the frame below is
        // taken, since the pointer is itself something the scene draws.
        let stack = slots.first().copied().unwrap();
        scene.pointer_x = i32::try_from(stack.x + stack.width / 2).unwrap();
        scene.pointer_y = i32::try_from(stack.y + stack.height / 2).unwrap();
        assert_eq!(
            scene.band_press_at_pointer(width, height),
            Some(BandPress::Button(
                key(1),
                crate::layout::Presentation::Stacked
            )),
            "the first window's band answered no button"
        );
        // And the press moves NOTHING else. This is an assertion about PIXELS
        // and it has to be: a lone leaf laid out as a run of one would draw
        // the same tile and lose the border around its own title bar, since
        // `frame_rect` drops the band for any placement in a run. Both grouped
        // presentations are checked, because the lone-leaf case is served by
        // one line and a line can be written for one of them.
        let ungrouped = painted(&scene, width, height);
        let stride = width * 4;
        let moved_off_the_buttons = |after: &[u8]| {
            let mut moved = 0usize;
            for y in 0..height {
                for x in 0..width {
                    let at = y * stride + x * 4;
                    if ungrouped.get(at..at + 4) != after.get(at..at + 4)
                        && !slots.iter().any(|slot| contains(*slot, x, y))
                    {
                        moved = moved.saturating_add(1);
                    }
                }
            }
            moved
        };
        for (wanted, marks) in [
            (crate::layout::Presentation::Stacked, [true, false, false]),
            (crate::layout::Presentation::Tabbed, [false, true, false]),
        ] {
            scene.command(Command::SetPresentation(wanted));
            assert_eq!(lit(&scene), marks, "{wanted:?} did not take");
            assert_eq!(
                moved_off_the_buttons(&painted(&scene, width, height)),
                0,
                "{wanted:?} moved a lone window's pixels off its buttons"
            );
        }

        // A second window opens INTO that choice rather than ignoring it.
        scene.commit(key(2), surface([1, 1, 1, 0], 8, 8)).unwrap();
        assert!(
            scene
                .tiled_placements(width, height)
                .iter()
                .all(|placement| placement.run == Some(Axis::Horizontal)),
            "the workspace's tabs did not reach the container it grew"
        );
    }

    #[test]
    fn a_band_with_no_room_for_buttons_carries_none() {
        // A column of six tabs across a narrow output: each tab is a fraction
        // of one strip, and buttons there would be the whole tab with the
        // title squeezed out. The painter and the hit test come through the
        // same gate, so neither draws one either.
        let mut scene = Scene::new();
        let key = |object| SurfaceKey { client: 1, object };
        let height = least_output_height(8);
        for object in 1..=7 {
            scene
                .commit(key(object), surface([1, 1, 1, 0], 8, 8))
                .unwrap();
            if object == 3 {
                scene.command(Command::Move(crate::layout::Direction::Down));
            }
        }
        scene.command(Command::SetPresentation(
            crate::layout::Presentation::Tabbed,
        ));
        let narrow = 240;
        let tab = tile(&scene, narrow, height, 4);
        assert_eq!(tab.run, Some(Axis::Horizontal), "the column did not tab");
        assert!(band_buttons(tab.band).is_none(), "a tab found room");
        assert!(scene.band_buttons_of(&tab).is_none());
        scene.pointer_x = i32::try_from(tab.band.x + tab.band.width - 1).unwrap();
        scene.pointer_y = i32::try_from(tab.band.y + 1).unwrap();
        assert_eq!(
            scene.band_press_at_pointer(narrow, height),
            Some(BandPress::Handle(key(4))),
            "a narrow tab answered a button instead of a handle"
        );
    }

    #[test]
    fn the_stack_button_is_two_lines_over_a_thicker_window_and_fills_its_icon() {
        // Three marks are what tell this icon from the other two, and the
        // arithmetic has to hold at the band height the compositor actually
        // uses as well as at the edges. Counted as RUNS of ink rather than
        // rows: two marks that merged would colour the same rows and read as
        // one thicker mark on screen.
        // EVERY band height that carries buttons rather than a sample of
        // three. A band is `min(TITLE_HEIGHT, tile height)`, so the reachable
        // set is the gate's threshold up to a full band — nine heights — and
        // a 40 past it, since this arithmetic should not turn over on a band
        // taller than the one drawn today. Three samples left the middle
        // uncovered, where dividing by 5 rather than by the gate's own
        // constant draws three equal bars that pass every assertion below.
        for height in (BUTTON_ICON_LEAST + BUTTON_INSET * 2..=TITLE_HEIGHT).chain([40]) {
            let slot = Rect {
                x: 0,
                y: 0,
                width: BUTTON_WIDTH,
                height,
            };
            let (width, stride) = (BUTTON_WIDTH, BUTTON_WIDTH * 4);
            let mut frame = vec![0u8; stride * height];
            draw_button(
                &mut frame,
                width,
                height,
                stride,
                slot,
                Presentation::Stacked,
                BUTTON_INK,
            );
            let inked = |y: usize| {
                let row = Rect {
                    x: 0,
                    y,
                    width,
                    height: 1,
                };
                count_color(&frame, stride, row, BUTTON_INK) > 0
            };
            // Thicknesses of each run of ink, top to bottom.
            let mut runs: Vec<usize> = Vec::new();
            for y in 0..height {
                match (inked(y), y > 0 && inked(y - 1)) {
                    (true, false) => runs.push(1),
                    (true, true) => {
                        if let Some(last) = runs.last_mut() {
                            *last += 1;
                        }
                    }
                    _ => {}
                }
            }
            assert_eq!(
                runs.len(),
                3,
                "the stack icon is not three marks at {height}"
            );
            // The last is the WINDOW under two collapsed titles, so it is the
            // thickest — three equal bars would be a hamburger menu and would
            // say nothing about which leaf a stack shows.
            let body = runs.last().copied().unwrap();
            assert!(
                runs.iter().take(2).all(|thickness| body > *thickness),
                "the stack icon's body is not thicker than its lines at {height}"
            );
            // And it fills the icon rather than sitting short inside it, or it
            // reads as a smaller button beside its neighbours.
            let icon = inset(slot, BUTTON_INSET).unwrap();
            assert!(
                inked(icon.y),
                "the icon starts below its own top at {height}"
            );
            assert!(
                inked(icon.y + icon.height - 1),
                "the icon stops above its own bottom at {height}"
            );
            // And NOWHERE outside it. `ui::fill` bounds a mark to the FRAME
            // and not to the slot, so ink that overran the icon would land on
            // the band or on the neighbouring button — which is why the body
            // takes exactly what the lines leave and is given no floor.
            for y in (0..icon.y).chain(icon.y.saturating_add(icon.height)..height) {
                assert!(!inked(y), "ink at row {y} outside the icon at {height}");
            }
        }
    }

    #[test]
    fn a_band_too_short_to_draw_an_icon_carries_no_buttons_either() {
        // The refusal has to be the SAME question the painter asks, or the
        // band's last 48 pixels answer a press with nothing drawn there — a
        // short tile keeps its band and loses its client, so this is reachable
        // rather than theoretical.
        let wide = 600;
        let least = BUTTON_ICON_LEAST + BUTTON_INSET * 2;
        let draws = |height: usize, shows| {
            let stride = wide * 4;
            let mut frame = vec![0u8; stride * height.max(1)];
            draw_button(
                &mut frame,
                wide,
                height.max(1),
                stride,
                Rect {
                    x: 0,
                    y: 0,
                    width: BUTTON_WIDTH,
                    height,
                },
                shows,
                BUTTON_INK,
            );
            frame.chunks(4).any(|pixel| pixel == BUTTON_INK)
        };
        // Whenever the band answers, all THREE icons have something to draw —
        // the implication that matters, since `draw_button` is only ever
        // reached through this gate.
        for height in 0..=(least + 8) {
            let band = Rect {
                x: 0,
                y: 0,
                width: wide,
                height,
            };
            if band_buttons(band).is_none() {
                continue;
            }
            for shows in BUTTONS {
                assert!(
                    draws(height, shows),
                    "height {height} answers but draws nothing for {shows:?}"
                );
            }
        }
        // And the threshold is where it is: one pixel shorter refuses.
        let band = |height| Rect {
            x: 0,
            y: 0,
            width: wide,
            height,
        };
        assert!(
            band_buttons(band(least)).is_some(),
            "the least height refuses"
        );
        assert!(
            band_buttons(band(least - 1)).is_none(),
            "a band one pixel short still carries buttons"
        );
    }

    #[test]
    fn a_bands_least_width_leaves_a_whole_title_cell_beside_the_buttons() {
        // The reserve is a glyph at the scale titles are DRAWN at. Reserving an
        // unscaled one leaves half a cell, which is the clipped smear the
        // threshold exists to prevent.
        let cell = ui::GLYPH_ADVANCE * TITLE_SCALE;
        let least = BUTTON_WIDTH * BUTTONS.len() + TITLE_TEXT_LEFT + cell;
        let band = |width| Rect {
            x: 0,
            y: 0,
            width,
            height: TITLE_HEIGHT,
        };
        assert!(
            band_buttons(band(least)).is_some(),
            "the least width refuses"
        );
        assert!(
            band_buttons(band(least - 1)).is_none(),
            "a band one pixel short still carries buttons"
        );
        // The gap the title gets at that width is a whole cell, not part of one.
        let rects = band_buttons(band(least)).unwrap();
        let text = rects.first().unwrap().x - TITLE_TEXT_LEFT;
        assert!(text >= cell, "the name has less than one cell to sit in");
    }

    #[test]
    fn a_bands_buttons_mark_the_presentation_its_container_is_in() {
        let mut scene = Scene::new();
        let key = |object| SurfaceKey { client: 1, object };
        for object in 1..=2 {
            scene
                .commit(key(object), surface([1, 1, 1, 0], 8, 8))
                .unwrap();
        }
        let (width, height) = (600, least_output_height(8));
        let lit = |scene: &Scene| {
            let frame = painted(scene, width, height);
            let band = tile(scene, width, height, 1).band;
            band_buttons(band)
                .unwrap()
                .iter()
                .map(|slot| count_color(&frame, width * 4, *slot, BUTTON_INK_ON) > 0)
                .collect::<Vec<_>>()
        };
        // Exactly one is marked, and it moves with the presentation — the
        // band says which of the three the container is in as well as
        // offering the other two.
        assert_eq!(lit(&scene), [false, false, true], "split is not marked");
        scene.command(Command::SetPresentation(
            crate::layout::Presentation::Stacked,
        ));
        assert_eq!(lit(&scene), [true, false, false], "stacked is not marked");
        scene.command(Command::SetPresentation(
            crate::layout::Presentation::Tabbed,
        ));
        assert_eq!(lit(&scene), [false, true, false], "tabbed is not marked");
    }

    #[test]
    fn a_title_stops_before_the_buttons_rather_than_running_under_them() {
        let mut scene = Scene::new();
        let key = |object| SurfaceKey { client: 1, object };
        for object in 1..=2 {
            scene
                .commit(key(object), surface([1, 1, 1, 0], 8, 8))
                .unwrap();
        }
        // Long enough to reach the strip and past it.
        assert!(scene.set_title(key(1), "AB".repeat(60)));
        let (width, height) = (600, least_output_height(8));
        scene.pointer_x = 0;
        scene.pointer_y = i32::try_from(height - 1).unwrap();
        let frame = painted(&scene, width, height);
        let band = tile(&scene, width, height, 1).band;
        let rects = band_buttons(band).unwrap();
        let strip = Rect {
            x: rects.first().unwrap().x,
            y: band.y,
            width: band
                .x
                .saturating_add(band.width)
                .saturating_sub(rects.first().unwrap().x),
            height: band.height,
        };
        assert!(
            count_color(&frame, width * 4, band, TITLE_TEXT) > 0,
            "the title never reached its band"
        );
        assert_eq!(
            count_color(&frame, width * 4, strip, TITLE_TEXT),
            0,
            "the title ran under the buttons"
        );
    }

    #[test]
    fn a_tabs_title_is_clipped_to_its_share_of_the_strip() {
        // The clip that holds an overlong title inside a full-width band is
        // the same call for a tab, and a tab is where it MATTERS: the bands
        // are adjacent along the direction the text runs, so a title that
        // overflowed would land in the neighbouring tab's name rather than in
        // a gap. The other two are left untitled so one count answers it.
        let mut scene = Scene::new();
        let mut keys = Vec::new();
        for object in 1..=3 {
            let key = SurfaceKey { client: 1, object };
            scene.commit(key, surface([1, 2, 3, 0], 8, 8)).unwrap();
            if object == 2 {
                scene.command(Command::Move(crate::layout::Direction::Down));
            }
            keys.push(key);
        }
        let named = *keys.first().unwrap();
        assert!(scene.set_title(named, "AB".repeat(40)));
        scene.command(Command::SetPresentation(
            crate::layout::Presentation::Tabbed,
        ));

        let (width, height) = (320, least_output_height(8));
        let stride = width * 4;
        let mut frame = vec![0u8; stride * height];
        scene.pointer_x = 0;
        scene.pointer_y = i32::try_from(height - 1).unwrap();
        scene.render(&mut frame, width, height, stride);

        let placements = scene.tiled_placements(width, height);
        let index = placements
            .iter()
            .position(|placement| placement.key == named)
            .unwrap();
        let band = placements.get(index).unwrap().band;
        assert!(
            placements
                .iter()
                .all(|placement| placement.run == Some(Axis::Horizontal)),
            "the column did not tab, so this proves nothing"
        );
        assert!(band.width < width, "the tab is the whole strip");
        let inside = count_color(&frame, stride, band, TITLE_TEXT);
        assert!(inside > 0, "the title never reached its tab");
        let whole = Rect {
            x: 0,
            y: 0,
            width,
            height,
        };
        assert_eq!(count_color(&frame, stride, whole, TITLE_TEXT), inside);
    }

    #[test]
    fn the_bar_names_the_workspaces_and_the_mark_follows_a_switch() {
        // The strip is the ONLY thing on screen that says which workspace an
        // operator is on: switching to an empty one leaves a bare desktop,
        // which is what the workspace they left looks like from behind a
        // fullscreen window. Driven through the scene rather than over
        // `bar::paint` directly, since what the strip is handed comes from the
        // layout and nothing else checks that it is asked at paint time.
        let mut scene = Scene::new();
        let key = |object| SurfaceKey { client: 1, object };
        let (width, height) = (600, least_output_height(8));
        let stride = width * 4;
        scene.commit(key(1), surface([1, 1, 1, 0], 8, 8)).unwrap();
        scene.command(Command::SwitchWorkspace(3));
        scene.commit(key(2), surface([1, 1, 1, 0], 8, 8)).unwrap();
        // The pointer paints last and parks at the origin, which is the first
        // cell — so it is moved off the strip before any of this is read.
        scene.pointer_y = i32::try_from(height - 1).unwrap();

        // Two workspaces hold a window, so the strip names both — and the
        // BLOCK is on the one being looked at.
        let blocks = |scene: &Scene| {
            let frame = painted(scene, width, height);
            // The block's OWN top row: the number is punched out of it lower
            // down, so a column of solid ink is not what a marked cell is.
            let mut runs = Vec::new();
            for x in 0..width {
                let inked = pixel(&frame, stride, x, 0) == bar::INK;
                match (inked, runs.last_mut()) {
                    (true, Some((_, end))) if *end == x => *end = x + 1,
                    (true, _) => runs.push((x, x + 1)),
                    _ => {}
                }
            }
            runs
        };
        let on_three = blocks(&scene);
        assert_eq!(
            on_three.len(),
            1,
            "one workspace is marked, not {on_three:?}"
        );
        scene.command(Command::SwitchWorkspace(1));
        let on_one = blocks(&scene);
        assert_eq!(on_one.len(), 1, "one workspace is marked, not {on_one:?}");
        // The mark MOVED, and leftwards, because 1 sorts before 3.
        assert!(
            on_one.first() < on_three.first(),
            "the mark did not follow the switch: {on_one:?} then {on_three:?}"
        );

        // A workspace nobody has been to is not on the strip; one the operator
        // switches to is, empty or not.
        scene.command(Command::SwitchWorkspace(7));
        let on_seven = blocks(&scene);
        assert_eq!(on_seven.len(), 1, "an empty workspace lost its mark");
        assert!(
            on_seven.first() > on_three.first(),
            "7 is not to the right of 3"
        );
    }

    #[test]
    fn a_fullscreen_tile_stops_at_the_bar_rather_than_covering_them() {
        // This is where the reservation is load-bearing and the tiled case is
        // not: a tiled placement is already inset by GAP, which happens to
        // equal BAR_HEIGHT, so removing the offset leaves it looking right.
        // Fullscreen has no gap at all — it is the full area or nothing — so
        // it is the only arrangement whose pixels reach row 0 without it.
        let mut scene = Scene::new();
        let key = SurfaceKey {
            client: 1,
            object: 1,
        };
        scene.commit(key, surface([9, 8, 7, 0], 400, 400)).unwrap();
        scene.command(Command::ToggleFullscreen);
        for (width, height) in [(320, 200), (640, 400)] {
            // `position`, not the iterator method whose name is also a shell
            // command: this file is `include_str!`'d into the td-compositor
            // recipe, so its text is scanned as a bootstrap step's.
            let views = scene.views(width, height);
            let index = views.iter().position(|view| view.key == key).unwrap();
            let view = *views.get(index).unwrap();
            assert_eq!(view.rect.y, BAR_HEIGHT, "{width}x{height}");
            assert_eq!(view.rect.x, 0, "{width}x{height}");
            assert_eq!(view.rect.height, height - BAR_HEIGHT, "{width}x{height}");
            assert_eq!(view.rect.width, width, "{width}x{height}");

            // And the pixels agree: the bar's rows keep their own colour and
            // the client's start immediately below them. The pointer sprite
            // is parked below the bar first — it paints last and would
            // otherwise be the thing found in row 0.
            let stride = width * 4;
            let mut frame = vec![0u8; stride * height];
            scene.pointer_x = i32::try_from(width / 4).unwrap();
            scene.pointer_y = i32::try_from(height - 1).unwrap();
            scene.render(&mut frame, width, height, stride);
            for y in 0..BAR_HEIGHT {
                // The leftmost pixels are the active workspace's cell, which
                // is the strip's two colours exchanged, so only THAT sample is
                // allowed either of them. The rest stay pinned to the bar's
                // background, or a strip that spanned the whole output would
                // pass this.
                let found = pixel(&frame, stride, 0, y);
                assert!(
                    found == bar::BACKGROUND || found == bar::INK,
                    "{width}x{height}: fullscreen reached the bar at 0,{y}"
                );
                for x in [width / 2, width - 1] {
                    assert_eq!(
                        pixel(&frame, stride, x, y),
                        bar::BACKGROUND,
                        "{width}x{height}: fullscreen reached the bar at {x},{y}"
                    );
                }
            }
            assert_eq!(pixel(&frame, stride, 0, BAR_HEIGHT), [9, 8, 7, 0]);

            // A click in ANY of the bar's rows and columns reaches no tile,
            // even though a fullscreen surface covers every column below.
            for y in 0..BAR_HEIGHT {
                for x in [0, width / 2, width - 1] {
                    scene.pointer_x = i32::try_from(x).unwrap();
                    scene.pointer_y = i32::try_from(y).unwrap();
                    assert_eq!(
                        scene.pointer_target(width, height),
                        None,
                        "{width}x{height}: the bar is clickable at {x},{y}"
                    );
                }
            }
            scene.pointer_x = 0;
            scene.pointer_y = i32::try_from(BAR_HEIGHT).unwrap();
            assert_eq!(
                scene.pointer_target(width, height).map(|point| point.key),
                Some(key)
            );
        }
    }

    #[test]
    fn an_output_no_taller_than_the_bar_tiles_nothing_and_paints_no_further() {
        let mut scene = Scene::new();
        scene
            .commit(
                SurfaceKey {
                    client: 1,
                    object: 1,
                },
                surface([1, 2, 3, 0], 8, 8),
            )
            .unwrap();
        for height in [0, 1, BAR_HEIGHT] {
            let width = 64usize;
            let stride = width * 4;
            // Rows past the output, so "paints no further" is a readable
            // property rather than a buffer length that cannot change.
            let guard = 4usize;
            let mut frame = vec![0u8; stride * (height + guard)];
            scene.render(&mut frame, width, height, stride);
            assert!(
                frame
                    .get(stride * height..)
                    .is_some_and(|rows| rows.iter().all(|byte| *byte == 0)),
                "{height}: painted past the output"
            );
            assert!(scene
                .views(width, height)
                .iter()
                .all(|view| view.rect.height == 0 || view.rect.width == 0));
        }
    }

    #[test]
    fn renderer_uses_tiling_geometry_gaps_and_focus_borders() {
        let mut scene = Scene::new();
        for (object, color) in [(1, [1, 2, 3, 0]), (2, [4, 5, 6, 0])] {
            scene
                .commit(SurfaceKey { client: 1, object }, surface(color, 100, 100))
                .unwrap();
        }
        let width = 120usize;
        let height = 80 + BAR_HEIGHT;
        let stride = width * 4;
        let mut frame = vec![0; stride * height];
        scene.render(&mut frame, width, height, stride);

        // A tile's top rows are its title band now, and the client's own
        // pixels start below it. Both are asserted, so a band that stopped
        // being drawn and a client that stopped being pushed down are two
        // different failures rather than one.
        assert_eq!(tiled(&frame, stride, 24, 24), TITLE_UNFOCUSED);
        assert_eq!(tiled(&frame, stride, 72, 24), TITLE_FOCUSED);
        assert_eq!(tiled(&frame, stride, 24, 24 + TITLE_HEIGHT), [1, 2, 3, 0]);
        assert_eq!(tiled(&frame, stride, 72, 24 + TITLE_HEIGHT), [4, 5, 6, 0]);
        assert_eq!(tiled(&frame, stride, 60, 30), [0x30, 0x25, 0x20, 0]);
        assert_eq!(tiled(&frame, stride, 68, 30), [0xc0, 0x70, 0xf0, 0]);
        assert_eq!(tiled(&frame, stride, 20, 30), [0x70, 0x70, 0x70, 0]);
        // The bar owns its own rows and the tiles start below them.
        assert_eq!(pixel(&frame, stride, 24, 0), [0x18, 0x14, 0x20, 0]);
        assert_eq!(
            pixel(&frame, stride, 24, BAR_HEIGHT - 1),
            [0x18, 0x14, 0x20, 0]
        );

        scene.command(Command::Focus(crate::layout::Direction::Left));
        scene.render(&mut frame, width, height, stride);
        assert_eq!(tiled(&frame, stride, 20, 30), [0xc0, 0x70, 0xf0, 0]);
        assert_eq!(tiled(&frame, stride, 68, 30), [0x70, 0x70, 0x70, 0]);
        // The band follows focus with the border, and the two are separate
        // colours: a band that took the border's would say nothing new.
        assert_eq!(tiled(&frame, stride, 24, 24), TITLE_FOCUSED);
        assert_eq!(tiled(&frame, stride, 72, 24), TITLE_UNFOCUSED);
    }

    #[test]
    fn committing_an_existing_surface_updates_without_mapping_twice() {
        let mut scene = Scene::new();
        let key = SurfaceKey {
            client: 1,
            object: 9,
        };
        scene.commit(key, surface([1, 2, 3, 0], 1, 1)).unwrap();
        scene.commit(key, surface([4, 5, 6, 0], 2, 2)).unwrap();
        assert_eq!(scene.surfaces.len(), 1);
        assert_eq!(scene.layout.placements(100, 100, 0, 0).len(), 1);
        assert_eq!(scene.surface_bytes, 16);
        assert!(scene.layout.check_invariants().is_ok());
    }

    #[test]
    fn workspaces_render_only_their_own_surfaces() {
        let mut scene = Scene::new();
        scene
            .commit(
                SurfaceKey {
                    client: 1,
                    object: 1,
                },
                surface([1, 2, 3, 0], 100, 100),
            )
            .unwrap();
        scene
            .commit(
                SurfaceKey {
                    client: 1,
                    object: 2,
                },
                surface([4, 5, 6, 0], 100, 100),
            )
            .unwrap();
        scene.command(Command::MoveToWorkspace(2));
        let mut frame = vec![0; 100 * 100 * 4];
        scene.command(Command::SwitchWorkspace(2));
        scene.render(&mut frame, 100, 100, 100 * 4);
        assert!(frame.as_chunks::<4>().0.contains(&[4, 5, 6, 0]));
        assert!(!frame.as_chunks::<4>().0.contains(&[1, 2, 3, 0]));
        scene.command(Command::SwitchWorkspace(1));
        scene.render(&mut frame, 100, 100, 100 * 4);
        assert!(frame.as_chunks::<4>().0.contains(&[1, 2, 3, 0]));
        assert!(!frame.as_chunks::<4>().0.contains(&[4, 5, 6, 0]));
    }

    #[test]
    fn pointer_motion_clamps_to_the_output() {
        let mut scene = Scene::new();
        // The answer is whether the pointer MOVED, not whether one was asked
        // for: everything downstream — the paint owed, the focus re-answered,
        // the drop re-derived — turns on the position having changed.
        assert!(!scene.move_pointer(-9, -4, 10, 8));
        assert_eq!((scene.pointer_x, scene.pointer_y), (0, 0));
        assert!(scene.move_pointer(i32::MAX, i32::MAX, 10, 8));
        assert_eq!((scene.pointer_x, scene.pointer_y), (9, 7));
        // Asked for, clamped away, and so not a move.
        assert!(!scene.move_pointer(i32::MAX, i32::MAX, 10, 8));
        assert_eq!((scene.pointer_x, scene.pointer_y), (9, 7));
        // One axis alone is enough to be one.
        assert!(scene.move_pointer(0, -1, 10, 8));
        assert!(!scene.move_pointer(0, 0, 10, 8));
        assert!(scene.move_pointer(1, 1, 0, 0));
        assert_eq!((scene.pointer_x, scene.pointer_y), (0, 0));
    }

    #[test]
    fn an_absolute_pointer_reaches_both_edges_of_the_output() {
        let mut scene = Scene::new();
        let tablet = |numerator| Fraction {
            numerator,
            denominator: 32767,
        };
        // The far edge EXACTLY, which is what this exists for: a relative
        // device on a host that warps its own cursor cannot be relied on to
        // arrive at the last column, and the operator sees a strip of screen
        // they cannot reach.
        assert!(scene.place_pointer(tablet(32767), tablet(32767), 800, 600));
        assert_eq!((scene.pointer_x, scene.pointer_y), (799, 599));
        // And back to the near one.
        assert!(scene.place_pointer(tablet(0), tablet(0), 800, 600));
        assert_eq!((scene.pointer_x, scene.pointer_y), (0, 0));

        // Halfway is halfway on both axes at once — the pair always arrives
        // together, the reader having completed whichever the device left out.
        assert!(scene.place_pointer(tablet(16384), tablet(16384), 800, 600));
        assert_eq!((scene.pointer_x, scene.pointer_y), (400, 300));
        // Reporting the position it already has is not a move — and an
        // absolute device re-sends one far more readily than a relative one
        // sends a zero delta, so this is the common case rather than a
        // curiosity.
        assert!(!scene.place_pointer(tablet(16384), tablet(16384), 800, 600));

        // Degenerate inputs answer the near edge rather than dividing or
        // running off: no output to be anywhere on, and no span to be
        // anywhere along.
        assert_eq!(across(tablet(32767), 0), 0);
        assert_eq!(
            across(
                Fraction {
                    numerator: 5,
                    denominator: 0
                },
                800
            ),
            0
        );
        // Past the end is the end, not past the screen.
        assert_eq!(across(tablet(999_999), 800), 799);
    }

    /// The composed claim, and the one that decides whether the operator can
    /// reach the right-hand column at all. QEMU scales the host pointer onto
    /// `0..=0x7fff` with an integer division that FLOORS, so it never emits
    /// 0x7fff and every column arrives a shade low. Flooring a second time here
    /// would cost a pixel almost everywhere and the last column entirely, which
    /// is the bug this whole increment is about — so every host column must
    /// come back as itself.
    #[test]
    fn every_host_column_survives_qemus_own_scaling() {
        // These five are SAMPLES of a property that is total over the
        // supported range: the round trip returns every column exactly while
        // the width is at most 16384, which is `MAX_UI_DIMENSION`. That bound
        // is asserted beside `across` at compile time rather than here,
        // because raising it would cost a column at widths no sample names.
        for extent in [640usize, 800, 1024, 1920, 3840] {
            for column in 0..extent {
                let value = u32::try_from(column as u64 * 32767 / extent as u64).unwrap();
                let landed = across(
                    Fraction {
                        numerator: value,
                        denominator: 32767,
                    },
                    extent,
                );
                assert_eq!(
                    landed,
                    i32::try_from(column).unwrap(),
                    "column {column} of {extent} left as {value} and came back as {landed}"
                );
            }
        }
    }

    #[test]
    fn pointer_hit_testing_uses_visible_surface_pixels_and_local_coordinates() {
        let mut scene = Scene::new();
        let key = SurfaceKey {
            client: 4,
            object: 9,
        };
        scene
            .commit(
                key,
                Surface {
                    width: 10,
                    height: 8,
                    pixels: vec![0; 10 * 8 * 4],
                    format: SHM_XRGB8888,
                },
            )
            .unwrap();
        let view = scene
            .views(100, least_output_height(8))
            .first()
            .copied()
            .unwrap();
        let x = i32::try_from(view.rect.x.saturating_add(3)).unwrap();
        let y = i32::try_from(view.rect.y.saturating_add(4)).unwrap();
        scene.move_pointer(x, y, 100, least_output_height(8));
        assert_eq!(
            scene.pointer_target(100, least_output_height(8)),
            Some(SurfacePoint { key, x: 3, y: 4 })
        );

        scene.move_pointer(20, 0, 100, least_output_height(8));
        assert_eq!(scene.pointer_target(100, least_output_height(8)), None);
        assert_eq!(
            scene.pointer_target_for(key, 100, least_output_height(8)),
            Some(SurfacePoint { key, x: 23, y: 4 })
        );
        scene.unmap(key);
        assert_eq!(
            scene.pointer_target_for(key, 100, least_output_height(8)),
            None
        );
    }

    #[test]
    fn input_regions_include_additions_exclude_holes_and_reset_to_infinite() {
        let mut scene = Scene::new();
        let key = SurfaceKey {
            client: 4,
            object: 9,
        };
        scene.commit(key, surface([1, 2, 3, 0], 10, 8)).unwrap();
        let view = scene
            .views(100, least_output_height(8))
            .first()
            .copied()
            .unwrap();
        let x = i32::try_from(view.rect.x.saturating_add(3)).unwrap();
        let y = i32::try_from(view.rect.y.saturating_add(4)).unwrap();
        scene.move_pointer(x, y, 100, least_output_height(8));

        let empty = InputRegion::new();
        assert!(scene.set_input_region(key, Some(Arc::new(empty))));
        assert_eq!(scene.pointer_target(100, least_output_height(8)), None);

        let mut region = InputRegion::new();
        assert!(region.add(0, 0, 10, 8));
        assert!(region.subtract(2, 3, 3, 3));
        assert!(scene.set_input_region(key, Some(Arc::new(region))));
        assert_eq!(scene.pointer_target(100, least_output_height(8)), None);
        scene.move_pointer(-2, -3, 100, least_output_height(8));
        assert_eq!(
            scene.pointer_target(100, least_output_height(8)),
            Some(SurfacePoint { key, x: 1, y: 1 })
        );

        assert!(scene.set_input_region(key, None));
        scene.move_pointer(2, 3, 100, least_output_height(8));
        assert_eq!(
            scene.pointer_target(100, least_output_height(8)),
            Some(SurfacePoint { key, x: 3, y: 4 })
        );
        let mut bounded = InputRegion::new();
        assert!(!bounded.add(0, 0, 0, 1));
        for x in 0..MAX_INPUT_REGION_OPERATIONS {
            assert!(bounded.add(i32::try_from(x).unwrap(), 0, 1, 1));
        }
        assert!(!bounded.add(0, 0, 1, 1));
        assert_eq!(bounded.len(), MAX_INPUT_REGION_OPERATIONS);
    }

    #[test]
    fn tiny_split_preserves_client_pixels_and_zero_area_tiles_draw_nothing() {
        let mut scene = Scene::new();
        for (object, color) in [(1, [1, 2, 3, 0]), (2, [4, 5, 6, 0]), (3, [7, 8, 9, 0])] {
            scene
                .commit(SurfaceKey { client: 1, object }, surface(color, 2, 2))
                .unwrap();
        }
        let height = least_output_height(2);
        let mut frame = vec![0; 2 * height * 4];
        scene.render(&mut frame, 2, height, 2 * 4);
        assert_eq!(tiled(&frame, 2 * 4, 0, GAP + TITLE_HEIGHT), [1, 2, 3, 0]);
        assert_eq!(tiled(&frame, 2 * 4, 1, GAP + TITLE_HEIGHT), [4, 5, 6, 0]);
        assert!(!frame.as_chunks::<4>().0.contains(&[7, 8, 9, 0]));
    }

    #[test]
    fn transient_unmap_remembers_workspace_while_remove_forgets_it() {
        let mut scene = Scene::new();
        let key = SurfaceKey {
            client: 1,
            object: 4,
        };
        scene.commit(key, surface([1, 2, 3, 0], 1, 1)).unwrap();
        scene.command(Command::MoveToWorkspace(2));
        assert!(scene.unmap(key));
        assert!(!scene.unmap(key));
        scene.commit(key, surface([1, 2, 3, 0], 1, 1)).unwrap();
        assert!(scene.layout.placements(100, 100, 0, 0).is_empty());
        assert!(scene.remove(key));
        assert!(!scene.remove(key));
        scene.commit(key, surface([1, 2, 3, 0], 1, 1)).unwrap();
        assert_eq!(scene.layout.placements(100, 100, 0, 0).len(), 1);
        assert!(scene.layout.check_invariants().is_ok());
    }

    /// `H[1, V[2, 3]]` — a window beside a column of two — on an output tall
    /// enough that a client area is taller than its own band, which is what
    /// keeps a drop onto a tile distinguishable from one onto a band.
    fn a_window_beside_a_column() -> Scene {
        let mut scene = Scene::new();
        for object in 1..=2 {
            scene
                .commit(
                    SurfaceKey { client: 1, object },
                    surface([1, 1, 1, 0], 8, 8),
                )
                .unwrap();
        }
        scene
            .commit(
                SurfaceKey {
                    client: 1,
                    object: 3,
                },
                surface([1, 1, 1, 0], 8, 8),
            )
            .unwrap();
        below(&mut scene, 3);
        scene
    }

    /// `H[1, 2, 3, 4]` — a row of four, which is what it takes for the
    /// neighbour that grows into a dragged window's place to lie almost
    /// entirely underneath it.
    fn a_row_of_four() -> Scene {
        let mut scene = Scene::new();
        for object in 1..=4 {
            scene
                .commit(
                    SurfaceKey { client: 1, object },
                    surface([1, 1, 1, 0], 8, 8),
                )
                .unwrap();
        }
        scene
    }

    fn tile_order(scene: &Scene, width: usize, height: usize) -> Vec<u32> {
        scene
            .tiled_placements(width, height)
            .iter()
            .map(|placement| placement.key.object)
            .collect()
    }

    /// Put the window just committed BELOW the one that was focused before it,
    /// splitting that window's tile rather than joining its container — the
    /// drop an operator makes on a bottom edge. `SetSplit(Axis::Vertical)`
    /// before a commit used to do this, and there is no such mode now: a new
    /// window joins whatever container it opens in.
    fn below(scene: &mut Scene, object: u32) {
        let moved = SurfaceKey { client: 1, object };
        let over = SurfaceKey {
            client: 1,
            object: object.saturating_sub(1),
        };
        assert!(
            scene.layout.drop_onto(
                moved,
                over,
                DropKind::Beside {
                    axis: Axis::Vertical,
                    before: false,
                },
            ),
            "the drop that opens a window below another moved nothing"
        );
    }

    fn tile(scene: &Scene, width: usize, height: usize, object: u32) -> Placement {
        let placements = scene.tiled_placements(width, height);
        let at = placements
            .iter()
            .position(|placement| placement.key.object == object)
            .unwrap();
        *placements.get(at).unwrap()
    }

    fn painted(scene: &Scene, width: usize, height: usize) -> Vec<u8> {
        let mut frame = vec![0u8; width.saturating_mul(height).saturating_mul(4)];
        scene.render(&mut frame, width, height, width.saturating_mul(4));
        frame
    }

    #[test]
    fn a_drag_draws_a_block_and_moves_nothing_until_the_release() {
        // The whole of the new drag, asserted where the operator reads it: the
        // FRAME. Aiming paints a block over the arrangement and changes
        // NOTHING underneath — not the tiles, not the map the clients are
        // configured for — and the release is what moves the window.
        //
        // The previous drag re-flowed the arrangement live and committed
        // whatever was drawn. That is what this replaces: an operator aiming
        // at a tile could push it out from under the pointer on the way.
        let mut scene = a_window_beside_a_column();
        let (width, height) = (240, 600);
        let dragged = SurfaceKey {
            client: 1,
            object: 1,
        };
        assert!(
            tile(&scene, width, height, 3).rect.height > tile(&scene, width, height, 3).band.height,
            "the output is too short to tell a tile from its band"
        );
        let undragged = painted(&scene, width, height);
        let undragged_views = scene.views(width, height);
        let undragged_order = tile_order(&scene, width, height);

        // The bottom half of 3, which in a COLUMN means below it.
        let target = tile(&scene, width, height, 3).rect;
        scene.pointer_x = i32::try_from(target.x + 2).unwrap();
        scene.pointer_y = i32::try_from(target.y + target.height - 2).unwrap();
        assert!(scene.aim_drop(dragged, width, height), "no block went up");
        let aiming = painted(&scene, width, height);
        assert_ne!(aiming, undragged, "the block never reached the screen");
        assert_eq!(
            tile_order(&scene, width, height),
            undragged_order,
            "aiming moved a window"
        );
        assert_eq!(
            scene.views(width, height),
            undragged_views,
            "aiming told the clients something"
        );

        // The release, which is the first thing here that moves anything.
        assert_eq!(scene.commit_drop(dragged), Some(true));
        assert_eq!(tile_order(&scene, width, height), [2, 3, 1]);
        assert_ne!(
            scene.views(width, height),
            undragged_views,
            "the drop never reached the clients"
        );
        // And the block is down: what is drawn now is the arrangement itself.
        assert!(!scene.hint_is_live());
        assert_ne!(painted(&scene, width, height), aiming);
        scene.layout.check_invariants().unwrap();
    }

    #[test]
    fn a_window_trades_with_the_neighbour_under_the_pointer() {
        // The middle ninth of a tile is the TRADE zone, and with the picture
        // static it is simply the tile the operator can see. This used to be
        // the hard case: the aim was computed with the dragged window taken
        // out, so the neighbour that grew into its place lay under it and a
        // dead zone over the dragged window's own tile swallowed most of the
        // neighbour's trade zone — measured across a 1600-wide output, all of
        // it from four windows on. Neither the aim geometry nor the dead zone
        // exists now.
        let mut scene = a_row_of_four();
        let (width, height) = (1600, 600);
        let dragged = SurfaceKey {
            client: 1,
            object: 1,
        };
        let onto = tile(&scene, width, height, 2).rect;
        scene.pointer_x = i32::try_from(onto.x + onto.width / 2).unwrap();
        scene.pointer_y = i32::try_from(onto.y + onto.height / 2).unwrap();
        assert!(scene.aim_drop(dragged, width, height), "no block went up");
        assert_eq!(
            scene.hint_area(),
            Some(frame_rect(&tile(&scene, width, height, 2))),
            "a trade promises the whole tile it trades with"
        );
        assert_eq!(scene.commit_drop(dragged), Some(true));
        assert_eq!(tile_order(&scene, width, height), [2, 1, 3, 4]);
        scene.layout.check_invariants().unwrap();
    }

    #[test]
    fn a_drop_into_a_stack_marks_the_band_it_would_go_in_at() {
        // A stack's container is a LIST, so `insert_beside` refuses the axis
        // inside one and every drop but a swap becomes a plain insert into the
        // run. The block has to say the same thing the tree does: the new band
        // appears in the RUN, not in the content rectangle every leaf shares.
        //
        // Drawn on the content rect it would be doubly wrong — a `Beside`
        // promising half an area the window is not going to take, and an
        // `InRun` bar marking an edge of a rectangle nowhere near the band the
        // operator is pointing at. Measured on the stack below: aiming at the
        // top of leaf 3's band put the bar at y=128 rather than y=88, and
        // aiming at the bottom of it put the bar at the foot of the content
        // rectangle, most of the output away.
        let mut scene = a_row_of_four();
        let (width, height) = (1600, 600);
        scene.command(Command::ToggleGrouped);
        let dragged = SurfaceKey {
            client: 1,
            object: 1,
        };
        // Leaf 3's band, which is a stacked-AWAY one: it is not the leaf
        // showing the content, so the two rectangles cannot be confused.
        let onto = tile(&scene, width, height, 3);
        assert!(onto.run.is_some(), "the column did not stack");
        assert!(
            onto.band.y + onto.band.height <= onto.rect.y,
            "the band and the content rect overlap"
        );
        for (dy, before) in [(1, true), (onto.band.height - 1, false)] {
            scene.pointer_x = i32::try_from(onto.band.x + onto.band.width / 2).unwrap();
            scene.pointer_y = i32::try_from(onto.band.y + dy).unwrap();
            assert!(scene.aim_drop(dragged, width, height), "no block went up");
            assert_eq!(
                scene.hint_area(),
                Some(hint_bar(onto.band, Axis::Vertical, before)),
                "the block for a drop into a stack is not on its band"
            );
            // The bar is INSIDE the band it was aimed at, which is the
            // property an operator reads and the one the content rect fails.
            let block = scene.hint_area().unwrap();
            assert!(
                block.y >= onto.band.y && block.y + block.height <= onto.band.y + onto.band.height,
                "the block left the band"
            );
        }

        // And the swap keeps the content rect, since it really does put the
        // dragged window in that slot. Read off the leaf the stack is SHOWING,
        // the only one whose content rect answers the pointer.
        let shown = scene
            .tiled_placements(width, height)
            .iter()
            .position(|placement| placement.visible)
            .and_then(|at| scene.tiled_placements(width, height).get(at).copied())
            .unwrap();
        scene.pointer_x = i32::try_from(shown.rect.x + shown.rect.width / 2).unwrap();
        scene.pointer_y = i32::try_from(shown.rect.y + shown.rect.height / 2).unwrap();
        assert!(scene.aim_drop(dragged, width, height));
        assert_eq!(
            scene.hint_area(),
            Some(shown.rect),
            "a trade with a stacked leaf promises the content it would take"
        );
        scene.layout.check_invariants().unwrap();
    }

    #[test]
    fn the_block_is_two_parts_screen_to_one_part_blue_inside_its_own_rect() {
        // The block's PIXELS, which nothing else looks at: every other test
        // asks only whether the frame changed, so a swapped channel order, a
        // wrong ratio, or an off-by-one at the rect's edge would ship green.
        // Expected values are computed from the frame underneath rather than
        // written down, so this stays a statement about the blend rather than
        // about what happens to be behind it.
        let mut scene = a_row_of_four();
        let (width, height) = (1600, 600);
        let stride: usize = width * 4;
        let dragged = SurfaceKey {
            client: 1,
            object: 1,
        };
        let onto = tile(&scene, width, height, 3);
        scene.pointer_x = i32::try_from(onto.rect.x + onto.rect.width / 2).unwrap();
        scene.pointer_y = i32::try_from(onto.rect.y + onto.rect.height / 2).unwrap();
        let before = painted(&scene, width, height);
        assert!(scene.aim_drop(dragged, width, height), "no block went up");
        let area = scene.hint_area().expect("no block to read");
        let after = painted(&scene, width, height);

        let blend = |was: [u8; 4]| {
            let mut want = was;
            for (channel, tint) in want.iter_mut().zip([0xf0u16, 0x90, 0x30].iter()) {
                *channel =
                    u8::try_from((u16::from(*channel).saturating_mul(2).saturating_add(*tint)) / 3)
                        .unwrap();
            }
            want
        };
        // Inside, and at both extreme corners of the rect rather than only in
        // the middle: an off-by-one shows at the edge and nowhere else.
        for (x, y) in [
            (area.x + area.width / 2, area.y + area.height / 2),
            (area.x, area.y),
            (area.x + area.width - 1, area.y + area.height - 1),
        ] {
            assert_eq!(
                pixel(&after, stride, x, y),
                blend(pixel(&before, stride, x, y)),
                "the block did not blend at {x},{y}"
            );
        }
        // And just outside each of those corners, which is what says the rect
        // is the rect rather than one pixel bigger.
        for (x, y) in [
            (area.x.saturating_sub(1), area.y),
            (area.x, area.y.saturating_sub(1)),
            (area.x + area.width, area.y),
            (area.x, area.y + area.height),
        ] {
            assert_eq!(
                pixel(&after, stride, x, y),
                pixel(&before, stride, x, y),
                "the block leaked past its rect at {x},{y}"
            );
        }
    }

    #[test]
    fn a_drop_along_the_target_s_own_axis_marks_the_edge_rather_than_a_half() {
        // `insert_beside` splits only where the asked-for axis DIFFERS from
        // the target's own container: dropping "to the right of" a window in
        // a ROW is a plain insert into that row, so the dragged window takes
        // a whole slot and every sibling shrinks. It never occupies the half
        // the pointer was over, so promising that half is a picture the
        // release cannot keep.
        let mut scene = a_row_of_four();
        let (width, height) = (1600, 600);
        let dragged = SurfaceKey {
            client: 1,
            object: 1,
        };
        let onto = tile(&scene, width, height, 3);
        let frame = frame_rect(&onto);

        // The right edge of 3, along the row's OWN axis: an insert, and the
        // bar marks the side it goes in at.
        scene.pointer_x = i32::try_from(frame.x + frame.width - 2).unwrap();
        scene.pointer_y = i32::try_from(frame.y + frame.height / 2).unwrap();
        assert!(scene.aim_drop(dragged, width, height), "no block went up");
        assert_eq!(
            scene.hint_area(),
            Some(hint_bar(frame, Axis::Horizontal, false)),
            "a same-axis drop promised a half it cannot take"
        );

        // The CONTROL, and what keeps this a statement about the AXIS rather
        // than about edges: the top third of the same tile asks for a Vertical
        // split inside a Horizontal row, which really does halve that tile.
        // Read off the CLIENT area, since the band above it has two zones
        // rather than five and would answer `InRun` whatever the axis.
        scene.pointer_x = i32::try_from(onto.rect.x + onto.rect.width / 2).unwrap();
        scene.pointer_y = i32::try_from(onto.rect.y + onto.rect.height / 6).unwrap();
        assert!(scene.aim_drop(dragged, width, height));
        assert_eq!(
            scene.hint_area(),
            Some(hint_half(frame, Axis::Vertical, true)),
            "a cross-axis drop promised a bar where it splits the tile"
        );

        // And the tree agrees with the first of those: the row gains a member
        // rather than 3 gaining a sibling inside its own slot.
        scene.pointer_x = i32::try_from(frame.x + frame.width - 2).unwrap();
        scene.pointer_y = i32::try_from(frame.y + frame.height / 2).unwrap();
        assert!(scene.aim_drop(dragged, width, height));
        assert!(scene.commit_drop(dragged).is_some_and(|moved| moved));
        assert_eq!(tile_order(&scene, width, height), [2, 3, 1, 4]);
        assert_eq!(
            scene.layout.parent_axis(dragged),
            Some(Axis::Horizontal),
            "the drop made a container instead of joining the row"
        );
        scene.layout.check_invariants().unwrap();
    }

    #[test]
    fn an_aim_held_still_keeps_answering_the_same_block() {
        // The arrangement does not move while a drag is in flight, so a
        // pointer that has not moved cannot be over a different tile on the
        // next frame. That used to take a second geometry to promise: the
        // picture re-flowed around the drop, so aiming at it would push the
        // target away and the answer could alternate between two tiles with
        // the mouse held still. Now it is a property of doing nothing.
        let mut scene = a_window_beside_a_column();
        let (width, height) = (240, 600);
        let dragged = SurfaceKey {
            client: 1,
            object: 1,
        };
        let settled = tile_order(&scene, width, height);
        let target = tile(&scene, width, height, 3).rect;
        scene.pointer_x = i32::try_from(target.x + 2).unwrap();
        scene.pointer_y = i32::try_from(target.y + 2).unwrap();
        assert!(scene.aim_drop(dragged, width, height));
        let block = scene.hint_area();
        assert!(block.is_some(), "no block went up");
        for again in 0..4 {
            assert!(
                !scene.aim_drop(dragged, width, height),
                "the block moved on frame {again} with the pointer still"
            );
            assert_eq!(scene.hint_area(), block);
            assert_eq!(tile_order(&scene, width, height), settled);
        }
    }

    #[test]
    fn a_window_arriving_or_leaving_under_a_drag_drops_the_block() {
        // The block is derived from the arrangement, so it may not outlive a
        // change to it: a stale one would promise a landing beside a window
        // that has gone, or sit over a tile that has since moved.
        let (width, height) = (240, 600);
        let dragged = SurfaceKey {
            client: 1,
            object: 1,
        };
        let aim = |scene: &mut Scene| {
            let target = tile(scene, width, height, 3).rect;
            scene.pointer_x = i32::try_from(target.x + 2).unwrap();
            scene.pointer_y = i32::try_from(target.y + 2).unwrap();
            assert!(scene.aim_drop(dragged, width, height));
        };

        let mut scene = a_window_beside_a_column();
        aim(&mut scene);
        assert!(scene.unmap(SurfaceKey {
            client: 1,
            object: 2
        }));
        assert!(!scene.hint_is_live(), "the block outlived an unmap");

        let mut scene = a_window_beside_a_column();
        aim(&mut scene);
        scene
            .commit(
                SurfaceKey {
                    client: 2,
                    object: 1,
                },
                surface([1, 1, 1, 0], 8, 8),
            )
            .unwrap();
        assert!(!scene.hint_is_live(), "the block outlived a new window");

        let mut scene = a_window_beside_a_column();
        aim(&mut scene);
        scene.command(Command::Focus(crate::layout::Direction::Up));
        assert!(!scene.hint_is_live(), "the block outlived a command");
    }

    #[test]
    fn an_aim_leaves_the_focus_alone_and_the_drop_takes_it() {
        // Focus follows the ARRANGEMENT, and the arrangement does not move
        // while a block is up — so aiming changes nothing about what the
        // keyboard is pointed at, and a drag abandoned mid-flight leaves the
        // operator exactly where they were.
        //
        // The preview this replaces had to carry its own focus, because the
        // map published to clients was the previewed one and marking a
        // different window active would have aimed the keyboard at one window
        // while telling every client about another. Nothing is published until
        // the release now, so there is no second answer to keep in step.
        let mut scene = a_window_beside_a_column();
        let (width, height) = (240, 600);
        let dragged = SurfaceKey {
            client: 1,
            object: 1,
        };
        // 3 was mapped last and so is focused; move focus off it, or the
        // assertions below would hold by accident.
        let elsewhere = SurfaceKey {
            client: 1,
            object: 2,
        };
        assert!(scene.focus_key(elsewhere));
        assert_eq!(scene.focused(), Some(elsewhere));

        let target = tile(&scene, width, height, 2).rect;
        scene.pointer_x = i32::try_from(target.x + 2).unwrap();
        scene.pointer_y = i32::try_from(target.y + 2).unwrap();
        assert!(scene.aim_drop(dragged, width, height));
        assert_eq!(
            scene.focused(),
            Some(elsewhere),
            "aiming moved the keyboard"
        );
        // Abandoned: the block goes and nothing else moved.
        assert!(scene.clear_hint());
        assert_eq!(scene.focused(), Some(elsewhere));

        // Taken: the drop focuses what it moved, as every other way of moving
        // a window does.
        assert!(scene.aim_drop(dragged, width, height));
        assert_eq!(scene.commit_drop(dragged), Some(true));
        assert_eq!(scene.focused(), Some(dragged));
    }

    #[test]
    fn dropping_a_block_is_not_a_layout_change() {
        // These answer whether the LAYOUT moved, which is what gates the round
        // of configures their one caller sends; it repaints either way. A
        // block is drawn over the arrangement rather than replacing it, so
        // taking one down owes that repaint and nothing else — reporting it as
        // a layout change would reconfigure every client for a rectangle none
        // of them was ever told about.
        let (width, height) = (240, 600);
        let dragged = SurfaceKey {
            client: 1,
            object: 1,
        };
        let stranger = SurfaceKey {
            client: 9,
            object: 9,
        };
        let aim = |scene: &mut Scene| {
            let target = tile(scene, width, height, 3).rect;
            scene.pointer_x = i32::try_from(target.x + 2).unwrap();
            scene.pointer_y = i32::try_from(target.y + 2).unwrap();
            assert!(scene.aim_drop(dragged, width, height));
        };

        // A surface that is not in the layout at all, so the mutation itself
        // moves nothing and the block is the only thing that changes.
        for mutate in [
            (|scene: &mut Scene, key: SurfaceKey| scene.unmap(key))
                as fn(&mut Scene, SurfaceKey) -> bool,
            |scene: &mut Scene, key: SurfaceKey| scene.remove(key),
            |scene: &mut Scene, key: SurfaceKey| scene.remove_client(key.client),
        ] {
            let mut scene = a_window_beside_a_column();
            assert!(
                !mutate(&mut scene, stranger),
                "the stranger was in the layout"
            );
            aim(&mut scene);
            assert!(
                !mutate(&mut scene, stranger),
                "a dropped block asked for configures"
            );
            assert!(!scene.hint_is_live(), "the block outlived its base");
        }
    }

    #[test]
    fn argb_channels_are_premultiplied_before_they_reach_the_compositor() {
        let mut frame = vec![100, 100, 100, 0];
        blend_pixel(&mut frame, 1, 1, 4, 0, 0, [50, 25, 0, 128]);
        assert_eq!(frame, [99, 74, 49, 0]);
    }
}
