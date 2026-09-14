//! The editor's scene: its geometry over td-ui's raster primitives, and
//! the deterministic draw stream a `td_ui::raster::Raster` paints.

use crate::keys::Profile;
use crate::layout::{Affinity, Break, Caret, Layout, Metrics, Position, CELL_HEIGHT, CELL_WIDTH};
use crate::model::{Editor, Limits, TabId};
use crate::{text, Error, Result};
use td_ui::chrome::{self, Bar, Block, Status, Strip};
use td_ui::raster::{
    text_run, Composition, Draw, GlyphStyle, Primitive, Raster, Rect, Scale, Scrollbar, Surface,
    BORDER, CHROME, INACTIVE_SELECTION, INK, LINE_NUMBER, MISSPELLED, PAPER, SELECTED,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Geometry {
    width: usize,
    height: usize,
    scale: Scale,
    gutter_columns: usize,
    prompt_rows: usize,
    horizontal_scrollbar: bool,
}

pub const MAX_PROMPT_ROWS: usize = chrome::BLOCK_ROWS;

pub(crate) const MENU_LABELS: [&str; 5] = ["File", "Edit", "Format", "Help", "Directory"];

impl Default for Geometry {
    fn default() -> Self {
        Self {
            width: 800,
            height: 600,
            scale: Scale::default(),
            gutter_columns: 0,
            prompt_rows: 0,
            horizontal_scrollbar: false,
        }
    }
}

impl Geometry {
    pub fn new(width: usize, height: usize, scale: Scale) -> Result<Self> {
        Surface {
            width,
            height,
            scale,
        }
        .check()?;
        Ok(Self {
            width,
            height,
            scale,
            gutter_columns: 0,
            prompt_rows: 0,
            horizontal_scrollbar: false,
        })
    }
    pub fn dimensions(self) -> (usize, usize) {
        (self.width, self.height)
    }
    pub fn with_horizontal_scrollbar(mut self, enabled: bool) -> Self {
        self.horizontal_scrollbar = enabled;
        self
    }
    fn document_height(self) -> u32 {
        let chrome = 72 + if self.horizontal_scrollbar { 16 } else { 0 };
        self.height
            .saturating_sub(chrome * self.scale.value() + self.prompt().height as usize)
            as u32
    }
    pub fn with_prompt_rows(mut self, rows: usize) -> Result<Self> {
        if rows > MAX_PROMPT_ROWS { return Err(Error::Limit); }
        self.prompt_rows = rows;
        Ok(self)
    }
    pub fn prompt_rows(self) -> usize { self.prompt_rows }
    pub fn prompt(self) -> Rect {
        let scale = self.scale.value();
        let rows = self.prompt_rows.min(self.height.saturating_sub(72 * scale) / (16 * scale));
        Rect { x: 0, y: (24 * scale) as i64, width: self.width as u32,
            height: (rows * 16 * scale) as u32 }
    }
    pub(crate) fn with_line_numbers(mut self, lines: Option<usize>) -> Self {
        self.gutter_columns = lines.map_or(0, |n| n.to_string().len().max(2) + 1);
        self
    }
    pub fn gutter(self) -> Rect {
        let s = self.scale.value();
        Rect {
            x: (8 * s) as i64,
            y: (48 * s + self.prompt().height as usize) as i64,
            width: (self.gutter_columns * CELL_WIDTH * s).min(self.width.saturating_sub(16 * s))
                as u32,
            height: self.document_height(),
        }
    }
    pub fn scale(self) -> Scale {
        self.scale
    }
    pub fn menu(self, index: usize) -> Option<Rect> {
        Bar::new(self.surface(), &MENU_LABELS).header(index)
    }
    /// The surface the scene is laid out for: what `Raster::new` takes and
    /// `Raster::paint` holds a scene to.
    pub fn surface(self) -> Surface {
        Surface {
            width: self.width,
            height: self.height,
            scale: self.scale,
        }
    }
    pub fn bounds(self) -> Rect {
        self.surface().bounds()
    }
    pub fn document(self) -> Rect {
        let s = self.scale.value();
        let gutter = self.gutter();
        Rect {
            x: gutter.x + i64::from(gutter.width),
            y: (48 * s + self.prompt().height as usize) as i64,
            width: self.width.saturating_sub(32 * s + gutter.width as usize) as u32,
            height: self.document_height(),
        }
    }
    pub fn grid(self) -> (usize, usize) {
        let doc = self.document();
        (
            doc.width as usize / (CELL_WIDTH * self.scale.value()),
            doc.height as usize / (CELL_HEIGHT * self.scale.value()),
        )
    }
    pub fn scrollbar(self, total_rows: usize, first_row: usize) -> Option<Scrollbar> {
        let (columns, rows) = self.grid();
        if columns == 0 || rows == 0 {
            return None;
        }
        let s = self.scale.value();
        let document = self.document();
        let track = Rect {
            x: self.width.saturating_sub(16 * s) as i64,
            y: document.y,
            width: (12 * s) as u32,
            height: document.height,
        };
        Some(Scrollbar::new(
            track,
            rows,
            total_rows,
            first_row,
            self.scale,
            false,
        ))
    }
    pub fn horizontal_scrollbar(
        self,
        total_columns: usize,
        first_column: usize,
    ) -> Option<Scrollbar> {
        let (columns, rows) = self.grid();
        if !self.horizontal_scrollbar || columns == 0 || rows == 0 {
            return None;
        }
        let s = self.scale.value();
        let document = self.document();
        let track = Rect {
            x: document.x,
            y: document.y + i64::from(document.height) + (4 * s) as i64,
            width: document.width,
            height: (12 * s) as u32,
        };
        Some(Scrollbar::new(
            track,
            columns,
            total_columns,
            first_column,
            self.scale,
            true,
        ))
    }
    pub fn status(self) -> Rect {
        Status::new(self.surface()).rect()
    }
    /// The tab strip's row, below the menu bar and the minibuffer inset.
    fn strip(self) -> Rect {
        Rect {
            x: 0,
            y: 24 * self.scale.value() as i64 + i64::from(self.prompt().height),
            width: self.width as u32,
            height: (24 * self.scale.value()) as u32,
        }
    }
    pub fn tab_close(self, index: usize, active: usize, count: usize) -> Option<Rect> {
        if count > Limits::default().tabs {
            return None;
        }
        Strip::new(self.surface(), self.strip().y, active, count)?.close(index)
    }
    /// The active tab is always in the strip. Narrow surfaces clip one tab.
    pub fn tab(self, index: usize, active: usize, count: usize) -> Option<Rect> {
        if count > Limits::default().tabs {
            return None;
        }
        Strip::new(self.surface(), self.strip().y, active, count)?.tab(index)
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
    spelling: &'a [std::ops::Range<usize>],
    spelling_status: Option<String>,
    notice: Option<&'a str>,
    scrollbars: [Option<Scrollbar>; 2],
}

impl<'a> Scene<'a> {
    pub fn new(
        editor: &'a Editor,
        geometry: Geometry,
        view: View,
        labels: &'a [Label<'a>],
        profile: Profile,
    ) -> Result<Self> {
        Self::with_metrics(editor, geometry, view, labels, profile, None)
    }

    pub(crate) fn with_metrics(
        editor: &'a Editor,
        geometry: Geometry,
        view: View,
        labels: &'a [Label<'a>],
        profile: Profile,
        cached_metrics: Option<Metrics>,
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
        let mut scrollbars = [None; 2];
        let mut status = String::from("No document");
        if let Some(id) = editor.active() {
            let doc = editor.document(id)?;
            let byte = doc.selection().caret;
            let before = doc.text().get(..byte).ok_or(Error::InvalidPosition)?;
            let line = before.bytes().filter(|b| *b == b'\n').count() + 1;
            let current_line = before.rsplit_once('\n').map_or(before, |(_, tail)| tail);
            let column = text::column(current_line) + 1;
            status = format!(
                "Ln {line}, Col {column}   {}   Fill:{}   {}",
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
            if columns != 0 && rows != 0 {
                let layout = Layout::for_document(doc, columns, view.soft_wrap)?;
                let total = match cached_metrics {
                    Some(metrics) => metrics,
                    None => layout.metrics(),
                };
                scrollbars = [
                    geometry.scrollbar(total.rows, view.origin.row),
                    if view.soft_wrap {
                        None
                    } else {
                        geometry.horizontal_scrollbar(total.columns, view.origin.column)
                    },
                ];
                if view.focused && view.caret_visible {
                    caret = Some(layout.position(Caret {
                        byte,
                        affinity: view.affinity,
                    })?);
                }
            }
        }
        Ok(Self {
            editor,
            geometry,
            view,
            labels,
            status,
            caret,
            spelling: &[],
            spelling_status: None,
            notice: None,
            scrollbars,
        })
    }

    pub(crate) fn spelling(mut self, state: &'a crate::spelling::WindowState) -> Self {
        if self.editor.active().is_none() {
            return self;
        }
        let (status, marks) = state.view(self.editor);
        self.spelling_status = Some(status);
        self.spelling = marks;
        self
    }

    /// Non-modal feedback replaces the status text, never document pixels.
    pub fn notice(mut self, notice: Option<&'a str>) -> Self {
        self.notice = notice.filter(|text| !text.is_empty());
        self
    }

    /// Streams operations: the backend need not allocate a retained scene list.
    pub fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let Some(clip) = damage.intersection(self.geometry.bounds()) else {
            return;
        };
        let fill = |rect: Rect, color, sink: &mut dyn FnMut(Draw)| {
            if let Some(area) = rect.intersection(clip) {
                sink(Draw {
                    clip: area,
                    primitive: Primitive::Fill { rect: area, color },
                });
            }
        };
        fill(self.geometry.bounds(), PAPER, sink);
        let surface = self.geometry.surface();
        // The three bands tile [0, 48*scale + prompt height): the menu bar,
        // the minibuffer inset (filled empty; the window paints its caption),
        // and the tab strip.
        Bar::new(surface, &MENU_LABELS).emit(clip, sink);
        let prompt = self.geometry.prompt();
        let prompt_rows = prompt.height as usize / (CELL_HEIGHT * self.geometry.scale.value());
        if let Some(block) = Block::new(surface, prompt.y, prompt_rows) {
            block.emit("", clip, sink);
        }
        let count = self.editor.tabs().count();
        let active = self
            .editor
            .tabs()
            .position(|(id, _)| Some(id) == self.editor.active())
            .unwrap_or(0);
        if let Some(strip) = Strip::new(surface, self.geometry.strip().y, active, count) {
            let labels = self.labels;
            strip.emit(
                self.editor.tabs().map(|(id, doc)| {
                    let title = labels
                        .iter()
                        .find(|label| label.tab == id)
                        .map_or("Untitled", |label| label.title);
                    (title, doc.dirty())
                }),
                clip,
                sink,
            );
        }
        self.document(clip, sink);
        for bar in self.scrollbars.iter().flatten() {
            fill(bar.track, CHROME, sink);
            fill(
                bar.thumb,
                if bar.enabled() { LINE_NUMBER } else { BORDER },
                sink,
            );
        }
        let status = Status::new(surface);
        if let Some(notice) = self.notice {
            status.emit(notice.chars(), clip, sink);
            return;
        }
        let spelling = if self.editor.active().is_some() {
            self.spelling_status
                .as_deref()
                .unwrap_or("Spelling: not checked")
        } else {
            ""
        };
        // The ordinary status is the editor's metrics line, painted plain
        // through the band's frame; `Status::emit`'s ellipsis is for notices.
        status.frame(clip, sink);
        let rect = status.rect();
        let s = self.geometry.scale.value() as i64;
        self.label(
            self.status
                .chars()
                .chain(if spelling.is_empty() { "" } else { "   " }.chars())
                .chain(spelling.chars()),
            (8 * s, rect.y + 4 * s),
            rect,
            GlyphStyle::medium(INK, CHROME),
            clip,
            sink,
        );
    }

    fn document(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let (columns, rows) = self.geometry.grid();
        if columns == 0 || rows == 0 {
            return;
        }
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
        let gutter = self.geometry.gutter();
        if gutter.width != 0 && gutter.intersection(damage).is_some() {
            let mut number = 1usize;
            let mut first = true;
            for (index, row) in layout
                .rows()
                .take(self.view.origin.row.saturating_add(rows))
                .enumerate()
            {
                if index >= self.view.origin.row && first {
                    let label = number.to_string();
                    let s = self.geometry.scale.value();
                    self.label(
                        label.chars(),
                        (
                            gutter.x + i64::from(gutter.width)
                                - ((label.len() + 1) * CELL_WIDTH * s) as i64,
                            gutter.y + ((index - self.view.origin.row) * CELL_HEIGHT * s) as i64,
                        ),
                        gutter,
                        GlyphStyle::medium(LINE_NUMBER, PAPER),
                        damage,
                        sink,
                    );
                }
                first = row.ending() == Break::Newline;
                if first {
                    number += 1;
                }
            }
        }
        let Some(clip) = self.geometry.document().intersection(damage) else {
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
        let mut mark = 0;
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
                            style: GlyphStyle::medium(
                                if selected && self.view.focused {
                                    PAPER
                                } else {
                                    INK
                                },
                                if selected {
                                    if self.view.focused {
                                        SELECTED
                                    } else {
                                        INACTIVE_SELECTION
                                    }
                                } else {
                                    PAPER
                                },
                            ),
                        },
                    });
                }
                while self
                    .spelling
                    .get(mark)
                    .is_some_and(|range| range.end <= cell.bytes.start)
                {
                    mark += 1;
                }
                if self.spelling.get(mark).is_some_and(|range| {
                    range.start <= cell.bytes.start && cell.bytes.end <= range.end
                }) {
                    sink(Draw {
                        clip,
                        primitive: Primitive::Fill {
                            rect: Rect {
                                x,
                                y: y + ch as i64 - s as i64,
                                width: (cell.width * cw) as u32,
                                height: s as u32,
                            },
                            color: if selected && self.view.focused {
                                PAPER
                            } else {
                                MISSPELLED
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
        origin: (i64, i64),
        bounds: Rect,
        style: GlyphStyle,
        damage: Rect,
        sink: &mut dyn FnMut(Draw),
    ) {
        text_run(self.geometry.scale, chars, origin, bounds, style, damage, sink);
    }
}

impl Composition for Scene<'_> {
    fn surface(&self) -> Surface {
        self.geometry.surface()
    }
    fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        Scene::emit(self, damage, sink);
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
        Raster::new(&mut pixels, &font, geometry.surface(), 800 * 4)?
            .paint(&scene, geometry.bounds())?;
        let rgb = td_ui::raster::rgb(&pixels, geometry.surface(), 800 * 4)?;
        Ok(td_ui::raster::ppm(geometry.surface(), &rgb))
    };
    let ppm = fixture().map_err(std::io::Error::other)?;
    output.write_all(&ppm)?;
    Ok(())
}
