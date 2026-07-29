use crate::layout::{Command, Layout, Rect, ViewLayout};
use std::collections::BTreeMap;

pub const SHM_ARGB8888: u32 = 0;
pub const SHM_XRGB8888: u32 = 1;
const GAP: usize = 24;
const BORDER: usize = 4;
const MAX_SCENE_BYTES: usize = 128 * 1024 * 1024;

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

pub struct Scene {
    surfaces: BTreeMap<SurfaceKey, Surface>,
    layout: Layout,
    pointer_x: i32,
    pointer_y: i32,
    surface_bytes: usize,
}

impl Scene {
    pub fn new() -> Scene {
        Scene {
            surfaces: BTreeMap::new(),
            layout: Layout::new(),
            pointer_x: 0,
            pointer_y: 0,
            surface_bytes: 0,
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

    fn discard_pixels(&mut self, key: SurfaceKey) -> bool {
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
        self.surface_bytes = self.surface_bytes.saturating_sub(removed);
        self.layout.unmap_client(client);
        layout_changed
    }

    pub fn command(&mut self, command: Command) {
        self.layout.apply(command);
    }

    pub fn views(&self, width: usize, height: usize) -> Vec<ViewLayout> {
        self.layout.views(width, height, GAP)
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

    pub fn render(&self, frame: &mut [u8], width: usize, height: usize, stride: usize) {
        for row in frame.chunks_mut(stride) {
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

        let placements = self.layout.placements(width, height, GAP);
        for placement in &placements {
            if placement.rect.width == 0 || placement.rect.height == 0 {
                continue;
            }
            if !self.surfaces.contains_key(&placement.key) {
                continue;
            }
            let x = i64::try_from(placement.rect.x).unwrap_or(i64::MAX);
            let y = i64::try_from(placement.rect.y).unwrap_or(i64::MAX);
            draw_border(
                frame,
                width,
                height,
                stride,
                x,
                y,
                placement.rect.width,
                placement.rect.height,
                placement.focused,
            );
        }
        for placement in placements {
            if placement.rect.width == 0 || placement.rect.height == 0 {
                continue;
            }
            let Some(surface) = self.surfaces.get(&placement.key) else {
                continue;
            };
            draw_surface(frame, width, height, stride, placement.rect, surface);
        }
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
        let mut frame = vec![0xaa; 20 * 12 * 4];
        scene.render(&mut frame, 20, 12, 20 * 4);
        assert_eq!(frame.len(), 20 * 12 * 4);
        assert!(frame.as_chunks::<4>().0.contains(&[1, 2, 3, 0]));
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
    fn renderer_uses_tiling_geometry_gaps_and_focus_borders() {
        let mut scene = Scene::new();
        for (object, color) in [(1, [1, 2, 3, 0]), (2, [4, 5, 6, 0])] {
            scene
                .commit(SurfaceKey { client: 1, object }, surface(color, 100, 100))
                .unwrap();
        }
        let width = 120usize;
        let height = 80usize;
        let stride = width * 4;
        let mut frame = vec![0; stride * height];
        scene.render(&mut frame, width, height, stride);

        assert_eq!(pixel(&frame, stride, 24, 24), [1, 2, 3, 0]);
        assert_eq!(pixel(&frame, stride, 72, 24), [4, 5, 6, 0]);
        assert_eq!(pixel(&frame, stride, 60, 30), [0x30, 0x25, 0x20, 0]);
        assert_eq!(pixel(&frame, stride, 68, 30), [0xc0, 0x70, 0xf0, 0]);
        assert_eq!(pixel(&frame, stride, 20, 30), [0x70, 0x70, 0x70, 0]);

        scene.command(Command::Focus(crate::layout::Direction::Left));
        scene.render(&mut frame, width, height, stride);
        assert_eq!(pixel(&frame, stride, 20, 30), [0xc0, 0x70, 0xf0, 0]);
        assert_eq!(pixel(&frame, stride, 68, 30), [0x70, 0x70, 0x70, 0]);
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
        assert_eq!(scene.layout.placements(100, 100, 0).len(), 1);
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
    fn tiny_split_preserves_client_pixels_and_zero_area_tiles_draw_nothing() {
        let mut scene = Scene::new();
        for (object, color) in [(1, [1, 2, 3, 0]), (2, [4, 5, 6, 0]), (3, [7, 8, 9, 0])] {
            scene
                .commit(SurfaceKey { client: 1, object }, surface(color, 2, 2))
                .unwrap();
        }
        let mut frame = vec![0; 2 * 20 * 4];
        scene.render(&mut frame, 2, 20, 2 * 4);
        assert_eq!(pixel(&frame, 2 * 4, 0, 9), [1, 2, 3, 0]);
        assert_eq!(pixel(&frame, 2 * 4, 1, 9), [4, 5, 6, 0]);
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
        assert!(scene.layout.placements(100, 100, 0).is_empty());
        assert!(scene.remove(key));
        assert!(!scene.remove(key));
        scene.commit(key, surface([1, 2, 3, 0], 1, 1)).unwrap();
        assert_eq!(scene.layout.placements(100, 100, 0).len(), 1);
        assert!(scene.layout.check_invariants().is_ok());
    }

    #[test]
    fn argb_channels_are_premultiplied_before_they_reach_the_compositor() {
        let mut frame = vec![100, 100, 100, 0];
        blend_pixel(&mut frame, 1, 1, 4, 0, 0, [50, 25, 0, 128]);
        assert_eq!(frame, [99, 74, 49, 0]);
    }
}
