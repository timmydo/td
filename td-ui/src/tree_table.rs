//! Pure tree-table interaction over stable, validated visible row IDs.
use crate::raster::{Rect, Scrollbar, Surface};
pub use crate::tree_table_geometry::{
    CellRect, Error as GeometryError, Geometry, Hit as GeometryHit, GUTTER, INDENT, ROW,
};
pub use crate::tree_table_model::{
    Cell, Column, Error as ModelError, Heading, Model, Row, CELL_BYTES, COLUMNS, COLUMN_WIDTH,
    DEPTH, LABEL_BYTES, MODEL_BYTES, ROWS,
};
pub use crate::tree_table_paint::{Direction, Sort};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Geometry(crate::tree_table_geometry::Error),
    InvalidColumn,
    InvalidWidth,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Geometry(error) => write!(f, "tree table geometry: {error}"),
            Self::InvalidColumn => f.write_str("invalid tree table column"),
            Self::InvalidWidth => f.write_str("invalid tree table column width"),
        }
    }
}
impl std::error::Error for Error {}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Focus {
    None,
    Rows,
    Header(usize),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Key {
    Up,
    Down,
    PageUp,
    PageDown,
    First,
    Last,
    Left,
    Right,
    Activate,
    ScrollLeft,
    ScrollRight,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Event {
    Press { x: i64, y: i64 },
    Move { x: i64, y: i64 },
    Release { x: i64, y: i64 },
    Key { key: Key, repeated: bool },
    Scroll { rows: i64, columns: i64 },
    FocusLost,
    Other,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome<I> {
    Ignored,
    Consumed,
    Selected(I),
    Disclosure { id: I, expanded: bool },
    Sort(usize),
    Activate(I),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Target<I> {
    Row(I),
    Disclosure { id: I, expanded: bool },
    Header(usize),
}
#[derive(Clone, Copy, Debug)]
enum Gesture<I> {
    Canceled,
    Target(Target<I>),
    Scroll {
        bar: Scrollbar,
        grab: i64,
        origin: usize,
    },
}
#[derive(Debug)]
pub struct Controller<I> {
    model: Model<I>,
    surface: Surface,
    rect: Rect,
    widths: [u32; COLUMNS],
    geometry: Option<Geometry>,
    first: usize,
    offset: usize,
    selected: Option<I>,
    focus: Focus,
    gesture: Option<Gesture<I>>,
}
impl<I: Copy + Ord> Controller<I> {
    pub fn new(model: Model<I>, surface: Surface, rect: Rect) -> Result<Self, Error> {
        let mut widths = [0; COLUMNS];
        for (slot, column) in widths.iter_mut().zip(model.columns()) {
            *slot = column.preferred();
        }
        let mut this = Self {
            model,
            surface,
            rect,
            widths,
            geometry: None,
            first: 0,
            offset: 0,
            selected: None,
            focus: Focus::None,
            gesture: None,
        };
        this.layout()?;
        Ok(this)
    }
    pub fn model(&self) -> &Model<I> {
        &self.model
    }
    pub fn geometry(&self) -> Option<Geometry> {
        self.geometry
    }
    pub fn selected(&self) -> Option<I> {
        self.selected
    }
    pub fn captured(&self) -> bool {
        self.gesture.is_some()
    }
    pub fn focus(&self) -> Focus {
        self.focus
    }
    pub fn surface(&self) -> Surface {
        self.surface
    }
    pub fn first_anchor(&self) -> Option<I> {
        self.model.rows().get(self.first).map(|row| row.id)
    }
    pub fn set_focus(&mut self, focus: Focus) -> Result<(), Error> {
        if matches!(focus, Focus::Header(column) if column >= self.model.columns().len()) {
            return Err(Error::InvalidColumn);
        }
        if self.focus != focus {
            self.gesture = None;
            self.focus = focus;
        }
        Ok(())
    }
    fn layout(&mut self) -> Result<(), Error> {
        self.geometry = None;
        let widths = self
            .widths
            .get(..self.model.columns().len())
            .ok_or(Error::InvalidColumn)?;
        self.geometry = Geometry::new(
            self.surface,
            self.rect,
            widths,
            self.model.rows().len(),
            self.first,
            self.offset,
        )
        .map_err(Error::Geometry)?;
        if let Some(layout) = self.geometry {
            self.first = layout.first();
            self.offset = layout.offset();
        }
        Ok(())
    }
    pub fn resize(&mut self, surface: Surface, rect: Rect) -> Result<(), Error> {
        self.gesture = None;
        self.offset = self.offset * surface.scale.value() / self.surface.scale.value();
        self.surface = surface;
        self.rect = rect;
        self.layout()
    }
    pub fn replace(&mut self, model: Model<I>) -> Result<(), Error> {
        self.gesture = None;
        let anchor = self.first_anchor();
        self.selected = self.selected.filter(|id| model.find(*id).is_some());
        self.first = anchor
            .and_then(|id| model.find(id))
            .unwrap_or_else(|| self.first.min(model.rows().len().saturating_sub(1)));
        let same_columns = self.model.columns().len() == model.columns().len()
            && self
                .model
                .columns()
                .iter()
                .zip(model.columns())
                .all(|(a, b)| a.title() == b.title() && a.numeric() == b.numeric());
        for (slot, column) in self.widths.iter_mut().zip(model.columns()) {
            *slot = if same_columns {
                (*slot).max(column.minimum())
            } else {
                column.preferred()
            };
        }
        if matches!(self.focus, Focus::Header(column) if column >= model.columns().len()) {
            self.focus = Focus::Rows;
        }
        self.model = model;
        self.layout()
    }
    pub fn width(&self, column: usize) -> Option<u32> {
        self.model.columns().get(column)?;
        self.widths.get(column).copied()
    }
    pub fn set_width(&mut self, column: usize, width: u32) -> Result<(), Error> {
        self.gesture = None;
        let heading = self
            .model
            .columns()
            .get(column)
            .ok_or(Error::InvalidColumn)?;
        if width < heading.minimum() || width > COLUMN_WIDTH {
            return Err(Error::InvalidWidth);
        }
        *self.widths.get_mut(column).ok_or(Error::InvalidColumn)? = width;
        self.layout()
    }
    pub fn select(&mut self, id: Option<I>, reveal: bool) -> bool {
        self.gesture = None;
        if id.is_some_and(|id| self.model.find(id).is_none()) {
            return false;
        }
        self.selected = id;
        if reveal {
            self.reveal_selected();
        }
        true
    }
    fn reveal_selected(&mut self) {
        let Some(index) = self.selected.and_then(|id| self.model.find(id)) else {
            return;
        };
        let Some(geometry) = self.geometry else {
            self.first = index;
            return;
        };
        if index < self.first {
            self.first = index;
        } else if index >= self.first + geometry.visible() {
            self.first = index + 1 - geometry.visible();
        }
        // All stored inputs were validated; failure still retires stale geometry.
        let _ = self.layout();
    }
    pub fn target(&self, x: i64, y: i64) -> Option<Target<I>> {
        let geometry = self.geometry?;
        match geometry.hit(x, y)? {
            GeometryHit::Header(column) => Some(Target::Header(column)),
            GeometryHit::Cell { row, column } => {
                let item = self.model.rows().get(row)?;
                if column == 0
                    && item.children
                    && geometry
                        .disclosure(row, item.depth)
                        .is_some_and(|rect| rect.contains(x, y))
                {
                    Some(Target::Disclosure {
                        id: item.id,
                        expanded: !item.expanded,
                    })
                } else {
                    Some(Target::Row(item.id))
                }
            }
            _ => None,
        }
    }
    fn apply(&mut self, target: Target<I>) -> Outcome<I> {
        match target {
            Target::Header(column) => {
                self.focus = Focus::Rows;
                Outcome::Sort(column)
            }
            Target::Row(id) => {
                self.focus = Focus::Rows;
                self.selected = Some(id);
                Outcome::Selected(id)
            }
            Target::Disclosure { id, expanded } => {
                self.focus = Focus::Rows;
                self.selected = Some(id);
                Outcome::Disclosure { id, expanded }
            }
        }
    }
    fn move_scroll(&mut self, bar: Scrollbar, grab: i64, origin: usize, x: i64, y: i64) {
        let next = bar.position_at(bar.coordinate(x, y), grab, origin);
        if bar.horizontal() {
            self.offset = next;
        } else {
            self.first = next;
        }
        let _ = self.layout();
    }
    fn scroll(&mut self, rows: i64, columns: i64) {
        self.first = (self.first as i128 + i128::from(rows))
            .clamp(0, self.model.rows().len() as i128) as usize;
        self.offset = (self.offset as i128
            + i128::from(columns) * 16 * self.surface.scale.value() as i128)
            .clamp(0, (COLUMNS as u32 * COLUMN_WIDTH * 4) as i128) as usize;
        let _ = self.layout();
    }
    fn key(&mut self, key: Key) -> Outcome<I> {
        if self.focus == Focus::None {
            return Outcome::Ignored;
        }
        if matches!(key, Key::ScrollLeft | Key::ScrollRight) {
            self.scroll(0, if key == Key::ScrollLeft { -1 } else { 1 });
            return Outcome::Consumed;
        }
        if let Focus::Header(column) = self.focus {
            let next = match key {
                Key::Left => column.saturating_sub(1),
                Key::Right => (column + 1).min(self.model.columns().len() - 1),
                Key::First => 0,
                Key::Last => self.model.columns().len() - 1,
                Key::Activate => return Outcome::Sort(column),
                _ => return Outcome::Ignored,
            };
            self.focus = Focus::Header(next);
            // Reveal the complete heading whenever its width fits the viewport.
            let scale = self.surface.scale.value();
            let start = self
                .widths
                .iter()
                .take(next)
                .map(|v| *v as usize * scale)
                .sum::<usize>();
            let end = start + self.widths.get(next).copied().unwrap_or(0) as usize * scale;
            if let Some(g) = self.geometry {
                if start < self.offset {
                    self.offset = start;
                } else if end > self.offset + g.body().width as usize {
                    self.offset = end.saturating_sub(g.body().width as usize).min(start);
                }
            }
            let _ = self.layout();
            return Outcome::Consumed;
        }
        let count = self.model.rows().len();
        if count == 0 {
            return Outcome::Ignored;
        }
        let current = self.selected.and_then(|id| self.model.find(id));
        let index = current.unwrap_or(self.first.min(count - 1));
        let page = self
            .geometry
            .map(|g| g.visible().saturating_sub(1).max(1))
            .unwrap_or(1);
        let next = match key {
            Key::Up => {
                if current.is_some() {
                    index.saturating_sub(1)
                } else {
                    index
                }
            }
            Key::Down => {
                if current.is_some() {
                    (index + 1).min(count - 1)
                } else {
                    index
                }
            }
            Key::PageUp => index.saturating_sub(page),
            Key::PageDown => index.saturating_add(page).min(count - 1),
            Key::First => 0,
            Key::Last => count - 1,
            Key::Activate => {
                return current
                    .and_then(|index| self.model.rows().get(index))
                    .map(|row| Outcome::Activate(row.id))
                    .unwrap_or(Outcome::Ignored)
            }
            Key::Left | Key::Right => {
                let Some(row) = current.and_then(|index| self.model.rows().get(index)) else {
                    return Outcome::Ignored;
                };
                if row.children && row.expanded == (key == Key::Left) {
                    return Outcome::Disclosure {
                        id: row.id,
                        expanded: !row.expanded,
                    };
                }
                if key == Key::Left {
                    self.model.parent(index).unwrap_or(index)
                } else {
                    self.model.first_child(index).unwrap_or(index)
                }
            }
            _ => return Outcome::Ignored,
        };
        let Some(id) = self.model.rows().get(next).map(|row| row.id) else {
            return Outcome::Ignored;
        };
        let changed = self.selected != Some(id);
        self.selected = Some(id);
        self.reveal_selected();
        if changed {
            Outcome::Selected(id)
        } else {
            Outcome::Consumed
        }
    }
    pub fn event(&mut self, event: Event) -> Outcome<I> {
        match event {
            Event::Press { x, y } => {
                self.gesture = None;
                let Some(g) = self.geometry else {
                    return Outcome::Ignored;
                };
                let bar = match g.hit(x, y) {
                    Some(GeometryHit::Vertical) => Some(g.vertical()),
                    Some(GeometryHit::Horizontal) => g.horizontal(),
                    _ => None,
                };
                if let Some(bar) = bar {
                    if !bar.enabled() {
                        return Outcome::Consumed;
                    }
                    let coordinate = bar.coordinate(x, y);
                    let origin = if bar.horizontal() {
                        self.offset
                    } else {
                        self.first
                    };
                    let grab = if bar.thumb.contains(x, y) {
                        coordinate - bar.coordinate(bar.thumb.x, bar.thumb.y)
                    } else {
                        i64::from(if bar.horizontal() {
                            bar.thumb.width
                        } else {
                            bar.thumb.height
                        }) / 2
                    };
                    self.gesture = Some(Gesture::Scroll { bar, grab, origin });
                    self.move_scroll(bar, grab, origin, x, y);
                    return Outcome::Consumed;
                }
                if let Some(target) = self.target(x, y) {
                    self.focus = Focus::Rows;
                    self.gesture = Some(Gesture::Target(target));
                    Outcome::Consumed
                } else {
                    Outcome::Ignored
                }
            }
            Event::Move { x, y } => match self.gesture {
                Some(Gesture::Scroll { bar, grab, origin }) => {
                    self.move_scroll(bar, grab, origin, x, y);
                    Outcome::Consumed
                }
                Some(Gesture::Target(target)) => {
                    if self.target(x, y) != Some(target) {
                        self.gesture = Some(Gesture::Canceled);
                    }
                    Outcome::Consumed
                }
                Some(Gesture::Canceled) => Outcome::Consumed,
                None => Outcome::Ignored,
            },
            Event::Release { x, y } => match self.gesture.take() {
                Some(Gesture::Scroll { bar, grab, origin }) => {
                    self.move_scroll(bar, grab, origin, x, y);
                    Outcome::Consumed
                }
                Some(Gesture::Target(target)) if self.target(x, y) == Some(target) => {
                    self.apply(target)
                }
                Some(_) => Outcome::Consumed,
                None => Outcome::Ignored,
            },
            Event::Key { key, repeated } => {
                self.gesture = None;
                if repeated && key == Key::Activate {
                    if self.focus == Focus::None {
                        Outcome::Ignored
                    } else {
                        Outcome::Consumed
                    }
                } else {
                    self.key(key)
                }
            }
            Event::Scroll { rows, columns } => {
                self.gesture = None;
                if self.geometry.is_none() {
                    return Outcome::Ignored;
                }
                self.scroll(rows, columns);
                Outcome::Consumed
            }
            Event::FocusLost => {
                self.gesture = None;
                self.focus = Focus::None;
                Outcome::Consumed
            }
            Event::Other => {
                self.gesture = None;
                Outcome::Ignored
            }
        }
    }
}
