use crate::MAX_HELD_KEYS;
use std::collections::BTreeSet;
use std::fmt::Write;

const GLYPH_WIDTH: usize = 5;
pub(crate) const GLYPH_HEIGHT: usize = 7;
pub(crate) const GLYPH_ADVANCE: usize = 6;
const MAX_HELD_BUTTONS: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UiKeyState {
    Released,
    Pressed,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UiModifiers {
    pub depressed: u32,
    pub latched: u32,
    pub locked: u32,
    pub group: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KeyboardUpdate {
    Enter { keys: BTreeSet<u32> },
    Leave,
    Key { key: u32, state: UiKeyState },
    Modifiers(UiModifiers),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PointerUpdate {
    Enter { x: i32, y: i32 },
    Leave,
    Motion { x: i32, y: i32 },
    Button { button: u32, state: UiKeyState },
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UiModel {
    keyboard_focused: bool,
    pointer_focused: bool,
    pointer_x: i32,
    pointer_y: i32,
    keys: BTreeSet<u32>,
    buttons: BTreeSet<u32>,
    modifiers: UiModifiers,
    last_key: Option<(u32, UiKeyState)>,
    revision: u64,
}

impl UiModel {
    pub fn revision(&self) -> u64 {
        self.revision
    }

    #[cfg(test)]
    pub fn last_key(&self) -> Option<(u32, UiKeyState)> {
        self.last_key
    }

    #[cfg(test)]
    pub fn pointer_has_button(&self, x: i32, y: i32, button: u32) -> bool {
        self.pointer_focused
            && self.pointer_x == x
            && self.pointer_y == y
            && self.buttons.contains(&button)
    }

    pub fn keyboard(&mut self, update: KeyboardUpdate) -> Result<bool, String> {
        let mut next = self.clone();
        match update {
            KeyboardUpdate::Enter { keys } => {
                if keys.len() > MAX_HELD_KEYS {
                    return Err(format!(
                        "keyboard enter carries more than {MAX_HELD_KEYS} keys"
                    ));
                }
                next.keyboard_focused = true;
                next.keys = keys;
            }
            KeyboardUpdate::Leave => {
                next.keyboard_focused = false;
                next.keys.clear();
                next.modifiers = UiModifiers::default();
            }
            KeyboardUpdate::Key { key, state } => {
                if !next.keyboard_focused {
                    return Err(format!("keyboard key {key} arrived without focus"));
                }
                match state {
                    UiKeyState::Pressed => {
                        if !next.keys.contains(&key) && next.keys.len() >= MAX_HELD_KEYS {
                            return Err(format!(
                                "keyboard state exceeds {MAX_HELD_KEYS} held keys"
                            ));
                        }
                        next.keys.insert(key);
                    }
                    UiKeyState::Released => {
                        next.keys.remove(&key);
                    }
                }
                next.last_key = Some((key, state));
            }
            KeyboardUpdate::Modifiers(modifiers) => {
                if !next.keyboard_focused {
                    return Err("keyboard modifiers arrived without focus".into());
                }
                next.modifiers = modifiers;
            }
        }
        self.finish(next)
    }

    pub fn pointer_frame(&mut self, updates: &[PointerUpdate]) -> Result<bool, String> {
        let mut next = self.clone();
        for update in updates {
            match *update {
                PointerUpdate::Enter { x, y } => {
                    next.pointer_focused = true;
                    next.pointer_x = x;
                    next.pointer_y = y;
                }
                PointerUpdate::Leave => {
                    next.pointer_focused = false;
                    next.buttons.clear();
                }
                PointerUpdate::Motion { x, y } => {
                    if !next.pointer_focused {
                        return Err("pointer motion arrived without focus".into());
                    }
                    next.pointer_x = x;
                    next.pointer_y = y;
                }
                PointerUpdate::Button { button, state } => {
                    if !next.pointer_focused {
                        return Err(format!("pointer button {button} arrived without focus"));
                    }
                    match state {
                        UiKeyState::Pressed => {
                            if !next.buttons.contains(&button)
                                && next.buttons.len() >= MAX_HELD_BUTTONS
                            {
                                return Err(format!(
                                    "pointer state exceeds {MAX_HELD_BUTTONS} held buttons"
                                ));
                            }
                            next.buttons.insert(button);
                        }
                        UiKeyState::Released => {
                            next.buttons.remove(&button);
                        }
                    }
                }
            }
        }
        self.finish(next)
    }

    pub fn paint(&self, pixels: &mut [u8], width: usize, height: usize) -> Result<(), String> {
        let expected = width
            .checked_mul(height)
            .and_then(|count| count.checked_mul(4))
            .ok_or_else(|| "UI text frame size overflow".to_string())?;
        if pixels.len() != expected {
            return Err(format!(
                "UI text frame has {} bytes, expected {expected}",
                pixels.len()
            ));
        }
        let scale = if width >= 480 && height >= 240 { 2 } else { 1 };
        let left = width / 16 + 8;
        let top = height / 12 + 6;
        let step = GLYPH_HEIGHT
            .saturating_mul(scale)
            .saturating_add(4usize.saturating_mul(scale));
        let lines = [
            ("TD WAYLAND INPUT".to_string(), [0xff, 0xff, 0xff, 0]),
            (self.keyboard_line()?, [0xe0, 0xd0, 0xff, 0]),
            (self.last_key_line()?, [0xc0, 0xe8, 0xff, 0]),
            (self.modifier_line()?, [0xc0, 0xe8, 0xff, 0]),
            (self.pointer_line()?, [0xd0, 0xff, 0xd8, 0]),
            (self.button_line()?, [0xd0, 0xff, 0xd8, 0]),
        ];
        for (row, (line, color)) in lines.into_iter().enumerate() {
            let y = top.saturating_add(row.saturating_mul(step));
            draw_text(
                pixels,
                width,
                height,
                width.saturating_mul(4),
                left,
                y,
                scale,
                &line,
                color,
            );
        }
        Ok(())
    }

    fn finish(&mut self, mut next: UiModel) -> Result<bool, String> {
        if &next == self {
            return Ok(false);
        }
        next.revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| "UI model revision exhausted".to_string())?;
        *self = next;
        Ok(true)
    }

    fn keyboard_line(&self) -> Result<String, String> {
        let mut line = if self.keyboard_focused {
            "KEYBOARD FOCUS KEYS".to_string()
        } else {
            "KEYBOARD IDLE KEYS".to_string()
        };
        if self.keys.is_empty() {
            line.push_str(" NONE");
            return Ok(line);
        }
        for key in self.keys.iter().take(6) {
            line.push(' ');
            append_key(&mut line, *key)?;
        }
        Ok(line)
    }

    fn last_key_line(&self) -> Result<String, String> {
        let mut line = "LAST KEY".to_string();
        let Some((key, state)) = self.last_key else {
            line.push_str(" NONE");
            return Ok(line);
        };
        line.push(' ');
        append_key(&mut line, key)?;
        match state {
            UiKeyState::Released => line.push_str(" UP"),
            UiKeyState::Pressed => line.push_str(" DOWN"),
        }
        Ok(line)
    }

    fn modifier_line(&self) -> Result<String, String> {
        let mut line = String::new();
        write!(
            &mut line,
            "MODS {} {} {} {}",
            self.modifiers.depressed,
            self.modifiers.latched,
            self.modifiers.locked,
            self.modifiers.group
        )
        .map_err(|_| "format UI modifiers".to_string())?;
        Ok(line)
    }

    fn pointer_line(&self) -> Result<String, String> {
        let state = if self.pointer_focused {
            "FOCUS"
        } else {
            "IDLE"
        };
        let mut line = String::new();
        write!(
            &mut line,
            "POINTER {} {} {state}",
            fixed_integer(self.pointer_x),
            fixed_integer(self.pointer_y)
        )
        .map_err(|_| "format UI pointer".to_string())?;
        Ok(line)
    }

    fn button_line(&self) -> Result<String, String> {
        let mut line = "BUTTONS".to_string();
        if self.buttons.is_empty() {
            line.push_str(" NONE");
            return Ok(line);
        }
        for button in self.buttons.iter().take(6) {
            write!(&mut line, " {button}").map_err(|_| "format UI buttons".to_string())?;
        }
        Ok(line)
    }
}

fn fixed_integer(value: i32) -> i32 {
    value / 256
}

fn append_key(line: &mut String, key: u32) -> Result<(), String> {
    if let Some(label) = key_label(key) {
        line.push_str(label);
        Ok(())
    } else {
        write!(line, "{key}").map_err(|_| "format UI key".to_string())
    }
}

fn key_label(key: u32) -> Option<&'static str> {
    match key {
        1 => Some("ESC"),
        2 => Some("1"),
        3 => Some("2"),
        4 => Some("3"),
        5 => Some("4"),
        6 => Some("5"),
        7 => Some("6"),
        8 => Some("7"),
        9 => Some("8"),
        10 => Some("9"),
        11 => Some("0"),
        14 => Some("BACKSPACE"),
        15 => Some("TAB"),
        16 => Some("Q"),
        17 => Some("W"),
        18 => Some("E"),
        19 => Some("R"),
        20 => Some("T"),
        21 => Some("Y"),
        22 => Some("U"),
        23 => Some("I"),
        24 => Some("O"),
        25 => Some("P"),
        28 => Some("ENTER"),
        30 => Some("A"),
        31 => Some("S"),
        32 => Some("D"),
        33 => Some("F"),
        34 => Some("G"),
        35 => Some("H"),
        36 => Some("J"),
        37 => Some("K"),
        38 => Some("L"),
        44 => Some("Z"),
        45 => Some("X"),
        46 => Some("C"),
        47 => Some("V"),
        48 => Some("B"),
        49 => Some("N"),
        50 => Some("M"),
        57 => Some("SPACE"),
        59 => Some("F1"),
        60 => Some("F2"),
        61 => Some("F3"),
        62 => Some("F4"),
        63 => Some("F5"),
        64 => Some("F6"),
        65 => Some("F7"),
        66 => Some("F8"),
        67 => Some("F9"),
        68 => Some("F10"),
        87 => Some("F11"),
        88 => Some("F12"),
        103 => Some("UP"),
        105 => Some("LEFT"),
        106 => Some("RIGHT"),
        108 => Some("DOWN"),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_text(
    pixels: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    x: usize,
    y: usize,
    scale: usize,
    text: &str,
    color: [u8; 4],
) {
    draw_text_clipped(
        pixels,
        width,
        height,
        stride,
        x,
        y,
        scale,
        text,
        color,
        (0, 0, width, height),
    );
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_text_clipped(
    pixels: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    x: usize,
    y: usize,
    scale: usize,
    text: &str,
    color: [u8; 4],
    clip: (usize, usize, usize, usize),
) {
    let (clip_x, clip_y, clip_width, clip_height) = clip;
    let clip_right = clip_x.saturating_add(clip_width).min(width);
    let clip_bottom = clip_y.saturating_add(clip_height).min(height);
    // One glyph per CHARACTER, not per byte. Titles are client-supplied UTF-8
    // and are bounded in characters, so walking bytes would spend a multi-byte
    // scalar's worth of cells on it and push later text out of the clip.
    for (column, character) in text.chars().enumerate() {
        let origin_x = x.saturating_add(column.saturating_mul(GLYPH_ADVANCE).saturating_mul(scale));
        // The origin only advances, so nothing after this can land either. A
        // title is up to 256 characters against a band that holds ten, and
        // without this every one of the rest is rasterized and discarded a
        // pixel at a time, on every frame.
        if origin_x >= clip_right {
            break;
        }
        for (glyph_y, bits) in glyph_for(character).into_iter().enumerate() {
            for glyph_x in 0..GLYPH_WIDTH {
                let shift = GLYPH_WIDTH.saturating_sub(1).saturating_sub(glyph_x);
                if bits & (1u8 << shift) == 0 {
                    continue;
                }
                for scale_y in 0..scale {
                    for scale_x in 0..scale {
                        let pixel_x = origin_x
                            .saturating_add(glyph_x.saturating_mul(scale))
                            .saturating_add(scale_x);
                        let pixel_y = y
                            .saturating_add(glyph_y.saturating_mul(scale))
                            .saturating_add(scale_y);
                        if pixel_x < clip_x
                            || pixel_x >= clip_right
                            || pixel_y < clip_y
                            || pixel_y >= clip_bottom
                        {
                            continue;
                        }
                        put_pixel(pixels, width, height, stride, pixel_x, pixel_y, color);
                    }
                }
            }
        }
    }
}

pub(crate) fn intersect(
    rect: (usize, usize, usize, usize),
    clip: (usize, usize, usize, usize),
) -> (usize, usize, usize, usize) {
    let (left, top, width, height) = rect;
    let (clip_left, clip_top, clip_width, clip_height) = clip;
    let right = left.saturating_add(width);
    let bottom = top.saturating_add(height);
    let clip_right = clip_left.saturating_add(clip_width);
    let clip_bottom = clip_top.saturating_add(clip_height);
    let clipped_left = left.max(clip_left);
    let clipped_top = top.max(clip_top);
    let clipped_right = right.min(clip_right);
    let clipped_bottom = bottom.min(clip_bottom);
    (
        clipped_left,
        clipped_top,
        clipped_right.saturating_sub(clipped_left),
        clipped_bottom.saturating_sub(clipped_top),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn fill(
    frame: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    rect: (usize, usize, usize, usize),
    color: [u8; 4],
) {
    let (left, top, rect_width, rect_height) = rect;
    let right = left.saturating_add(rect_width).min(width);
    let bottom = top.saturating_add(rect_height).min(height);
    for y in top..bottom {
        for x in left..right {
            put_pixel(frame, width, height, stride, x, y, color);
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn border(
    frame: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    rect: (usize, usize, usize, usize),
    color: [u8; 4],
) {
    let (left, top, rect_width, rect_height) = rect;
    if rect_width == 0 || rect_height == 0 {
        return;
    }
    let right = left.saturating_add(rect_width).saturating_sub(1);
    let bottom = top.saturating_add(rect_height).saturating_sub(1);
    for x in left..=right.min(width.saturating_sub(1)) {
        put_pixel(frame, width, height, stride, x, top, color);
        put_pixel(frame, width, height, stride, x, bottom, color);
    }
    for y in top..=bottom.min(height.saturating_sub(1)) {
        put_pixel(frame, width, height, stride, left, y, color);
        put_pixel(frame, width, height, stride, right, y, color);
    }
}

#[allow(clippy::too_many_arguments)]
fn put_pixel(
    pixels: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    x: usize,
    y: usize,
    color: [u8; 4],
) {
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
    if let Some(pixel) = pixels.get_mut(offset..end) {
        pixel.copy_from_slice(&color);
    }
}

/// One cell for one character. A scalar past `u8` has no byte to look up and
/// takes the missing box, which is the same answer the table gives for every
/// byte it does not carry — so a font that grows a Latin-1 row serves it
/// without a second rule here.
fn glyph_for(character: char) -> [u8; GLYPH_HEIGHT] {
    u8::try_from(character).map(glyph).unwrap_or(MISSING)
}

fn glyph(byte: u8) -> [u8; GLYPH_HEIGHT] {
    match byte.to_ascii_uppercase() {
        b'A' => [14, 17, 17, 31, 17, 17, 17],
        b'B' => [30, 17, 17, 30, 17, 17, 30],
        b'C' => [14, 17, 16, 16, 16, 17, 14],
        b'D' => [30, 17, 17, 17, 17, 17, 30],
        b'E' => [31, 16, 16, 30, 16, 16, 31],
        b'F' => [31, 16, 16, 30, 16, 16, 16],
        b'G' => [14, 17, 16, 23, 17, 17, 15],
        b'H' => [17, 17, 17, 31, 17, 17, 17],
        b'I' => [14, 4, 4, 4, 4, 4, 14],
        b'J' => [7, 2, 2, 2, 18, 18, 12],
        b'K' => [17, 18, 20, 24, 20, 18, 17],
        b'L' => [16, 16, 16, 16, 16, 16, 31],
        b'M' => [17, 27, 21, 21, 17, 17, 17],
        b'N' => [17, 25, 21, 19, 17, 17, 17],
        b'O' => [14, 17, 17, 17, 17, 17, 14],
        b'P' => [30, 17, 17, 30, 16, 16, 16],
        b'Q' => [14, 17, 17, 17, 21, 18, 13],
        b'R' => [30, 17, 17, 30, 20, 18, 17],
        b'S' => [15, 16, 16, 14, 1, 1, 30],
        b'T' => [31, 4, 4, 4, 4, 4, 4],
        b'U' => [17, 17, 17, 17, 17, 17, 14],
        b'V' => [17, 17, 17, 17, 17, 10, 4],
        b'W' => [17, 17, 17, 21, 21, 21, 10],
        b'X' => [17, 17, 10, 4, 10, 17, 17],
        b'Y' => [17, 17, 10, 4, 4, 4, 4],
        b'Z' => [31, 1, 2, 4, 8, 16, 31],
        b'0' => [14, 17, 19, 21, 25, 17, 14],
        b'1' => [4, 12, 4, 4, 4, 4, 14],
        b'2' => [14, 17, 1, 2, 4, 8, 31],
        b'3' => [30, 1, 1, 14, 1, 1, 30],
        b'4' => [2, 6, 10, 18, 31, 2, 2],
        b'5' => [31, 16, 16, 30, 1, 1, 30],
        b'6' => [14, 16, 16, 30, 17, 17, 14],
        b'7' => [31, 1, 2, 4, 8, 8, 8],
        b'8' => [14, 17, 17, 14, 17, 17, 14],
        b'9' => [14, 17, 17, 15, 1, 1, 14],
        b':' => [0, 4, 4, 0, 4, 4, 0],
        b'-' => [0, 0, 0, 31, 0, 0, 0],
        b'+' => [0, 4, 4, 31, 4, 4, 0],
        b'/' => [1, 2, 2, 4, 8, 8, 16],
        b'_' => [0, 0, 0, 0, 0, 0, 31],
        b'.' => [0, 0, 0, 0, 0, 0, 4],
        b'?' => [14, 17, 1, 2, 4, 0, 4],
        b' ' => [0, 0, 0, 0, 0, 0, 0],
        _ => MISSING,
    }
}

/// What an unmapped byte draws. A BOX rather than the question mark this used
/// to be, because `?` is a character td's own UI spells on purpose — it is how
/// the status bar says a reading could not be taken — and a font gap that drew
/// the same thing made a healthy `LOAD 0.42` render as `LOAD 0?42`, which is
/// indistinguishable from a failure.
const MISSING: [u8; GLYPH_HEIGHT] = [31, 17, 17, 17, 17, 17, 31];

/// Whether the font has a glyph of its own for this byte. Callers with a fixed
/// vocabulary — the status bar's line, the help sheet's rows — assert over it,
/// since nothing the compiler sees connects a string literal to the font.
#[cfg(test)]
pub(crate) fn is_mapped(byte: u8) -> bool {
    glyph(byte) != MISSING
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipped_text_never_writes_outside_horizontal_bounds() {
        let width = 12;
        let height = GLYPH_HEIGHT;
        let stride = width * 4;
        let color = [1, 2, 3, 4];
        let mut pixels = vec![0; stride * height];
        draw_text_clipped(
            &mut pixels,
            width,
            height,
            stride,
            4,
            0,
            1,
            "A",
            color,
            (6, 0, 2, height),
        );
        let mut painted = false;
        for (y, row) in pixels.chunks_exact(stride).enumerate() {
            for x in 0..width {
                let start = x * 4;
                let pixel = row.get(start..start + 4).unwrap();
                if pixel == color {
                    assert!((6..8).contains(&x), "painted ({x}, {y})");
                    painted = true;
                } else {
                    assert_eq!(pixel, [0, 0, 0, 0]);
                }
            }
        }
        assert!(painted);
    }

    #[test]
    fn keyboard_state_is_explicit_bounded_and_focus_scoped() {
        let mut model = UiModel::default();
        assert!(model
            .keyboard(KeyboardUpdate::Enter {
                keys: BTreeSet::from([30, 31]),
            })
            .unwrap());
        assert_eq!(model.revision(), 1);
        assert!(model
            .keyboard(KeyboardUpdate::Modifiers(UiModifiers {
                depressed: 5,
                latched: 0,
                locked: 16,
                group: 0,
            }))
            .unwrap());
        assert!(model
            .keyboard(KeyboardUpdate::Key {
                key: 30,
                state: UiKeyState::Released,
            })
            .unwrap());
        assert!(model.keyboard_line().unwrap().contains("S"));
        assert_eq!(model.last_key_line().unwrap(), "LAST KEY A UP");
        assert_eq!(model.modifier_line().unwrap(), "MODS 5 0 16 0");
        assert!(model.keyboard(KeyboardUpdate::Leave).unwrap());
        assert!(model
            .keyboard(KeyboardUpdate::Key {
                key: 30,
                state: UiKeyState::Pressed,
            })
            .is_err());
        assert!(model
            .keyboard(KeyboardUpdate::Modifiers(UiModifiers::default()))
            .is_err());

        let too_many = (0..=MAX_HELD_KEYS as u32).collect();
        assert!(UiModel::default()
            .keyboard(KeyboardUpdate::Enter { keys: too_many })
            .is_err());
    }

    #[test]
    fn pointer_frames_are_atomic_and_leave_clears_buttons() {
        let mut model = UiModel::default();
        assert!(model
            .pointer_frame(&[
                PointerUpdate::Enter {
                    x: 12 * 256,
                    y: 34 * 256,
                },
                PointerUpdate::Button {
                    button: 272,
                    state: UiKeyState::Pressed,
                },
            ])
            .unwrap());
        assert_eq!(model.revision(), 1);
        assert_eq!(model.pointer_line().unwrap(), "POINTER 12 34 FOCUS");
        assert_eq!(model.button_line().unwrap(), "BUTTONS 272");
        assert!(model
            .pointer_frame(&[PointerUpdate::Motion {
                x: -2 * 256,
                y: 40 * 256,
            }])
            .unwrap());
        assert_eq!(model.pointer_line().unwrap(), "POINTER -2 40 FOCUS");
        assert!(model.pointer_frame(&[PointerUpdate::Leave]).unwrap());
        assert_eq!(model.button_line().unwrap(), "BUTTONS NONE");
        assert!(model
            .pointer_frame(&[PointerUpdate::Motion { x: 0, y: 0 }])
            .is_err());
        assert!(model
            .pointer_frame(&[PointerUpdate::Button {
                button: 272,
                state: UiKeyState::Released,
            }])
            .is_err());
    }

    #[test]
    fn pointer_button_capacity_is_checked_before_mutation() {
        let mut model = UiModel::default();
        model
            .pointer_frame(&[PointerUpdate::Enter { x: 0, y: 0 }])
            .unwrap();
        let buttons: Vec<PointerUpdate> = (0..MAX_HELD_BUTTONS)
            .map(|button| PointerUpdate::Button {
                button: u32::try_from(button).unwrap(),
                state: UiKeyState::Pressed,
            })
            .collect();
        model.pointer_frame(&buttons).unwrap();
        let snapshot = model.clone();
        assert!(model
            .pointer_frame(&[PointerUpdate::Button {
                button: 999,
                state: UiKeyState::Pressed,
            }])
            .is_err());
        assert_eq!(model, snapshot);
    }

    #[test]
    fn text_raster_is_deterministic_clipped_and_xrgb() {
        let mut model = UiModel::default();
        let mut first = vec![0u8; 320 * 200 * 4];
        let mut second = first.clone();
        model.paint(&mut first, 320, 200).unwrap();
        model.paint(&mut second, 320, 200).unwrap();
        assert_eq!(first, second);
        assert!(first
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| *pixel != [0; 4]));
        assert!(first.as_chunks::<4>().0.iter().all(|pixel| pixel[3] == 0));

        let mut tiny = vec![0u8; 4];
        draw_text(&mut tiny, 1, 1, 4, 0, 0, 2, "A", [1, 2, 3, 0]);
        assert_eq!(tiny, [0, 0, 0, 0]);
        assert!(model.paint(&mut tiny, 2, 1).is_err());

        model
            .pointer_frame(&[
                PointerUpdate::Enter {
                    x: 12 * 256,
                    y: 34 * 256,
                },
                PointerUpdate::Button {
                    button: 0x110,
                    state: UiKeyState::Pressed,
                },
            ])
            .unwrap();
        let mut pointer = vec![0u8; first.len()];
        model.paint(&mut pointer, 320, 200).unwrap();
        assert_ne!(pointer, first);

        model
            .keyboard(KeyboardUpdate::Enter {
                keys: BTreeSet::new(),
            })
            .unwrap();
        model
            .keyboard(KeyboardUpdate::Key {
                key: 30,
                state: UiKeyState::Pressed,
            })
            .unwrap();
        let mut keyboard = vec![0u8; first.len()];
        model.paint(&mut keyboard, 320, 200).unwrap();
        assert_ne!(keyboard, pointer);
    }

    /// A helper that renders one string into a fresh buffer, so two renders
    /// can be compared byte for byte.
    fn rendered(text: &str, width: usize, clip: (usize, usize, usize, usize)) -> Vec<u8> {
        let height = GLYPH_HEIGHT * 2;
        let stride = width * 4;
        let mut pixels = vec![0u8; stride * height];
        draw_text_clipped(
            &mut pixels,
            width,
            height,
            stride,
            0,
            0,
            2,
            text,
            [9, 8, 7, 6],
            clip,
        );
        pixels
    }

    #[test]
    fn text_advances_by_character_and_stops_where_the_clip_does() {
        let width = 64usize;
        let all = (0, 0, width, GLYPH_HEIGHT * 2);

        // One cell per CHARACTER. `é` is two bytes and has no glyph, so a
        // byte-walking renderer spends TWO cells on it and pushes the `A` one
        // cell right — which is what this compares against rather than
        // merely asserting that something was drawn.
        assert_eq!(rendered("\u{e9}A", width, all), rendered("@A", width, all));
        assert_ne!(rendered("\u{e9}A", width, all), rendered("@@A", width, all));

        // And the early-out is invisible: a string far longer than its clip
        // paints exactly what the prefix that fits paints. The `break` itself
        // is not otherwise falsifiable — it changes no pixel, only how many
        // are computed and thrown away — so what is pinned is that it throws
        // away nothing that would have shown.
        // Exactly two cells wide, so the third character's ORIGIN is the
        // first pixel past the clip and the break is on the boundary rather
        // than one glyph inside it — at 25 the third glyph's leftmost column
        // still lands, and "the prefix that fits" would be three characters.
        let band = (0, 0, GLYPH_ADVANCE * 2 * 2, GLYPH_HEIGHT * 2);
        assert_eq!(
            rendered(&"AB".repeat(128), width, band),
            rendered("AB", width, band)
        );
    }

    #[test]
    fn font_covers_the_ui_vocabulary_and_unknown_falls_back() {
        // `is_mapped`, not a length. `glyph` returns `[u8; GLYPH_HEIGHT]`, so
        // asserting its `len()` was a tautology wearing this test's name —
        // which is how the missing period got past it.
        for byte in b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 :-+/_?." {
            assert!(is_mapped(*byte), "{:?} has no glyph", *byte as char);
        }
        // The fallback is its own shape and `?` is NOT it. This line used to
        // assert the opposite — pinning the collision that made a healthy
        // `LOAD 0.42` draw as `LOAD 0?42`, indistinguishable from `LOAD ?`.
        assert!(!is_mapped(b'@'));
        assert_ne!(glyph(b'?'), MISSING);
        assert_eq!(key_label(30), Some("A"));
        assert_eq!(key_label(108), Some("DOWN"));
        assert_eq!(key_label(999), None);
    }

    #[test]
    fn revision_exhaustion_preserves_the_model() {
        let mut model = UiModel {
            revision: u64::MAX,
            ..UiModel::default()
        };
        let snapshot = model.clone();
        assert!(model
            .keyboard(KeyboardUpdate::Enter {
                keys: BTreeSet::new(),
            })
            .is_err());
        assert_eq!(model, snapshot);
    }
}
