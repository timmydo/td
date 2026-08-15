use crate::bar::{self, BAR_HEIGHT};
use crate::help::Help;
use crate::launcher::{LaunchRequest, Launcher, LauncherAction};
use crate::layout::{Axis, Command, DropKind, Layout, Placement, Rect, ViewLayout};
use crate::ui;
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
    if placement.stacked {
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
    preview: Option<Layout>,
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
            preview: None,
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
            self.preview = None;
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
        // Dropping a drag's preview moves the screen even when the unmap
        // itself did not — an already-unmapped surface detaching is the case
        // — and the map published to clients was the previewed one, so it
        // counts as a layout change or those two are left disagreeing.
        self.clear_preview() || layout_changed
    }

    pub fn remove(&mut self, key: SurfaceKey) -> bool {
        let layout_changed = self.layout.contains(key);
        self.discard_pixels(key);
        self.titles.remove(&key);
        self.layout.forget(key);
        self.clear_preview() || layout_changed
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
        self.clear_preview() || layout_changed
    }

    pub fn command(&mut self, command: Command) {
        self.layout.apply(command);
        self.preview = None;
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
        let mut views =
            self.arrangement()
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

    pub(crate) fn tiled_placements(&self, width: usize, height: usize) -> Vec<Placement> {
        self.placements_of(self.arrangement(), width, height)
    }

    /// The arrangement to DRAW and to report: a drag's preview while one is
    /// up, the layout itself otherwise. Every consumer of tiling geometry
    /// reads this, for the reason `tiled_height` gives — two of them
    /// disagreeing mid-drag is a click landing on a different tile than the
    /// one under the cursor.
    fn arrangement(&self) -> &Layout {
        self.preview.as_ref().unwrap_or(&self.layout)
    }

    /// The same geometry for an arbitrary arrangement rather than the one on
    /// screen. A drag needs both at once: what is DRAWN is the result of the
    /// drop, and what the drop is aimed at is computed against the arrangement
    /// with the dragged window taken out.
    fn placements_of(&self, layout: &Layout, width: usize, height: usize) -> Vec<Placement> {
        let mut placements = layout.placements(width, self.tiled_height(height), GAP, TITLE_HEIGHT);
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

    /// Read off the ARRANGEMENT rather than the layout, as the geometry is:
    /// a preview carries its own focus — the drop focuses what it moved — and
    /// the map published to clients marks that window active. Answering from
    /// the layout instead would aim the keyboard at one window while telling
    /// every client another was activated.
    pub fn focused(&self) -> Option<SurfaceKey> {
        self.arrangement().focused()
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

    #[cfg(test)]
    pub fn pointer_target(&self, width: usize, height: usize) -> Option<SurfacePoint> {
        let placements = self.tiled_placements(width, height);
        self.pointer_target_from(&placements)
    }

    #[cfg(test)]
    pub(crate) fn pointer_at(&self) -> (i32, i32) {
        (self.pointer_x, self.pointer_y)
    }

    #[cfg(test)]
    pub(crate) fn layout(&self) -> &Layout {
        &self.layout
    }

    /// Draw the drop a release would perform, rather than an outline of it.
    ///
    /// TWO arrangements are in play and they are deliberately different. What
    /// is DRAWN is the result — the layout with the dragged window moved to
    /// where the pointer says. What the pointer is measured AGAINST is the
    /// layout with that window taken OUT, which does not re-flow as the
    /// preview does: aiming at the picture would let a tile be pushed away by
    /// the very motion aiming at it, and a target that moves when approached
    /// can oscillate between two answers over one pixel.
    ///
    /// The window that was picked up is a DEAD ZONE, or a press alone would
    /// move it. Answers whether the screen would differ, so a pointer moving
    /// inside one half costs no repaint.
    pub fn preview_drop(&mut self, dragged: SurfaceKey, width: usize, height: usize) -> bool {
        let mut preview = self.layout.clone();
        // Read out of `layout` rather than the preview, so the dead zone is
        // fixed for the whole gesture.
        if !self.pointer_on_dragged(dragged, width, height) {
            let base = self.aim_layout(dragged);
            if let Some((target, drop)) = self.drop_target_in(&base, width, height) {
                preview.drop_onto(dragged, target, drop);
            }
        }
        let changed = self.arrangement() != &preview;
        self.preview = (preview != self.layout).then_some(preview);
        changed
    }

    /// Whether a preview is up, which is the same question as the arrangement
    /// differing from the layout: one is only ever held while it does.
    #[cfg(test)]
    pub fn preview_is_live(&self) -> bool {
        self.preview.is_some()
    }

    /// Take the preview as the arrangement itself. The drop is applied by
    /// KEEPING what was drawn rather than by computing it a second time,
    /// which is the whole of what the preview promises: there is no other
    /// answer for the release to disagree with.
    pub fn commit_preview(&mut self) -> bool {
        // A preview is only ever held while it DIFFERS from the layout, so
        // holding one and changing the arrangement are the same question.
        let Some(preview) = self.preview.take() else {
            return false;
        };
        self.layout = preview;
        true
    }

    /// Drop the preview and go back to the arrangement itself. Answers
    /// whether the screen moves, by the same invariant `commit_preview` reads.
    pub fn clear_preview(&mut self) -> bool {
        self.preview.take().is_some()
    }

    fn pointer_on_dragged(&self, key: SurfaceKey, width: usize, height: usize) -> bool {
        let placements = self.placements_of(&self.layout, width, height);
        self.pointer_at_usize()
            .and_then(|(x, y)| placements.get(tile_at(&placements, x, y)?))
            .is_some_and(|placement| placement.key == key)
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

    /// The window whose TITLE BAND the pointer is over, which is the handle a
    /// bare drag takes. A band reaches no client — the hit test above knows
    /// only client areas — so this is a question only the compositor answers,
    /// and the seam that makes a band draggable without a client seeing it.
    pub fn band_at_pointer(&self, width: usize, height: usize) -> Option<SurfaceKey> {
        let placements = self.tiled_placements(width, height);
        let x = usize::try_from(self.pointer_x).ok()?;
        let y = usize::try_from(self.pointer_y).ok()?;
        let index = placements
            .iter()
            .position(|placement| contains(placement.band, x, y))?;
        Some(placements.get(index)?.key)
    }

    /// Where a drop lands: the window under the pointer, and what landing
    /// there means.
    #[cfg(test)]
    fn drop_target(&self, width: usize, height: usize) -> Option<(SurfaceKey, DropKind)> {
        self.drop_target_in(self.arrangement(), width, height)
    }

    /// The arrangement a drop is AIMED at: the layout with the dragged window
    /// taken OUT. One derivation for the drag and for the tests that pick
    /// points out of it, or the geometry a test aims into drifts from the
    /// geometry the drag measures and neither says so.
    fn aim_layout(&self, dragged: SurfaceKey) -> Layout {
        let mut base = self.layout.clone();
        base.unmap(dragged);
        base
    }

    /// The geometry a drag's drop is AIMED at, which is not the geometry on
    /// screen: the dragged window is taken out of it, so its tiles are bigger
    /// and in different places than the ones being drawn. A test that picks
    /// an aim point off the screen is asking a different question than the
    /// drag does, and got away with it only while a tile had two zones.
    #[cfg(test)]
    pub(crate) fn aim_placements(
        &self,
        dragged: SurfaceKey,
        width: usize,
        height: usize,
    ) -> Vec<Placement> {
        self.placements_of(&self.aim_layout(dragged), width, height)
    }

    /// Where a drop aimed at `layout` would land. A drag passes the
    /// arrangement with the dragged window REMOVED, which is constant for the
    /// whole gesture: aiming at what is on screen instead would let the
    /// preview re-flow the very tile the pointer is over, so moving toward a
    /// slot could push that slot away.
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
    ///
    /// `layout` is only HALF the input, which the band branch below makes
    /// unavoidable: the geometry comes from the argument and the run's
    /// direction from `self.layout`, because the two questions are asked of
    /// different trees on purpose. So this is not a pure function of what it
    /// is passed, and passing an arrangement that is not the aim one — the
    /// live PREVIEW, say — mixes two answers rather than asking a third.
    pub fn drop_target_in(
        &self,
        layout: &Layout,
        width: usize,
        height: usize,
    ) -> Option<(SurfaceKey, DropKind)> {
        let placements = self.placements_of(layout, width, height);
        let (x, y) = self.pointer_at_usize()?;
        let placement = placements.get(tile_at(&placements, x, y)?)?;
        if contains(placement.band, x, y) {
            let band = placement.band;
            // Asked of the tree the drop will be APPLIED to rather than of
            // `layout`, which is the aim geometry with the dragged window
            // taken out: a container of two collapses there and answers
            // nothing, and a stack that collapses answers as its own axis
            // rather than as a run. Both would read the half along the wrong
            // side of the band.
            let before = match self.layout.run_direction(placement.key) {
                Some(Axis::Horizontal) => x < band.x.saturating_add(band.width / 2),
                Some(Axis::Vertical) | None => y < band.y.saturating_add(band.height / 2),
            };
            return Some((placement.key, DropKind::InRun { before }));
        }
        Some((placement.key, zone_of(placement.rect, x, y)))
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
        let Some(title) = self.titles.get(&placement.key) else {
            return;
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
            rect,
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
        bar::paint(frame, width, height, stride, &self.status);
        self.launcher.paint(frame, width, height, stride);
        self.help.paint(frame, width, height, stride);
        draw_pointer(frame, width, height, stride, self.pointer_x, self.pointer_y);
    }
}

fn contains(rect: Rect, x: usize, y: usize) -> bool {
    x >= rect.x
        && y >= rect.y
        && x < rect.x.saturating_add(rect.width)
        && y < rect.y.saturating_add(rect.height)
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
        scene.command(Command::SetSplit(crate::layout::Axis::Horizontal));
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
        scene.command(Command::SetSplit(crate::layout::Axis::Vertical));
        scene
            .commit(
                SurfaceKey {
                    client: 1,
                    object: 4,
                },
                surface([4, 5, 6, 0], 8, 8),
            )
            .unwrap();
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
    fn a_bands_half_is_read_where_the_drop_lands_not_where_it_aims() {
        // The two geometries a drag has in play disagree about more than
        // pixels. A ROW of two: take one out and the row is gone, so the aim
        // tree says the target is in no container at all and its band would
        // be halved top-to-bottom — down a strip twenty pixels tall, where
        // one side of the row can never be asked for. The tree the drop
        // LANDS in still has the row, and that is the one to ask.
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
        let mut base = scene.layout.clone();
        base.unmap(key(1));
        assert_eq!(base.run_direction(key(2)), None, "the row survived");

        // The band as the AIM geometry has it, which is where the pointer is
        // hit-tested — the whole width, not the right half it is drawn as.
        let band = {
            let placements = scene.placements_of(&base, width, height);
            let at = placements
                .iter()
                .position(|placement| placement.key == key(2))
                .unwrap();
            placements.get(at).unwrap().band
        };

        // Left of the band and BELOW its middle, so the two readings give
        // opposite answers and only one of them can be right.
        //
        // The LEFT point is asked of this function alone: in a real gesture
        // it lies inside the dragged window's own tile on screen, which the
        // dead zone refuses. That is the cost recorded in DESIGN.md — the
        // half "before this neighbour" is unreachable — and it is why the
        // half has to be pinned here rather than through a whole drag.
        for (x, before) in [
            (band.x + 1, true),
            (band.x + band.width.saturating_sub(2), false),
        ] {
            scene.pointer_x = i32::try_from(x).unwrap();
            scene.pointer_y = i32::try_from(band.y + band.height - 1).unwrap();
            assert_eq!(
                scene.drop_target_in(&base, width, height),
                Some((key(2), DropKind::InRun { before })),
                "band at x={x}"
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
        scene.command(Command::SetSplit(crate::layout::Axis::Vertical));
        scene
            .commit(
                SurfaceKey {
                    client: 1,
                    object: 3,
                },
                surface([1, 1, 1, 0], 8, 8),
            )
            .unwrap();
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
        scene.command(Command::ToggleStacked);
        let stacked = scene.tiled_placements(width, height);
        assert!(stacked.iter().all(|placement| placement.stacked));
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
        scene.command(Command::Focus(crate::layout::Direction::Right));
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
        assert_eq!(scene.band_at_pointer(width, height), None);
    }

    #[test]
    fn a_stacked_column_draws_every_band_but_only_the_focused_client() {
        let mut scene = Scene::new();
        scene.command(Command::SetSplit(crate::layout::Axis::Vertical));
        let colors = [[1, 2, 3, 0], [4, 5, 6, 0], [7, 8, 9, 0]];
        for (index, color) in colors.iter().enumerate() {
            let object = u32::try_from(index).unwrap().saturating_add(1);
            scene
                .commit(SurfaceKey { client: 1, object }, surface(*color, 400, 400))
                .unwrap();
        }
        let (width, height) = (320, least_output_height(200));
        let stride = width * 4;
        let mut frame = vec![0u8; stride * height];
        scene.pointer_x = 0;
        scene.pointer_y = i32::try_from(height - 1).unwrap();

        scene.command(Command::ToggleStacked);

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
                for x in [0, width / 2, width - 1] {
                    assert_eq!(
                        pixel(&frame, stride, x, y),
                        [0x18, 0x14, 0x20, 0],
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
        scene.command(Command::SetSplit(crate::layout::Axis::Vertical));
        scene
            .commit(
                SurfaceKey {
                    client: 1,
                    object: 3,
                },
                surface([1, 1, 1, 0], 8, 8),
            )
            .unwrap();
        scene
    }

    fn tile_order(scene: &Scene, width: usize, height: usize) -> Vec<u32> {
        scene
            .tiled_placements(width, height)
            .iter()
            .map(|placement| placement.key.object)
            .collect()
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
    fn a_drag_preview_is_the_pixels_the_release_leaves() {
        // The whole of what the preview promises, asserted where the operator
        // reads it: the FRAME. Dropping is not "apply what was computed" but
        // "keep what was drawn", so the release cannot move a pixel — and the
        // clients are owed nothing further either, since the map published to
        // them was the previewed one all along.
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

        // The bottom half of 3, which in a COLUMN means below it.
        let target = tile(&scene, width, height, 3).rect;
        scene.pointer_x = i32::try_from(target.x + 2).unwrap();
        scene.pointer_y = i32::try_from(target.y + target.height - 2).unwrap();
        assert!(scene.preview_drop(dragged, width, height));
        let previewed = painted(&scene, width, height);
        let previewed_views = scene.views(width, height);
        assert_ne!(
            previewed_views, undragged_views,
            "the clients were never told the window had moved"
        );
        assert_eq!(tile_order(&scene, width, height), [2, 3, 1]);
        // Without this the two frames below could agree by both being the
        // undragged one, and the test would pass having previewed nothing.
        assert_ne!(
            previewed, undragged,
            "the preview did not change the screen"
        );

        assert!(scene.commit_preview());
        assert_eq!(
            painted(&scene, width, height),
            previewed,
            "the release moved pixels the preview had promised"
        );
        assert_eq!(scene.views(width, height), previewed_views);
        assert_eq!(tile_order(&scene, width, height), [2, 3, 1]);
        // And the picture is now the layout's own, not an overlay on it.
        assert!(scene.preview.is_none());
        assert_eq!(tile_order(&scene, width, height), [2, 3, 1]);
        scene.layout.check_invariants().unwrap();
    }

    #[test]
    fn the_window_a_drag_was_picked_up_by_previews_nothing() {
        // A click on a title bar has to stay a click. With the dragged window
        // taken out of the arrangement the target is measured against, the
        // pixel under a press belongs to whichever neighbour grew into it, so
        // without the dead zone a press ALONE would move the window.
        //
        // The zone is the whole window rather than its band, which is what an
        // Alt press needs — that one lands anywhere on a window, so a band-
        // sized zone would leave a press on a client area moving it. It also
        // reads better for the band: dragging DOWN into the window's own body
        // used to re-parent it beside whichever neighbour had grown into that
        // space, which is a jump nobody asked for.
        let mut scene = a_window_beside_a_column();
        let (width, height) = (240, 600);
        let dragged = SurfaceKey {
            client: 1,
            object: 1,
        };
        let handle = tile(&scene, width, height, 1).band;
        let body = tile(&scene, width, height, 1).rect;
        let undragged = scene.tiled_placements(width, height);
        for (x, y) in [
            (handle.x, handle.y),
            (handle.x + handle.width / 2, handle.y + handle.height / 2),
            (
                handle.x + handle.width - 1,
                handle.y + handle.height.saturating_sub(1),
            ),
            (body.x + 1, body.y),
            (body.x + body.width / 2, body.y + body.height / 2),
            (
                body.x + body.width - 1,
                body.y + body.height.saturating_sub(1),
            ),
        ] {
            scene.pointer_x = i32::try_from(x).unwrap();
            scene.pointer_y = i32::try_from(y).unwrap();
            assert!(
                !scene.preview_drop(dragged, width, height),
                "the window previewed a drop at {x},{y}"
            );
            assert!(!scene.commit_preview(), "a press alone moved the window");
            assert_eq!(scene.tiled_placements(width, height), undragged);
        }

        // On a DIFFERENT window the drag is live — without which the
        // assertions above would hold for a dead zone that had swallowed the
        // whole screen. Compared as GEOMETRY rather than as an order: this
        // drop lands 1 first in the column it was beside, so the tiles come
        // out in the same sequence and only their rectangles say the
        // arrangement moved at all.
        let onto = tile(&scene, width, height, 2).rect;
        scene.pointer_x = i32::try_from(onto.x + 2).unwrap();
        scene.pointer_y = i32::try_from(onto.y + 2).unwrap();
        assert!(scene.preview_drop(dragged, width, height));
        assert_ne!(scene.tiled_placements(width, height), undragged);
    }

    #[test]
    fn a_preview_does_not_move_the_target_it_is_aimed_at() {
        // Why the target is measured against the arrangement with the dragged
        // window REMOVED rather than against the picture. The picture
        // re-flows around the drop, so a pointer that has not moved would be
        // over a different tile on the next frame — over the dragged window
        // itself, which refuses its own drop, so the preview would fall back
        // to the arrangement and the frame after that would put it back. A
        // picture alternating between two answers while the mouse is still.
        let mut scene = a_window_beside_a_column();
        let (width, height) = (240, 600);
        let dragged = SurfaceKey {
            client: 1,
            object: 1,
        };
        let target = tile(&scene, width, height, 3).rect;
        scene.pointer_x = i32::try_from(target.x + 2).unwrap();
        scene.pointer_y = i32::try_from(target.y + 2).unwrap();
        assert!(scene.preview_drop(dragged, width, height));
        assert_eq!(tile_order(&scene, width, height), [2, 1, 3]);
        for again in 0..4 {
            assert!(
                !scene.preview_drop(dragged, width, height),
                "the preview changed on frame {again} with the pointer still"
            );
            assert_eq!(tile_order(&scene, width, height), [2, 1, 3]);
        }
    }

    #[test]
    fn a_window_arriving_or_leaving_under_a_drag_drops_the_preview() {
        // The preview is derived from the arrangement, so it may not outlive
        // a change to it: a stale one would keep drawing a window that has
        // gone, or lay out around one that has arrived.
        let (width, height) = (240, 600);
        let dragged = SurfaceKey {
            client: 1,
            object: 1,
        };
        let aim = |scene: &mut Scene| {
            let target = tile(scene, width, height, 3).rect;
            scene.pointer_x = i32::try_from(target.x + 2).unwrap();
            scene.pointer_y = i32::try_from(target.y + 2).unwrap();
            assert!(scene.preview_drop(dragged, width, height));
        };

        let mut scene = a_window_beside_a_column();
        aim(&mut scene);
        assert!(scene.unmap(SurfaceKey {
            client: 1,
            object: 2
        }));
        assert!(scene.preview.is_none(), "the preview outlived an unmap");

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
        assert!(scene.preview.is_none(), "the preview outlived a new window");

        let mut scene = a_window_beside_a_column();
        aim(&mut scene);
        scene.command(Command::Focus(crate::layout::Direction::Up));
        assert!(scene.preview.is_none(), "the preview outlived a command");
    }

    #[test]
    fn a_preview_carries_the_focus_its_drop_would_leave() {
        // Focus is read off the ARRANGEMENT for the reason its geometry is.
        // A drop focuses what it moved, and the map published to clients
        // marks that window active; answering from the layout underneath
        // would aim the keyboard at one window while telling every client
        // another had been activated.
        let mut scene = a_window_beside_a_column();
        let (width, height) = (240, 600);
        let dragged = SurfaceKey {
            client: 1,
            object: 1,
        };
        // 3 was mapped last and so is focused; move focus off it, or the
        // preview's own focus and the layout's would agree by accident.
        let elsewhere = SurfaceKey {
            client: 1,
            object: 2,
        };
        assert!(scene.focus_key(elsewhere));
        assert_eq!(scene.focused(), Some(elsewhere));

        let target = tile(&scene, width, height, 2).rect;
        scene.pointer_x = i32::try_from(target.x + 2).unwrap();
        scene.pointer_y = i32::try_from(target.y + 2).unwrap();
        assert!(scene.preview_drop(dragged, width, height));
        assert_eq!(
            scene.focused(),
            Some(dragged),
            "the picture and the keyboard disagree about what is focused"
        );
        // And the layout underneath is untouched, so a cancelled drag hands
        // focus back rather than keeping the drop's.
        assert_eq!(scene.layout.focused(), Some(elsewhere));
        assert!(scene.clear_preview());
        assert_eq!(scene.focused(), Some(elsewhere));
    }

    #[test]
    fn dropping_a_preview_counts_as_a_layout_change() {
        // A mutation that changes nothing about the layout can still move the
        // screen, by dropping the preview drawn over it. Its caller repaints
        // and republishes on that answer, so reporting the layout's own
        // change alone leaves the map published to clients holding previewed
        // geometry the screen no longer shows.
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
            assert!(scene.preview_drop(dragged, width, height));
        };

        // A surface that is not in the layout at all: without the preview
        // this unmap is a no-op and says so.
        let mut scene = a_window_beside_a_column();
        assert!(!scene.unmap(stranger), "the stranger was in the layout");
        aim(&mut scene);
        assert!(scene.unmap(stranger), "a dropped preview went unreported");

        let mut scene = a_window_beside_a_column();
        assert!(!scene.remove(stranger));
        aim(&mut scene);
        assert!(scene.remove(stranger), "a dropped preview went unreported");

        let mut scene = a_window_beside_a_column();
        assert!(!scene.remove_client(9));
        aim(&mut scene);
        assert!(scene.remove_client(9), "a dropped preview went unreported");
    }

    #[test]
    fn argb_channels_are_premultiplied_before_they_reach_the_compositor() {
        let mut frame = vec![100, 100, 100, 0];
        blend_pixel(&mut frame, 1, 1, 4, 0, 0, [50, 25, 0, 128]);
        assert_eq!(frame, [99, 74, 49, 0]);
    }
}
