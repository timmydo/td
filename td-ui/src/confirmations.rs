//! Revision-bound confirmation with a scrollable immutable request.
//! The consumer routes its chords in; `Key::from_chord` is the default
//! set.

use crate::chrome::{Item, List, Panel, Row, ROW};
use crate::raster::{Draw, Rect, Surface};
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
    /// A second action shown between Cancel and Confirm, such as Discard.
    alternate: Option<(String, A)>,
    /// A further action shown after the alternate, before Confirm.
    further: Option<(String, A)>,
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
            alternate: None,
            further: None,
            revision,
        })
    }
    /// Adds a second action, labelled as a confirmation is, between Cancel
    /// and Confirm: a three-way choice such as Save, Discard or Cancel. A
    /// second call replaces the first.
    pub fn with_alternate(mut self, label: &str, action: A) -> Result<Self, Error> {
        self.alternate = Some((action_label(label)?, action));
        Ok(self)
    }
    /// Adds a further action, labelled as the alternate is, after the
    /// alternate and before Confirm: a four-way choice such as Cancel, No,
    /// Yes or Yes to all. A second call replaces the first.
    pub fn with_further(mut self, label: &str, action: A) -> Result<Self, Error> {
        self.further = Some((action_label(label)?, action));
        Ok(self)
    }
    pub fn storage_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.title.capacity()
            + self.confirm.capacity()
            + [&self.alternate, &self.further]
                .into_iter()
                .flatten()
                .map(|(label, _)| label.capacity())
                .sum::<usize>()
            + self.details.capacity() * std::mem::size_of::<String>()
            + self.details.iter().map(String::capacity).sum::<usize>()
    }
    fn actions(&self) -> usize {
        2 + usize::from(self.alternate.is_some()) + usize::from(self.further.is_some())
    }
    pub fn revision(&self) -> R {
        self.revision
    }
}

/// An alternate's or a further action's label, validated as the
/// confirmation's is.
fn action_label(label: &str) -> Result<String, Error> {
    if label.len() > LABEL_BYTES {
        return Err(Error::Limit);
    }
    if label.is_empty() || label.chars().any(char::is_control) {
        return Err(Error::InvalidText);
    }
    copy_text(label)
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
    /// The model's alternate action; never focused without one.
    Alternate,
    /// The model's further action; never focused without one.
    Further,
    Confirm,
}

// Tab order, the detail list included.
const TWO_ACTIONS: [Focus; 3] = [Focus::Details, Focus::Cancel, Focus::Confirm];
const THREE_ACTIONS: [Focus; 4] = [
    Focus::Details,
    Focus::Cancel,
    Focus::Alternate,
    Focus::Confirm,
];
const FURTHER_ACTIONS: [Focus; 4] = [
    Focus::Details,
    Focus::Cancel,
    Focus::Further,
    Focus::Confirm,
];
const FOUR_ACTIONS: [Focus; 5] = [
    Focus::Details,
    Focus::Cancel,
    Focus::Alternate,
    Focus::Further,
    Focus::Confirm,
];
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
impl Key {
    /// The default bindings: Tab and Shift+Tab between the details and
    /// the actions, Up and Down, Page Up and Down, Home and End within
    /// the details, Return or Space to choose the focused action, and
    /// Escape to cancel. A chord it names nothing for is the consumer's
    /// to pass as `Event::Other`, which the open dialog consumes.
    pub fn from_chord(chord: &str) -> Option<Self> {
        match chord {
            "Tab" => Some(Self::Tab),
            "S-Tab" => Some(Self::BackTab),
            "Up" => Some(Self::Up),
            "Down" => Some(Self::Down),
            "PageUp" => Some(Self::PageUp),
            "PageDown" => Some(Self::PageDown),
            "Home" => Some(Self::Home),
            "End" => Some(Self::End),
            "Return" | "Space" | " " => Some(Self::Activate),
            "Escape" => Some(Self::Escape),
            _ => None,
        }
    }
}

/// The open dialog's keys as a key list shows them, `Key::from_chord`'s
/// as `(keys, what)` rows for a consumer's `keys::Section`.
pub const KEYS: &[(&str, &str)] = &[
    ("Tab/S-Tab", "move between the details and the buttons"),
    ("Up/Down", "scroll the details"),
    ("PageUp/PageDown", "scroll the details a page"),
    ("Home/End", "go to the details' start or end"),
    (
        "Return/Space",
        "choose the focused button; Cancel starts focused",
    ),
    ("Escape", "cancel"),
];
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
        let actions = model.actions();
        // The title, at least one detail row and every action.
        if (rect.height as usize) < (2 + actions) * row {
            return Err(Error::NoRoom);
        }
        let columns = (rect.width as usize / (CELL_WIDTH * scale)).saturating_sub(4);
        if columns < 16
            || model.title.chars().count() > columns
            || model.confirm.chars().count() > columns
            || [&model.alternate, &model.further]
                .into_iter()
                .flatten()
                .any(|(label, _)| label.chars().count() > columns)
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
        let action_y = rect.y + i64::from(rect.height) - (actions * row) as i64;
        let actions_panel = Panel::within(
            surface,
            Rect {
                y: action_y,
                height: (actions * row) as u32,
                ..rect
            },
        )
        .ok_or(Error::NoRoom)?;
        let details = List::new(
            surface,
            Rect {
                y: rect.y + row as i64,
                height: rect.height - ((1 + actions) * row) as u32,
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
        Ok((title, details, actions_panel, wrapped))
    }
    /// Retained text and wrapped-row capacities, excluding allocator bookkeeping.
    pub fn storage_bytes(&self) -> usize {
        std::mem::size_of::<Self>() - std::mem::size_of::<Model<A, R>>()
            + self.model.storage_bytes()
            + self.wrapped.capacity() * std::mem::size_of::<Line>()
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
        self.action_row(focus).and_then(|row| self.actions.row(row))
    }
    // Rows top to bottom: Cancel, the alternate if any, the further
    // action if any, Confirm.
    fn action_row(&self, focus: Focus) -> Option<usize> {
        match focus {
            Focus::Cancel => Some(0),
            Focus::Alternate => self.model.alternate.as_ref().map(|_| 1),
            Focus::Further => self
                .model
                .further
                .as_ref()
                .map(|_| 1 + usize::from(self.model.alternate.is_some())),
            Focus::Confirm => Some(self.model.actions() - 1),
            Focus::Details => None,
        }
    }
    fn order(&self) -> &'static [Focus] {
        match (self.model.alternate.is_some(), self.model.further.is_some()) {
            (false, false) => &TWO_ACTIONS,
            (true, false) => &THREE_ACTIONS,
            (false, true) => &FURTHER_ACTIONS,
            (true, true) => &FOUR_ACTIONS,
        }
    }
    fn cycled(&self, forward: bool) -> Focus {
        let order = self.order();
        let at = order.iter().position(|f| *f == self.focus).unwrap_or(0);
        let next = if forward {
            (at + 1) % order.len()
        } else {
            (at + order.len() - 1) % order.len()
        };
        order.get(next).copied().unwrap_or(Focus::Cancel)
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
        let row = self.actions.hit(x, y)?;
        [
            Focus::Cancel,
            Focus::Alternate,
            Focus::Further,
            Focus::Confirm,
        ]
        .into_iter()
        .find(|focus| self.action_row(*focus) == Some(row))
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
    /// Closes with an alternate's or a further action's ID; one the model
    /// lacks consumes the input.
    fn choose(&mut self, action: Option<A>, prior_focus_exists: bool) -> Outcome<A, F> {
        match action {
            Some(action) => self.close(Choice::Confirmed(action), prior_focus_exists),
            None => Outcome::Consumed,
        }
    }
    fn activate(&mut self, focus: Focus, prior_focus_exists: bool) -> Outcome<A, F> {
        match focus {
            Focus::Cancel => self.close(Choice::Cancelled, prior_focus_exists),
            Focus::Confirm => self.close(Choice::Confirmed(self.model.action), prior_focus_exists),
            Focus::Alternate => {
                let action = self.model.alternate.as_ref().map(|(_, action)| *action);
                self.choose(action, prior_focus_exists)
            }
            Focus::Further => {
                let action = self.model.further.as_ref().map(|(_, action)| *action);
                self.choose(action, prior_focus_exists)
            }
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
                    Key::Tab => self.focus = self.cycled(true),
                    Key::BackTab => self.focus = self.cycled(false),
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
        let row = |label| Row {
            label,
            shortcut: "",
            enabled: true,
            checked: false,
        };
        let between = [&self.model.alternate, &self.model.further]
            .into_iter()
            .flatten()
            .map(|(label, _)| row(label.as_str()));
        self.actions.emit(
            std::iter::once(row("Cancel"))
                .chain(between)
                .chain(std::iter::once(row(&self.model.confirm))),
            self.action_row(self.focus).unwrap_or(usize::MAX),
            damage,
            sink,
        );
    }
}
