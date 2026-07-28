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
    order: Vec<SurfaceKey>,
    focused: usize,
    pointer_x: i32,
    pointer_y: i32,
    surface_bytes: usize,
}

impl Scene {
    pub fn new() -> Scene {
        Scene {
            surfaces: BTreeMap::new(),
            order: Vec::new(),
            focused: 0,
            pointer_x: 0,
            pointer_y: 0,
            surface_bytes: 0,
        }
    }

    pub fn commit(&mut self, key: SurfaceKey, surface: Surface) -> Result<(), String> {
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
        if !self.surfaces.contains_key(&key) {
            self.order.push(key);
        }
        self.surfaces.insert(key, surface);
        self.surface_bytes = next;
        if self.focused >= self.order.len() {
            self.focused = self.order.len().saturating_sub(1);
        }
        Ok(())
    }

    pub fn remove(&mut self, key: SurfaceKey) {
        if let Some(surface) = self.surfaces.remove(&key) {
            self.surface_bytes = self.surface_bytes.saturating_sub(surface.pixels.len());
        }
        self.order.retain(|candidate| *candidate != key);
        if self.focused >= self.order.len() {
            self.focused = self.order.len().saturating_sub(1);
        }
    }

    pub fn remove_client(&mut self, client: u64) {
        let removed = self
            .surfaces
            .iter()
            .filter(|(key, _)| key.client == client)
            .fold(0usize, |total, (_, surface)| {
                total.saturating_add(surface.pixels.len())
            });
        self.surfaces.retain(|key, _| key.client != client);
        self.surface_bytes = self.surface_bytes.saturating_sub(removed);
        self.order.retain(|key| key.client != client);
        if self.focused >= self.order.len() {
            self.focused = self.order.len().saturating_sub(1);
        }
    }

    pub fn focus_left(&mut self) {
        self.focused = self.focused.saturating_sub(1);
    }

    pub fn focus_right(&mut self) {
        if self.focused.saturating_add(1) < self.order.len() {
            self.focused += 1;
        }
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

        let mut positions: Vec<(SurfaceKey, i64, i64)> = Vec::new();
        let mut focused_width = 0usize;
        if let Some(key) = self.order.get(self.focused) {
            if let Some(surface) = self.surfaces.get(key) {
                focused_width = surface
                    .width
                    .min(width.saturating_sub(GAP.saturating_mul(2)));
            }
        }
        let center_x = width
            .saturating_sub(focused_width)
            .checked_div(2)
            .unwrap_or(0);
        let mut x = i64::try_from(center_x).unwrap_or(i64::MAX);
        for key in self.order.iter().take(self.focused) {
            let Some(surface) = self.surfaces.get(key) else {
                continue;
            };
            let column = surface
                .width
                .min(width.saturating_sub(GAP.saturating_mul(2)))
                .saturating_add(GAP);
            x = x.saturating_sub(i64::try_from(column).unwrap_or(i64::MAX));
        }
        for key in &self.order {
            let Some(surface) = self.surfaces.get(key) else {
                continue;
            };
            let visible_height = surface
                .height
                .min(height.saturating_sub(GAP.saturating_mul(2)));
            let y = height.saturating_sub(visible_height) / 2;
            positions.push((key.to_owned(), x, i64::try_from(y).unwrap_or(i64::MAX)));
            let column = surface
                .width
                .min(width.saturating_sub(GAP.saturating_mul(2)))
                .saturating_add(GAP);
            x = x.saturating_add(i64::try_from(column).unwrap_or(i64::MAX));
        }

        for (index, (key, x, y)) in positions.iter().enumerate() {
            let Some(surface) = self.surfaces.get(key) else {
                continue;
            };
            draw_border(
                frame,
                width,
                height,
                stride,
                *x,
                *y,
                surface.width,
                surface.height,
                index == self.focused,
            );
            draw_surface(frame, width, height, stride, *x, *y, surface);
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
    x: i64,
    y: i64,
    surface: &Surface,
) {
    let Some((source_x_start, visible_columns)) = visible_span(x, surface.width, width) else {
        return;
    };
    let Some((source_y_start, visible_rows)) = visible_span(y, surface.height, height) else {
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
        scene.remove_client(1);
        assert_eq!(scene.surfaces.len(), 1);
        assert!(scene.surfaces.contains_key(&SurfaceKey {
            client: 2,
            object: 4
        }));
    }

    #[test]
    fn focused_surface_stays_centered_without_overlapping_its_neighbors() {
        let mut scene = Scene::new();
        for (object, color) in [(1, [1, 2, 3, 0]), (2, [4, 5, 6, 0]), (3, [7, 8, 9, 0])] {
            scene
                .commit(SurfaceKey { client: 1, object }, surface(color, 2, 2))
                .unwrap();
        }
        scene.focus_right();
        let width = 200usize;
        let height = 100usize;
        let stride = width * 4;
        let mut frame = vec![0; stride * height];
        scene.render(&mut frame, width, height, stride);
        let y = 49usize;
        for (x, expected) in [
            (73usize, [1, 2, 3, 0]),
            (99usize, [4, 5, 6, 0]),
            (125usize, [7, 8, 9, 0]),
        ] {
            let offset = y * stride + x * 4;
            assert_eq!(frame.get(offset..offset + 4), Some(expected.as_slice()));
        }
    }

    #[test]
    fn argb_channels_are_premultiplied_before_they_reach_the_compositor() {
        let mut frame = vec![100, 100, 100, 0];
        blend_pixel(&mut frame, 1, 1, 4, 0, 0, [50, 25, 0, 128]);
        assert_eq!(frame, [99, 74, 49, 0]);
    }
}
