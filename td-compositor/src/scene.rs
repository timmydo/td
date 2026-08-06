use crate::bar::{self, BAR_HEIGHT};
use crate::help::Help;
use crate::launcher::{LaunchRequest, Launcher, LauncherAction};
use crate::layout::{Command, Layout, Placement, Rect, ViewLayout};
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
/// It assumes the two TOUCH and share a left edge and a width, which every
/// arrangement here produces and
/// `a_title_band_tops_every_tile_partitions_it_and_swallows_its_own_clicks`
/// asserts against the tile the split actually produced. A stacked container
/// breaks BOTH halves — its focused child's band sits up in the run of bands
/// while the content is below all of them, and the run spans the container
/// rather than the child — so the branch for it lands with the arrangement
/// that needs it rather than as an untaken arm nothing can reach today. That
/// test only walks the arrangements it BUILDS, so it fails loudly on a
/// stacked one only once it builds one.
fn frame_rect(placement: &Placement) -> Rect {
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
        layout_changed
    }

    pub fn remove(&mut self, key: SurfaceKey) -> bool {
        let layout_changed = self.layout.contains(key);
        self.discard_pixels(key);
        self.titles.remove(&key);
        self.layout.forget(key);
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
        layout_changed
    }

    pub fn command(&mut self, command: Command) {
        self.layout.apply(command);
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

    fn tiled_placements(&self, width: usize, height: usize) -> Vec<Placement> {
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

    pub fn move_pointer(&mut self, dx: i32, dy: i32, width: usize, height: usize) {
        let max_x = i32::try_from(width.saturating_sub(1)).unwrap_or(i32::MAX);
        let max_y = i32::try_from(height.saturating_sub(1)).unwrap_or(i32::MAX);
        self.pointer_x = self.pointer_x.saturating_add(dx).clamp(0, max_x);
        self.pointer_y = self.pointer_y.saturating_add(dy).clamp(0, max_y);
    }

    #[cfg(test)]
    pub fn pointer_target(&self, width: usize, height: usize) -> Option<SurfacePoint> {
        let placements = self.tiled_placements(width, height);
        self.pointer_target_from(&placements)
    }

    fn pointer_target_from(&self, placements: &[Placement]) -> Option<SurfacePoint> {
        let x = usize::try_from(self.pointer_x).ok()?;
        let y = usize::try_from(self.pointer_y).ok()?;
        for placement in placements {
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
            .filter(|placement| placement.key == key)
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
        for placement in &placements {
            // The FRAME, not the client area: a tile too short to hold both is
            // all band and no client, and guarding on the client alone would
            // drop the one thing left to draw for it.
            let outline = frame_rect(placement);
            if outline.width == 0 || outline.height == 0 {
                continue;
            }
            if !self.surfaces.contains_key(&placement.key) {
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
            self.draw_title(frame, width, height, stride, placement);
        }
        for placement in placements {
            // The CLIENT area here, where the border pass above wants the
            // frame: a tile too short to hold a band has no client pixels to
            // draw, and `draw_surface` was already a no-op for one.
            if placement.rect.width == 0 || placement.rect.height == 0 {
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
        assert_eq!(scene.title(key), Some("FIREFOX - A PAGE"), "remapped nameless");

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
                .commit(SurfaceKey { client: 1, object }, surface([1, 2, 3, 0], 8, 8))
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
                .commit(SurfaceKey { client: 1, object }, surface([1, 2, 3, 0], 8, 8))
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
        scene.move_pointer(-9, -4, 10, 8);
        assert_eq!((scene.pointer_x, scene.pointer_y), (0, 0));
        scene.move_pointer(i32::MAX, i32::MAX, 10, 8);
        assert_eq!((scene.pointer_x, scene.pointer_y), (9, 7));
        scene.move_pointer(1, 1, 0, 0);
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
        let view = scene.views(100, least_output_height(8)).first().copied().unwrap();
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
        assert_eq!(scene.pointer_target_for(key, 100, least_output_height(8)), None);
    }

    #[test]
    fn input_regions_include_additions_exclude_holes_and_reset_to_infinite() {
        let mut scene = Scene::new();
        let key = SurfaceKey {
            client: 4,
            object: 9,
        };
        scene.commit(key, surface([1, 2, 3, 0], 10, 8)).unwrap();
        let view = scene.views(100, least_output_height(8)).first().copied().unwrap();
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

    #[test]
    fn argb_channels_are_premultiplied_before_they_reach_the_compositor() {
        let mut frame = vec![100, 100, 100, 0];
        blend_pixel(&mut frame, 1, 1, 4, 0, 0, [50, 25, 0, 128]);
        assert_eq!(frame, [99, 74, 49, 0]);
    }
}
