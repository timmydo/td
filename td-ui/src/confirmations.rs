//! Revision-bound confirmation with a scrollable immutable request.

use crate::chrome::{Item, List, Panel, Row, ROW};
use crate::raster::{Draw, Primitive, Rect, Surface, BORDER};
use crate::CELL_WIDTH;

pub const DETAILS: usize = 256;
pub const DETAIL_BYTES: usize = 4096;
pub const TEXT_BYTES: usize = 1024 * 1024;
pub const WRAPPED_ROWS: usize = 65536;
pub const LABEL_BYTES: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Limit,
    InvalidText,
    InvalidSurface,
    NoRoom,
    Allocation,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Limit => "confirmation limit exceeded",
            Self::InvalidText => "invalid confirmation text",
            Self::InvalidSurface => "invalid confirmation surface",
            Self::NoRoom => "surface cannot show the confirmation",
            Self::Allocation => "confirmation allocation failed",
        })
    }
}
impl std::error::Error for Error {}

#[derive(Debug)]
pub struct Model<A, R> {
    title: String,
    confirm: String,
    details: Vec<String>,
    action: A,
    revision: R,
}
impl<A: Copy, R: Copy + Eq> Model<A, R> {
    pub fn new(
        title: &str,
        confirm: &str,
        details: &[&str],
        action: A,
        revision: R,
    ) -> Result<Self, Error> {
        if details.len() > DETAILS || title.len() > LABEL_BYTES || confirm.len() > LABEL_BYTES {
            return Err(Error::Limit);
        }
        if title.is_empty() || confirm.is_empty() || details.is_empty() {
            return Err(Error::InvalidText);
        }
        let mut total = 0usize;
        for text in details {
            if text.len() > DETAIL_BYTES {
                return Err(Error::Limit);
            }
            total = total.checked_add(text.len()).ok_or(Error::Limit)?;
            if total > TEXT_BYTES {
                return Err(Error::Limit);
            }
        }
        if std::iter::once(&title)
            .chain(std::iter::once(&confirm))
            .chain(details.iter())
            .any(|text| text.chars().any(char::is_control))
        {
            return Err(Error::InvalidText);
        }
        let mut captured = Vec::new();
        captured
            .try_reserve_exact(details.len())
            .map_err(|_| Error::Allocation)?;
        for text in details {
            captured.push(copy_text(text)?);
        }
        Ok(Self {
            title: copy_text(title)?,
            confirm: copy_text(confirm)?,
            details: captured,
            action,
            revision,
        })
    }
    pub fn revision(&self) -> R {
        self.revision
    }
}

fn copy_text(text: &str) -> Result<String, Error> {
    let mut owned = String::new();
    owned
        .try_reserve_exact(text.len())
        .map_err(|_| Error::Allocation)?;
    owned.push_str(text);
    Ok(owned)
}

#[derive(Debug)]
struct Line {
    detail: usize,
    range: std::ops::Range<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Focus {
    Details,
    Cancel,
    Confirm,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Key {
    Tab,
    BackTab,
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    Activate,
    Escape,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Event {
    Key { key: Key, repeated: bool },
    Press { x: i64, y: i64 },
    Release { x: i64, y: i64 },
    Move { x: i64, y: i64 },
    Wheel { x: i64, y: i64, rows: isize },
    FocusLost,
    Other,
    Resize { surface: Surface, rect: Rect },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Choice<A> {
    Confirmed(A),
    Cancelled,
    Stale,
    Unavailable(Error),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome<A, F> {
    Ignored,
    Consumed,
    Changed,
    Closed {
        choice: Choice<A>,
        restore_focus: Option<F>,
    },
}

#[derive(Debug)]
pub struct Controller<A, R, F> {
    model: Model<A, R>,
    surface: Surface,
    rect: Rect,
    title: Panel,
    details: List,
    actions: Panel,
    wrapped: Vec<Line>,
    first: usize,
    selected: usize,
    focus: Focus,
    armed: Option<Focus>,
    prior_focus: Option<F>,
    open: bool,
}

impl<A: Copy, R: Copy + Eq, F: Copy> Controller<A, R, F> {
    pub fn new(
        model: Model<A, R>,
        surface: Surface,
        rect: Rect,
        prior_focus: Option<F>,
    ) -> Result<Self, Error> {
        let (title, details, actions, wrapped) = Self::layout(&model, surface, rect)?;
        Ok(Self {
            model,
            surface,
            rect,
            title,
            details,
            actions,
            wrapped,
            first: 0,
            selected: 0,
            focus: Focus::Cancel,
            armed: None,
            prior_focus,
            open: true,
        })
    }
    fn layout(
        model: &Model<A, R>,
        surface: Surface,
        rect: Rect,
    ) -> Result<(Panel, List, Panel, Vec<Line>), Error> {
        surface.check().map_err(|_| Error::InvalidSurface)?;
        if rect.intersection(surface.bounds()) != Some(rect) {
            return Err(Error::NoRoom);
        }
        let scale = surface.scale.value();
        let row = ROW * scale;
        if (rect.height as usize) < 4 * row {
            return Err(Error::NoRoom);
        }
        let columns = (rect.width as usize / (CELL_WIDTH * scale)).saturating_sub(4);
        if columns < 16
            || model.title.chars().count() > columns
            || model.confirm.chars().count() > columns
        {
            return Err(Error::NoRoom);
        }
        let title = Panel::within(
            surface,
            Rect {
                height: row as u32,
                ..rect
            },
        )
        .ok_or(Error::NoRoom)?;
        let action_y = rect.y + i64::from(rect.height) - (2 * row) as i64;
        let actions = Panel::within(
            surface,
            Rect {
                y: action_y,
                height: (2 * row) as u32,
                ..rect
            },
        )
        .ok_or(Error::NoRoom)?;
        let details = List::new(
            surface,
            Rect {
                y: rect.y + row as i64,
                height: rect.height - (3 * row) as u32,
                ..rect
            },
        )
        .ok_or(Error::NoRoom)?;
        let columns = (details.body().width as usize / (CELL_WIDTH * scale)).saturating_sub(4);
        if columns == 0 {
            return Err(Error::NoRoom);
        }
        let count = model.details.iter().try_fold(0usize, |count, text| {
            count
                .checked_add(text.chars().count().max(1).div_ceil(columns))
                .ok_or(Error::Limit)
        })?;
        if count > WRAPPED_ROWS {
            return Err(Error::NoRoom);
        }
        let mut wrapped = Vec::new();
        wrapped
            .try_reserve_exact(count)
            .map_err(|_| Error::Allocation)?;
        for (detail, text) in model.details.iter().enumerate() {
            let mut start = 0;
            let mut width = 0;
            for (offset, _) in text.char_indices() {
                if width == columns {
                    wrapped.push(Line {
                        detail,
                        range: start..offset,
                    });
                    start = offset;
                    width = 0;
                }
                width += 1;
            }
            wrapped.push(Line {
                detail,
                range: start..text.len(),
            });
        }
        Ok((title, details, actions, wrapped))
    }
    pub fn is_open(&self) -> bool {
        self.open
    }
    pub fn prior_focus(&self) -> Option<F> {
        self.prior_focus
    }
    pub fn focus(&self) -> Focus {
        self.focus
    }
    pub fn rect(&self) -> Rect {
        self.rect
    }
    pub fn details_rect(&self) -> Rect {
        self.details.rect()
    }
    pub fn action_rect(&self, focus: Focus) -> Option<Rect> {
        match focus {
            Focus::Cancel => self.actions.row(0),
            Focus::Confirm => self.actions.row(1),
            Focus::Details => None,
        }
    }
    pub fn detail_rows(&self) -> usize {
        self.wrapped.len()
    }
    pub fn detail_text(&self, row: usize) -> Option<&str> {
        let line = self.wrapped.get(row)?;
        self.model.details.get(line.detail)?.get(line.range.clone())
    }
    pub fn first(&self) -> usize {
        self.first
    }
    fn hit_action(&self, x: i64, y: i64) -> Option<Focus> {
        match self.actions.hit(x, y) {
            Some(0) => Some(Focus::Cancel),
            Some(1) => Some(Focus::Confirm),
            _ => None,
        }
    }
    fn close(&mut self, choice: Choice<A>, prior_focus_exists: bool) -> Outcome<A, F> {
        self.open = false;
        self.armed = None;
        Outcome::Closed {
            choice,
            restore_focus: if prior_focus_exists {
                self.prior_focus
            } else {
                None
            },
        }
    }
    fn activate(&mut self, focus: Focus, prior_focus_exists: bool) -> Outcome<A, F> {
        match focus {
            Focus::Cancel => self.close(Choice::Cancelled, prior_focus_exists),
            Focus::Confirm => self.close(Choice::Confirmed(self.model.action), prior_focus_exists),
            Focus::Details => Outcome::Consumed,
        }
    }
    pub fn event(
        &mut self,
        revision: Option<R>,
        prior_focus_exists: bool,
        event: Event,
    ) -> Outcome<A, F> {
        if !self.open {
            return Outcome::Ignored;
        }
        if event != Event::FocusLost && revision != Some(self.model.revision) {
            return self.close(Choice::Stale, prior_focus_exists);
        }
        match event {
            Event::Resize { surface, rect } => {
                self.armed = None;
                self.focus = Focus::Cancel;
                let (title, details, actions, wrapped) =
                    match Self::layout(&self.model, surface, rect) {
                        Ok(layout) => layout,
                        Err(error) => {
                            return self.close(Choice::Unavailable(error), prior_focus_exists)
                        }
                    };
                let relocated = |old: Option<&Line>| {
                    old.and_then(|old| {
                        wrapped.iter().position(|line| {
                            line.detail == old.detail
                                && (line.range.contains(&old.range.start)
                                    || line.range == old.range)
                        })
                    })
                    .unwrap_or(0)
                };
                let selected = relocated(self.wrapped.get(self.selected));
                let first = relocated(self.wrapped.get(self.first));
                self.surface = surface;
                self.rect = rect;
                self.title = title;
                self.details = details;
                self.actions = actions;
                self.wrapped = wrapped;
                self.first = details.reveal(self.wrapped.len(), selected, first);
                self.selected = selected;
                Outcome::Changed
            }
            Event::Key { repeated: true, .. } => {
                self.armed = None;
                Outcome::Consumed
            }
            Event::Key {
                key,
                repeated: false,
            } => {
                self.armed = None;
                match key {
                    Key::Escape => return self.close(Choice::Cancelled, prior_focus_exists),
                    Key::Activate => return self.activate(self.focus, prior_focus_exists),
                    Key::Tab => {
                        self.focus = match self.focus {
                            Focus::Details => Focus::Cancel,
                            Focus::Cancel => Focus::Confirm,
                            Focus::Confirm => Focus::Details,
                        }
                    }
                    Key::BackTab => {
                        self.focus = match self.focus {
                            Focus::Details => Focus::Confirm,
                            Focus::Cancel => Focus::Details,
                            Focus::Confirm => Focus::Cancel,
                        }
                    }
                    _ if self.focus != Focus::Details => return Outcome::Consumed,
                    Key::Up => self.selected = self.selected.saturating_sub(1),
                    Key::Down => self.selected = (self.selected + 1).min(self.wrapped.len() - 1),
                    Key::PageUp => {
                        self.selected = self.selected.saturating_sub(self.details.rows())
                    }
                    Key::PageDown => {
                        self.selected =
                            (self.selected + self.details.rows()).min(self.wrapped.len() - 1)
                    }
                    Key::Home => self.selected = 0,
                    Key::End => self.selected = self.wrapped.len() - 1,
                }
                self.first = self
                    .details
                    .reveal(self.wrapped.len(), self.selected, self.first);
                Outcome::Changed
            }
            Event::Press { x, y } => {
                self.armed = self.hit_action(x, y);
                if self.armed.is_some() {
                    return Outcome::Changed;
                }
                if let Some(row) = self
                    .details
                    .hit(x, y)
                    .map(|row| self.first + row)
                    .filter(|row| *row < self.wrapped.len())
                {
                    self.focus = Focus::Details;
                    self.selected = row;
                    return Outcome::Changed;
                }
                Outcome::Consumed
            }
            Event::Release { x, y } => {
                let armed = self.armed.take();
                if let Some(action) = armed.filter(|a| Some(*a) == self.hit_action(x, y)) {
                    return self.activate(action, prior_focus_exists);
                }
                Outcome::Consumed
            }
            Event::Move { x, y } => {
                if self.armed.is_some() && self.armed != self.hit_action(x, y) {
                    self.armed = None;
                }
                Outcome::Consumed
            }
            Event::Wheel { x, y, rows } => {
                self.armed = None;
                if !self.details.rect().contains(x, y) {
                    return Outcome::Consumed;
                }
                self.first = self
                    .first
                    .saturating_add_signed(rows)
                    .min(self.wrapped.len().saturating_sub(self.details.rows()));
                self.selected = self.selected.clamp(
                    self.first,
                    (self.first + self.details.rows()).min(self.wrapped.len()) - 1,
                );
                Outcome::Changed
            }
            Event::Other => {
                self.armed = None;
                Outcome::Consumed
            }
            Event::FocusLost => self.close(Choice::Cancelled, prior_focus_exists),
        }
    }
    pub fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        if !self.open {
            return;
        }
        self.title.emit(
            [Row {
                label: &self.model.title,
                shortcut: "",
                enabled: true,
                checked: false,
            }],
            usize::MAX,
            damage,
            sink,
        );
        self.details.emit(
            (self.first..self.wrapped.len())
                .filter_map(|row| self.detail_text(row))
                .map(|text| Item {
                    label: text,
                    meta: "",
                    enabled: true,
                    marked: false,
                }),
            self.first,
            if self.focus == Focus::Details {
                self.selected
            } else {
                usize::MAX
            },
            self.wrapped.len(),
            damage,
            sink,
        );
        self.actions.emit(
            [
                Row {
                    label: "Cancel",
                    shortcut: "",
                    enabled: true,
                    checked: false,
                },
                Row {
                    label: &self.model.confirm,
                    shortcut: "",
                    enabled: true,
                    checked: false,
                },
            ],
            match self.focus {
                Focus::Cancel => 0,
                Focus::Confirm => 1,
                Focus::Details => usize::MAX,
            },
            damage,
            sink,
        );
        let thickness = self.surface.scale.value() as u32;
        for y in [self.details.rect().y, self.actions.rect().y] {
            let rule = Rect {
                x: self.rect.x,
                y: y - i64::from(thickness),
                width: self.rect.width,
                height: thickness,
            };
            if let Some(clip) = rule.intersection(damage) {
                sink(Draw {
                    clip,
                    primitive: Primitive::Fill {
                        rect: rule,
                        color: BORDER,
                    },
                });
            }
        }
    }
}
