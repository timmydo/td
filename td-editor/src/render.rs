//! Deterministic scene operations and a clipped, allocation-free XRGB painter.

use crate::font::Font;
use crate::keys::Profile;
use crate::layout::{Affinity, Break, Caret, Layout, Position, CELL_HEIGHT, CELL_WIDTH};
use crate::model::{Editor, Limits, TabId};
use crate::{text, Error, Result};

pub const MAX_AXIS: usize = 8192;
pub const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;
pub const FONT_PROVENANCE: &str = include_str!("../../td-compositor/assets/PROVENANCE");
pub const FONT_COPYING: &str = include_str!("../../td-compositor/assets/unifont-COPYING");
pub const FONT_LICENSE: &str = include_str!("../../td-compositor/assets/unifont-OFL-1.1.txt");

pub const PAPER: u32 = 0xffffff;
pub const INK: u32 = 0x202124;
pub const CHROME: u32 = 0xf0f0f0;
pub const BORDER: u32 = 0xc5c7cb;
pub const SELECTED: u32 = 0x2468c5;
pub const INACTIVE_SELECTION: u32 = 0xd6d9df;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Scale(u8);

impl Scale {
    pub fn new(value: u8) -> Result<Self> {
        if !(1..=4).contains(&value) {
            return Err(Error::InvalidArgument);
        }
        Ok(Self(value))
    }
    pub fn value(self) -> usize {
        usize::from(self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rect {
    pub x: i64,
    pub y: i64,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn intersection(self, other: Self) -> Option<Self> {
        let left = self.x.max(other.x);
        let top = self.y.max(other.y);
        let right = self
            .x
            .saturating_add(i64::from(self.width))
            .min(other.x.saturating_add(i64::from(other.width)));
        let bottom = self
            .y
            .saturating_add(i64::from(self.height))
            .min(other.y.saturating_add(i64::from(other.height)));
        if right <= left || bottom <= top {
            return None;
        }
        Some(Self {
            x: left,
            y: top,
            width: u32::try_from(right.checked_sub(left)?).ok()?,
            height: u32::try_from(bottom.checked_sub(top)?).ok()?,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Primitive {
    Fill {
        rect: Rect,
        color: u32,
    },
    Glyph {
        x: i64,
        y: i64,
        scalar: char,
        color: u32,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Draw {
    pub clip: Rect,
    pub primitive: Primitive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Geometry {
    width: usize,
    height: usize,
    scale: Scale,
}

impl Default for Geometry {
    fn default() -> Self {
        Self {
            width: 800,
            height: 600,
            scale: Scale(1),
        }
    }
}

impl Geometry {
    pub fn new(width: usize, height: usize, scale: Scale) -> Result<Self> {
        if width == 0 || height == 0 || width > MAX_AXIS || height > MAX_AXIS {
            return Err(Error::InvalidArgument);
        }
        if width
            .checked_mul(height)
            .and_then(|n| n.checked_mul(4))
            .is_none_or(|n| n > MAX_FRAME_BYTES)
        {
            return Err(Error::Limit);
        }
        Ok(Self {
            width,
            height,
            scale,
        })
    }
    pub fn dimensions(self) -> (usize, usize) {
        (self.width, self.height)
    }
    pub fn scale(self) -> Scale {
        self.scale
    }
    pub fn bounds(self) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: self.width as u32,
            height: self.height as u32,
        }
    }
    pub fn document(self) -> Rect {
        let s = self.scale.value();
        Rect {
            x: (8 * s) as i64,
            y: (48 * s) as i64,
            width: self.width.saturating_sub(16 * s) as u32,
            height: self.height.saturating_sub(72 * s) as u32,
        }
    }
    pub fn grid(self) -> (usize, usize) {
        let doc = self.document();
        (
            doc.width as usize / (CELL_WIDTH * self.scale.value()),
            doc.height as usize / (CELL_HEIGHT * self.scale.value()),
        )
    }
    pub fn status(self) -> Rect {
        let height = 24 * self.scale.value();
        Rect {
            x: 0,
            y: self.height.saturating_sub(height) as i64,
            width: self.width as u32,
            height: height as u32,
        }
    }
    pub fn tab_close(self, index: usize, active: usize, count: usize) -> Option<Rect> {
        let tab = self.tab(index, active, count)?;
        let width = (24 * self.scale.value()) as u32;
        Some(Rect {
            x: tab.x + i64::from(tab.width - width),
            width,
            ..tab
        })
    }
    /// The active tab is always in the strip. Narrow surfaces clip one tab.
    pub fn tab(self, index: usize, active: usize, count: usize) -> Option<Rect> {
        if index >= count || count > Limits::default().tabs || active >= count {
            return None;
        }
        let width = 160 * self.scale.value();
        let visible = (self.width / width).max(1);
        let first = active.saturating_sub(visible - 1);
        let slot = index.checked_sub(first)?;
        if slot >= visible {
            return None;
        }
        Some(Rect {
            x: (slot * width) as i64,
            y: (24 * self.scale.value()) as i64,
            width: width as u32,
            height: (24 * self.scale.value()) as u32,
        })
    }
}

pub struct Raster<'pixels, 'font> {
    pixels: &'pixels mut [u8],
    font: &'font Font,
    geometry: Geometry,
    stride: usize,
}

impl<'pixels, 'font> Raster<'pixels, 'font> {
    /// Validation precedes all writes. Padding and excess storage stay intact.
    pub fn new(
        pixels: &'pixels mut [u8],
        font: &'font Font,
        geometry: Geometry,
        stride: usize,
    ) -> Result<Self> {
        if (font.width(), font.height()) != (CELL_WIDTH, CELL_HEIGHT)
            || stride < geometry.width * 4
            || !stride.is_multiple_of(4)
        {
            return Err(Error::InvalidArgument);
        }
        let needed = stride.checked_mul(geometry.height).ok_or(Error::Limit)?;
        if needed > MAX_FRAME_BYTES {
            return Err(Error::Limit);
        }
        if pixels.len() < needed {
            return Err(Error::InvalidArgument);
        }
        Ok(Self {
            pixels,
            font,
            geometry,
            stride,
        })
    }

    pub fn paint(&mut self, scene: &Scene<'_>, damage: Rect) -> Result<()> {
        if self.geometry != scene.geometry {
            return Err(Error::InvalidArgument);
        }
        scene.emit(damage, &mut |draw| self.draw(draw));
        Ok(())
    }

    pub fn draw(&mut self, draw: Draw) {
        let Some(clip) = draw.clip.intersection(self.geometry.bounds()) else {
            return;
        };
        let (rect, color, glyph) = match draw.primitive {
            Primitive::Fill { rect, color } => (rect, color, None),
            Primitive::Glyph {
                x,
                y,
                scalar,
                color,
            } => (
                Rect {
                    x,
                    y,
                    width: (CELL_WIDTH * self.geometry.scale.value()) as u32,
                    height: (CELL_HEIGHT * self.geometry.scale.value()) as u32,
                },
                color,
                Some(self.font.index(scalar)),
            ),
        };
        let Some(area) = rect.intersection(clip) else {
            return;
        };
        let bytes = (color | 0xff000000).to_le_bytes();
        for y in area.y..area.y + i64::from(area.height) {
            for x in area.x..area.x + i64::from(area.width) {
                if let Some(index) = glyph {
                    // Intersection with an at-most-32x64 glyph makes these
                    // differences small even for hostile signed origins.
                    let col = x.saturating_sub(rect.x) as usize / self.geometry.scale.value();
                    let row = y.saturating_sub(rect.y) as usize / self.geometry.scale.value();
                    if !self.font.pixel(index, col, row) {
                        continue;
                    }
                }
                let at = y as usize * self.stride + x as usize * 4;
                if let Some(pixel) = self.pixels.get_mut(at..at + 4) {
                    pixel.copy_from_slice(&bytes);
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct View {
    pub origin: Position,
    pub soft_wrap: bool,
    pub affinity: Affinity,
    pub focused: bool,
    pub caret_visible: bool,
}

impl Default for View {
    fn default() -> Self {
        Self {
            origin: Position { row: 0, column: 0 },
            soft_wrap: true,
            affinity: Affinity::Downstream,
            focused: true,
            caret_visible: true,
        }
    }
}

/// Display labels only; they never acquire a file association or authority.
pub struct Label<'a> {
    pub tab: TabId,
    pub title: &'a str,
}

pub struct Scene<'a> {
    editor: &'a Editor,
    geometry: Geometry,
    view: View,
    labels: &'a [Label<'a>],
    status: String,
    caret: Option<Position>,
}

impl<'a> Scene<'a> {
    pub fn new(
        editor: &'a Editor,
        geometry: Geometry,
        view: View,
        labels: &'a [Label<'a>],
        profile: Profile,
    ) -> Result<Self> {
        if labels.len() > Limits::default().tabs
            || labels.iter().any(|label| label.title.len() > 4096)
            || view.origin.row > text::MAX_FILE_BYTES
            || view.origin.column > text::MAX_FILE_BYTES * 8
        {
            return Err(Error::Limit);
        }
        for (index, label) in labels.iter().enumerate() {
            editor.document(label.tab)?;
            if labels
                .iter()
                .take(index)
                .any(|previous| previous.tab == label.tab)
            {
                return Err(Error::InvalidArgument);
            }
        }
        let mut caret = None;
        let mut status = String::from("No document");
        if let Some(id) = editor.active() {
            let doc = editor.document(id)?;
            let byte = doc.selection().caret;
            let before = doc.text().get(..byte).ok_or(Error::InvalidPosition)?;
            let line = before.bytes().filter(|b| *b == b'\n').count() + 1;
            let current_line = before.rsplit_once('\n').map_or(before, |(_, tail)| tail);
            let column = text::column(current_line) + 1;
            status = format!(
                "Ln {line}, Col {column}   {}   Fill:{}   {}   Spelling: not checked",
                if doc.format().ending == text::LineEnding::Lf {
                    "LF"
                } else {
                    "CRLF"
                },
                if doc.auto_fill() { "on" } else { "off" },
                if profile == Profile::Windows {
                    "Windows"
                } else {
                    "Emacs"
                }
            );
            let (columns, rows) = geometry.grid();
            if columns != 0 && rows != 0 && view.focused && view.caret_visible {
                caret = Some(
                    Layout::for_document(doc, columns, view.soft_wrap)?.position(Caret {
                        byte,
                        affinity: view.affinity,
                    })?,
                );
            }
        }
        Ok(Self {
            editor,
            geometry,
            view,
            labels,
            status,
            caret,
        })
    }

    /// Streams operations: the backend need not allocate a retained scene list.
    pub fn emit(&self, damage: Rect, sink: &mut impl FnMut(Draw)) {
        let Some(clip) = damage.intersection(self.geometry.bounds()) else {
            return;
        };
        let s = self.geometry.scale.value() as i64;
        let fill = |rect: Rect, color, sink: &mut dyn FnMut(Draw)| {
            if let Some(area) = rect.intersection(clip) {
                sink(Draw {
                    clip: area,
                    primitive: Primitive::Fill { rect: area, color },
                });
            }
        };
        fill(self.geometry.bounds(), PAPER, sink);
        fill(
            Rect {
                x: 0,
                y: 0,
                width: self.geometry.width as u32,
                height: (48 * s) as u32,
            },
            CHROME,
            sink,
        );
        self.label(
            "File   Edit   Format   Help".chars(),
            (8 * s, 4 * s),
            Rect {
                x: 0,
                y: 0,
                width: self.geometry.width as u32,
                height: (24 * s) as u32,
            },
            INK,
            clip,
            sink,
        );
        let count = self.editor.tabs().count();
        let active = self
            .editor
            .tabs()
            .position(|(id, _)| Some(id) == self.editor.active())
            .unwrap_or(0);
        for (index, (id, doc)) in self.editor.tabs().enumerate() {
            let Some(rect) = self.geometry.tab(index, active, count) else {
                continue;
            };
            fill(
                rect,
                if Some(id) == self.editor.active() {
                    PAPER
                } else {
                    CHROME
                },
                sink,
            );
            fill(
                Rect {
                    x: rect.x,
                    y: rect.y,
                    width: rect.width,
                    height: s as u32,
                },
                BORDER,
                sink,
            );
            fill(
                Rect {
                    x: rect.x + i64::from(rect.width) - s,
                    y: rect.y,
                    width: s as u32,
                    height: rect.height,
                },
                BORDER,
                sink,
            );
            let title = self
                .labels
                .iter()
                .find(|label| label.tab == id)
                .map_or("Untitled", |label| label.title);
            let Some(close) = self.geometry.tab_close(index, active, count) else {
                continue;
            };
            let title_rect = Rect {
                width: rect.width.saturating_sub(close.width),
                ..rect
            };
            self.label(
                if doc.dirty() { "*" } else { "" }
                    .chars()
                    .chain(title.chars()),
                (rect.x + 8 * s, rect.y + 4 * s),
                title_rect,
                INK,
                clip,
                sink,
            );
            self.label(
                "x".chars(),
                (close.x + 8 * s, close.y + 4 * s),
                close,
                INK,
                clip,
                sink,
            );
        }
        self.document(clip, sink);
        let status_rect = self.geometry.status();
        fill(status_rect, CHROME, sink);
        fill(
            Rect {
                height: s as u32,
                ..status_rect
            },
            BORDER,
            sink,
        );
        self.label(
            self.status.chars(),
            (8 * s, status_rect.y + 4 * s),
            status_rect,
            INK,
            clip,
            sink,
        );
    }

    fn document(&self, damage: Rect, sink: &mut impl FnMut(Draw)) {
        let (columns, rows) = self.geometry.grid();
        if columns == 0 || rows == 0 {
            return;
        }
        let Some(clip) = self.geometry.document().intersection(damage) else {
            return;
        };
        let Some(doc) = self
            .editor
            .active()
            .and_then(|id| self.editor.document(id).ok())
        else {
            return;
        };
        let Ok(layout) = Layout::for_document(doc, columns, self.view.soft_wrap) else {
            return;
        };
        let s = self.geometry.scale.value();
        let cw = CELL_WIDTH * s;
        let ch = CELL_HEIGHT * s;
        let left = if self.view.soft_wrap {
            0
        } else {
            self.view.origin.column
        };
        let selection = doc.selection().range();
        for (index, row) in layout
            .rows()
            .skip(self.view.origin.row)
            .take(rows)
            .enumerate()
        {
            let y = self.geometry.document().y + (index * ch) as i64;
            for cell in row.cells() {
                if cell.column >= left.saturating_add(columns) {
                    break;
                }
                if cell.column + cell.width <= left {
                    continue;
                }
                let x = self.geometry.document().x + (cell.column as i64 - left as i64) * cw as i64;
                let selected =
                    cell.bytes.start >= selection.start && cell.bytes.end <= selection.end;
                if selected {
                    sink(Draw {
                        clip,
                        primitive: Primitive::Fill {
                            rect: Rect {
                                x,
                                y,
                                width: (cell.width * cw) as u32,
                                height: ch as u32,
                            },
                            color: if self.view.focused {
                                SELECTED
                            } else {
                                INACTIVE_SELECTION
                            },
                        },
                    });
                }
                if cell.scalar != '\t' {
                    sink(Draw {
                        clip,
                        primitive: Primitive::Glyph {
                            x,
                            y,
                            scalar: cell.scalar,
                            color: if selected && self.view.focused {
                                PAPER
                            } else {
                                INK
                            },
                        },
                    });
                }
            }
            let end = row.bytes().end;
            if row.ending() == Break::Newline && selection.start <= end && end < selection.end {
                let Some(column) = row.columns().checked_sub(left).filter(|col| *col < columns)
                else {
                    continue;
                };
                let x = self.geometry.document().x + (column * cw) as i64;
                sink(Draw {
                    clip,
                    primitive: Primitive::Fill {
                        rect: Rect {
                            x,
                            y,
                            width: cw as u32,
                            height: ch as u32,
                        },
                        color: if self.view.focused {
                            SELECTED
                        } else {
                            INACTIVE_SELECTION
                        },
                    },
                });
            }
        }
        if let Some(position) = self.caret {
            let Some(row) = position
                .row
                .checked_sub(self.view.origin.row)
                .filter(|row| *row < rows)
            else {
                return;
            };
            let x = if self.view.soft_wrap {
                position
                    .column
                    .saturating_mul(CELL_WIDTH)
                    .min(columns * CELL_WIDTH - 1)
                    * s
            } else {
                let Some(column) = position
                    .column
                    .checked_sub(left)
                    .filter(|col| *col < columns)
                else {
                    return;
                };
                column * cw
            };
            sink(Draw {
                clip,
                primitive: Primitive::Fill {
                    rect: Rect {
                        x: self.geometry.document().x + x as i64,
                        y: self.geometry.document().y + (row * ch) as i64,
                        width: s as u32,
                        height: ch as u32,
                    },
                    color: INK,
                },
            });
        }
    }

    fn label(
        &self,
        chars: impl Iterator<Item = char>,
        (x, y): (i64, i64),
        bounds: Rect,
        color: u32,
        damage: Rect,
        sink: &mut impl FnMut(Draw),
    ) {
        let Some(clip) = bounds.intersection(damage) else {
            return;
        };
        let cw = (CELL_WIDTH * self.geometry.scale.value()) as i64;
        let slots = (bounds
            .x
            .saturating_add(i64::from(bounds.width))
            .saturating_sub(x)
            / cw)
            .max(0) as usize;
        for (index, scalar) in chars.take(slots).enumerate() {
            let scalar = if scalar.is_control() {
                '\u{fffd}'
            } else {
                scalar
            };
            sink(Draw {
                clip,
                primitive: Primitive::Glyph {
                    x: x + index as i64 * cw,
                    y,
                    scalar,
                    color,
                },
            });
        }
    }
}

/// Fixed demonstration, not file input or a window. Output is binary P6 PPM.
pub fn preview(output: &mut impl std::io::Write) -> std::io::Result<()> {
    use crate::model::{Command, Selection};
    let font = crate::font::pinned().map_err(std::io::Error::other)?;
    let fixture = || -> Result<Vec<u8>> {
        let mut editor = Editor::default();
        let notes = editor.new_tab()?;
        editor.dispatch(notes, 0, Command::Insert(
            "A small text editor\n\nBitmap text, tabs, and a plain document area.\n\nWindows and Emacs key profiles share the same commands.\nParagraph filling inserts real line breaks; soft wrapping does not.\n\nUnicode scalars: café, naïve, λ.\n\tTabs advance to eight-column stops.\n\nThis is the reference-renderer preview, not a Wayland window.\nOpen, Save, clipboard and spelling adapters come next.\n".into()))?;
        let readme = editor.load_bytes(b"td-editor\n")?;
        editor.select_tab(notes)?;
        let revision = editor.document(notes)?.revision();
        editor.dispatch(
            notes,
            revision,
            Command::Select(Selection {
                anchor: 2,
                caret: 7,
            }),
        )?;
        let geometry = Geometry::new(800, 600, Scale::new(1)?)?;
        let labels = [
            Label {
                tab: notes,
                title: "notes.txt",
            },
            Label {
                tab: readme,
                title: "README.md",
            },
        ];
        let scene = Scene::new(
            &editor,
            geometry,
            View::default(),
            &labels,
            Profile::Windows,
        )?;
        let mut pixels = vec![0; 800 * 600 * 4];
        Raster::new(&mut pixels, &font, geometry, 800 * 4)?.paint(&scene, geometry.bounds())?;
        Ok(pixels)
    };
    let pixels = fixture().map_err(std::io::Error::other)?;
    output.write_all(b"P6\n800 600\n255\n")?;
    let mut row = Vec::with_capacity(800 * 3);
    for source in pixels.as_chunks::<{ 800 * 4 }>().0 {
        row.clear();
        for [blue, green, red, _] in source.as_chunks::<4>().0 {
            row.extend_from_slice(&[*red, *green, *blue]);
        }
        output.write_all(&row)?;
    }
    Ok(())
}
