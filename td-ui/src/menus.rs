//! Bounded menu data and interaction over the shared bar and panel painters.
//! The caller supplies a revision and receives an action; this module performs
//! no application work. Geometry is identical for painting and hit testing.

use crate::chrome::{Bar, Panel, Row, PANEL_ROWS, PANEL_WIDTH, ROW};
use crate::raster::{Draw, Rect, Surface};
use crate::CELL_WIDTH;

pub const ENTRIES: usize = 256;
pub const PANELS: usize = 8;
pub const LABEL_BYTES: usize = 256;
pub const SHORTCUT_BYTES: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Limit,
    InvalidModel,
    InvalidEntry,
    InvalidSurface,
    NoRoom,
    Allocation,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Limit => "menu limit exceeded",
            Self::InvalidModel => "invalid menu model",
            Self::InvalidEntry => "invalid menu entry",
            Self::InvalidSurface => "invalid menu surface",
            Self::NoRoom => "surface cannot show the menu",
            Self::Allocation => "menu allocation failed",
        })
    }
}
impl std::error::Error for Error {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Bar,
    Context,
}

#[derive(Clone, Copy, Debug)]
pub enum Item<A> {
    Action(A),
    Submenu,
}

/// Nodes are in parent-before-child order. A parent is an index into this
/// model and must be a submenu. Bar roots are its labelled headers; context
/// roots are the rows of its first panel. Action IDs belong to the consumer.
#[derive(Clone, Copy, Debug)]
pub struct Node<'a, A> {
    pub parent: Option<usize>,
    pub row: Row<'a>,
    pub item: Item<A>,
}

#[derive(Clone, Debug)]
pub struct Model<'a, A, R> {
    kind: Kind,
    revision: R,
    nodes: Vec<Node<'a, A>>,
    roots: Vec<usize>,
    labels: Vec<&'a str>,
}

impl<'a, A: Copy, R: Copy + Eq> Model<'a, A, R> {
    pub fn new(kind: Kind, revision: R, nodes: &[Node<'a, A>]) -> Result<Self, Error> {
        if nodes.is_empty() {
            return Err(Error::InvalidModel);
        }
        if nodes.len() > ENTRIES {
            return Err(Error::Limit);
        }
        let mut depths = [0usize; ENTRIES];
        for (index, node) in nodes.iter().enumerate() {
            if node.row.label.len() > LABEL_BYTES || node.row.shortcut.len() > SHORTCUT_BYTES {
                return Err(Error::Limit);
            }
            if node.row.label.is_empty()
                || node.row.label.chars().any(char::is_control)
                || node.row.shortcut.chars().any(char::is_control)
            {
                return Err(Error::InvalidModel);
            }
            let depth = if let Some(parent) = node.parent {
                if parent >= index
                    || !nodes
                        .get(parent)
                        .is_some_and(|n| matches!(n.item, Item::Submenu))
                {
                    return Err(Error::InvalidModel);
                }
                depths.get(parent).copied().ok_or(Error::InvalidModel)? + 1
            } else {
                if kind == Kind::Bar && !matches!(node.item, Item::Submenu) {
                    return Err(Error::InvalidModel);
                }
                0
            };
            let panels = depth + usize::from(kind == Kind::Context);
            if panels > PANELS {
                return Err(Error::Limit);
            }
            *depths.get_mut(index).ok_or(Error::Limit)? = depth;
            if matches!(node.item, Item::Submenu) && !nodes.iter().any(|n| n.parent == Some(index))
            {
                return Err(Error::InvalidModel);
            }
        }
        let mut model = Self {
            kind,
            revision,
            nodes: Vec::new(),
            roots: Vec::new(),
            labels: Vec::new(),
        };
        model
            .nodes
            .try_reserve_exact(nodes.len())
            .map_err(|_| Error::Allocation)?;
        model
            .roots
            .try_reserve_exact(nodes.len())
            .map_err(|_| Error::Allocation)?;
        model
            .labels
            .try_reserve_exact(nodes.len())
            .map_err(|_| Error::Allocation)?;
        model.nodes.extend_from_slice(nodes);
        for (index, node) in nodes.iter().enumerate().filter(|(_, n)| n.parent.is_none()) {
            model.roots.push(index);
            model.labels.push(node.row.label);
        }
        Ok(model)
    }

    /// Owned capacities; labels are borrowed and charged by their owner.
    pub fn storage_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.nodes.capacity() * std::mem::size_of::<Node<'a, A>>()
            + self.roots.capacity() * std::mem::size_of::<usize>()
            + self.labels.capacity() * std::mem::size_of::<&str>()
    }
    pub fn revision(&self) -> R {
        self.revision
    }
    pub fn kind(&self) -> Kind {
        self.kind
    }
    pub fn node(&self, index: usize) -> Option<&Node<'a, A>> {
        self.nodes.get(index)
    }
    fn children(&self, parent: Option<usize>) -> impl Iterator<Item = (usize, &Node<'a, A>)> {
        self.nodes
            .iter()
            .enumerate()
            .filter(move |(_, n)| n.parent == parent)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Fit {
    /// Preserve document menus' complete panel above one status row.
    Complete,
    /// Fit to the surface, scrolling rows and replacing a parent with Back
    /// when two panels cannot fit side by side.
    Adaptive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    Activate,
    Escape,
    Dismiss,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Event {
    Key { key: Key, repeated: bool },
    Move { x: i64, y: i64 },
    Press { x: i64, y: i64 },
    Release,
    Other,
    Wheel { x: i64, y: i64, rows: isize },
    FocusLost,
    Resize(Surface),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome<A> {
    Ignored,
    Consumed,
    Changed,
    Dismissed,
    Stale,
    Activated(A),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Selection {
    None,
    Back,
    Node(usize),
}

#[derive(Clone, Copy, Debug)]
struct Level {
    parent: Option<usize>,
    panel: Panel,
    first: usize,
    selected: Selection,
    back: bool,
}

/// An immutable model plus its open path. An event's current revision must
/// equal the model's; stale input closes every panel without an activation.
/// Replacing the model means constructing a new, closed controller.
#[derive(Clone, Debug)]
pub struct Controller<'a, A, R> {
    model: Model<'a, A, R>,
    surface: Surface,
    fit: Fit,
    group: Option<usize>,
    levels: Vec<Level>,
}

impl<'a, A: Copy, R: Copy + Eq> Controller<'a, A, R> {
    pub fn new(model: Model<'a, A, R>, surface: Surface, fit: Fit) -> Result<Self, Error> {
        Surface::new(surface.width, surface.height, surface.scale)
            .map_err(|_| Error::InvalidSurface)?;
        let mut levels = Vec::new();
        levels
            .try_reserve_exact(PANELS)
            .map_err(|_| Error::Allocation)?;
        Ok(Self {
            model,
            surface,
            fit,
            group: None,
            levels,
        })
    }

    pub fn model(&self) -> &Model<'a, A, R> {
        &self.model
    }
    pub fn surface(&self) -> Surface {
        self.surface
    }
    pub fn is_open(&self) -> bool {
        !self.levels.is_empty()
    }
    /// Index among the bar's headers, not a node index.
    pub fn group(&self) -> Option<usize> {
        self.group
    }
    pub fn depth(&self) -> usize {
        self.levels.len()
    }
    pub fn selection(&self) -> Selection {
        self.levels.last().map_or(Selection::None, |l| l.selected)
    }
    /// Geometry of a visible panel; replacement views hide earlier levels.
    pub fn panel(&self, depth: usize) -> Option<Rect> {
        if depth < self.visible_start() {
            return None;
        }
        self.levels.get(depth).map(|l| l.panel.rect())
    }
    pub fn header_hit(&self, x: i64, y: i64) -> Option<usize> {
        if self.model.kind != Kind::Bar || !self.surface.bounds().contains(x, y) {
            return None;
        }
        Bar::new(self.surface, &self.model.labels).hit(x, y)
    }
    pub fn valid(&self, revision: Option<R>, surface: Surface) -> bool {
        revision == Some(self.model.revision) && surface == self.surface && self.is_open()
    }
    pub fn dismiss(&mut self) {
        self.levels.clear();
        self.group = None;
    }
    pub fn open_bar(&mut self, group: usize) -> Result<(), Error> {
        if self.model.kind != Kind::Bar {
            return Err(Error::InvalidModel);
        }
        let root = *self.model.roots.get(group).ok_or(Error::InvalidEntry)?;
        if !self.model.node(root).is_some_and(|n| n.row.enabled) {
            return Err(Error::InvalidEntry);
        }
        let anchor = Bar::new(self.surface, &self.model.labels)
            .header(group)
            .ok_or(Error::NoRoom)?;
        let level = self.layout(Some(root), anchor, None)?;
        self.levels.clear();
        self.levels.push(level);
        self.group = Some(group);
        Ok(())
    }
    pub fn open_context(&mut self, x: i64, y: i64) -> Result<(), Error> {
        if self.model.kind != Kind::Context {
            return Err(Error::InvalidModel);
        }
        let anchor = Rect {
            x,
            y,
            width: 0,
            height: 0,
        };
        let level = self.layout(None, anchor, None)?;
        self.levels.clear();
        self.levels.push(level);
        self.group = None;
        Ok(())
    }

    fn layout(
        &self,
        parent: Option<usize>,
        anchor: Rect,
        beside: Option<Rect>,
    ) -> Result<Level, Error> {
        let count = self.model.children(parent).count();
        if count == 0 {
            return Err(Error::InvalidModel);
        }
        let scale = self.surface.scale.value();
        let mut back = false;
        let panel = if self.fit == Fit::Complete && beside.is_none() {
            Panel::new(self.surface, anchor, count).ok_or(Error::NoRoom)?
        } else {
            let width = (PANEL_WIDTH * scale).min(self.surface.width);
            if width < 3 * CELL_WIDTH * scale {
                return Err(Error::NoRoom);
            }
            let top = if self.model.kind == Kind::Bar {
                ROW * scale
            } else {
                0
            };
            let room = self.surface.height.saturating_sub(top) / (ROW * scale);
            if room == 0 {
                return Err(Error::NoRoom);
            }
            let (x, y) = if let Some(previous) = beside {
                let height = count.min(PANEL_ROWS).min(room) * ROW * scale;
                let y = anchor
                    .y
                    .clamp(top as i64, (self.surface.height - height) as i64);
                let fits = |x: i64| {
                    let candidate = Rect {
                        x,
                        y,
                        width: width as u32,
                        height: height as u32,
                    };
                    x >= 0
                        && x + width as i64 <= self.surface.width as i64
                        && self
                            .levels
                            .iter()
                            .skip(self.visible_start())
                            .all(|level| candidate.intersection(level.panel.rect()).is_none())
                };
                let right = previous.x + i64::from(previous.width);
                let left = previous.x - width as i64;
                if fits(right) {
                    (right, anchor.y)
                } else if fits(left) {
                    (left, anchor.y)
                } else {
                    back = true;
                    (previous.x, previous.y)
                }
            } else {
                (anchor.x, anchor.y.saturating_add(i64::from(anchor.height)))
            };
            let rows = (count + usize::from(back)).min(PANEL_ROWS).min(room);
            if rows <= usize::from(back) {
                return Err(Error::NoRoom);
            }
            let height = rows * ROW * scale;
            let rect = Rect {
                x: x.clamp(0, (self.surface.width - width) as i64),
                y: y.clamp(
                    top as i64,
                    self.surface.height.saturating_sub(height) as i64,
                ),
                width: width as u32,
                height: height as u32,
            };
            Panel::within(self.surface, rect).ok_or(Error::NoRoom)?
        };
        let mut selected = self
            .model
            .children(parent)
            .find(|(_, n)| n.row.enabled)
            .map_or(Selection::None, |(i, _)| Selection::Node(i));
        if selected == Selection::None && self.fit == Fit::Complete && beside.is_none() {
            selected = self
                .model
                .children(parent)
                .next()
                .map_or(Selection::None, |(index, _)| Selection::Node(index));
        }
        let mut level = Level {
            parent,
            panel,
            first: 0,
            selected,
            back,
        };
        self.reveal(&mut level);
        Ok(level)
    }

    fn reveal(&self, level: &mut Level) {
        let Selection::Node(index) = level.selected else {
            return;
        };
        let Some(position) = self
            .model
            .children(level.parent)
            .position(|(i, _)| i == index)
        else {
            return;
        };
        let visible = level.panel.rows().saturating_sub(usize::from(level.back));
        if position < level.first {
            level.first = position;
        }
        if position >= level.first.saturating_add(visible) {
            level.first = position.saturating_add(1).saturating_sub(visible);
        }
    }
    fn visible_start(&self) -> usize {
        self.levels.iter().rposition(|l| l.back).unwrap_or(0)
    }
    fn hit(&self, x: i64, y: i64) -> Option<(usize, Selection)> {
        for (depth, level) in self
            .levels
            .iter()
            .enumerate()
            .skip(self.visible_start())
            .rev()
        {
            let Some(row) = level.panel.hit(x, y) else {
                continue;
            };
            if level.back && row == 0 {
                return Some((depth, Selection::Back));
            }
            let offset = row.saturating_sub(usize::from(level.back));
            let selection = self
                .model
                .children(level.parent)
                .nth(level.first + offset)
                .map_or(Selection::None, |(i, _)| Selection::Node(i));
            return Some((depth, selection));
        }
        None
    }
    pub fn row_rect(&self, node: usize) -> Option<Rect> {
        for level in self.levels.iter().skip(self.visible_start()).rev() {
            let position = self
                .model
                .children(level.parent)
                .position(|(i, _)| i == node);
            if let Some(position) = position {
                let row = position.checked_sub(level.first)? + usize::from(level.back);
                return level.panel.row(row);
            }
        }
        None
    }
    fn open_child(&mut self, node: usize) -> Result<Outcome<A>, Error> {
        let depth = self.levels.len();
        if depth >= PANELS {
            return Err(Error::Limit);
        }
        let previous = self.levels.last().ok_or(Error::InvalidEntry)?.panel.rect();
        let anchor = self.row_rect(node).ok_or(Error::InvalidEntry)?;
        let level = self.layout(Some(node), anchor, Some(previous))?;
        self.levels.push(level);
        Ok(Outcome::Changed)
    }
    fn pop(&mut self) -> Outcome<A> {
        self.levels.pop();
        if self.levels.is_empty() {
            self.group = None;
            Outcome::Dismissed
        } else {
            Outcome::Changed
        }
    }
    fn activate(&mut self) -> Result<Outcome<A>, Error> {
        match self.selection() {
            Selection::Back => Ok(self.pop()),
            Selection::Node(index) => {
                let node = self.model.node(index).ok_or(Error::InvalidEntry)?;
                if !node.row.enabled {
                    return Ok(Outcome::Consumed);
                }
                match node.item {
                    Item::Submenu => self.open_child(index),
                    Item::Action(action) => {
                        self.dismiss();
                        Ok(Outcome::Activated(action))
                    }
                }
            }
            Selection::None => Ok(Outcome::Consumed),
        }
    }
    fn navigate(&mut self, backward: bool) -> Result<Outcome<A>, Error> {
        let mut level = *self.levels.last().ok_or(Error::InvalidEntry)?;
        let children = self.model.children(level.parent).count();
        let count = children + usize::from(level.back);
        let position = match level.selected {
            Selection::Back => Some(0),
            Selection::Node(index) => self
                .model
                .children(level.parent)
                .position(|(i, _)| i == index)
                .map(|p| p + usize::from(level.back)),
            Selection::None => None,
        };
        let start = position.unwrap_or(if backward { 0 } else { count.saturating_sub(1) });
        for delta in 1..=count {
            let position = if backward {
                (start + count - delta) % count
            } else {
                (start + delta) % count
            };
            let selected = if level.back && position == 0 {
                Selection::Back
            } else {
                let Some((index, node)) = self
                    .model
                    .children(level.parent)
                    .nth(position - usize::from(level.back))
                else {
                    continue;
                };
                if !node.row.enabled {
                    continue;
                }
                Selection::Node(index)
            };
            level.selected = selected;
            self.reveal(&mut level);
            *self.levels.last_mut().ok_or(Error::InvalidEntry)? = level;
            return Ok(Outcome::Changed);
        }
        Ok(Outcome::Consumed)
    }
    fn switch_group(&mut self, backward: bool) -> Result<Outcome<A>, Error> {
        let Some(group) = self.group else {
            return Ok(self.pop());
        };
        let count = self.model.roots.len();
        for delta in 1..=count {
            let next = if backward {
                (group + count - delta) % count
            } else {
                (group + delta) % count
            };
            if self
                .model
                .roots
                .get(next)
                .and_then(|&i| self.model.node(i))
                .is_some_and(|n| n.row.enabled)
            {
                if next == group {
                    return Ok(Outcome::Consumed);
                }
                self.open_bar(next)?;
                return Ok(Outcome::Changed);
            }
        }
        Ok(Outcome::Consumed)
    }

    pub fn event(&mut self, revision: Option<R>, event: Event) -> Result<Outcome<A>, Error> {
        if let Event::Resize(surface) = event {
            Surface::new(surface.width, surface.height, surface.scale)
                .map_err(|_| Error::InvalidSurface)?;
            self.surface = surface;
            let open = self.is_open();
            self.dismiss();
            return Ok(if open {
                Outcome::Dismissed
            } else {
                Outcome::Ignored
            });
        }
        if event == Event::FocusLost {
            let open = self.is_open();
            self.dismiss();
            return Ok(if open {
                Outcome::Dismissed
            } else {
                Outcome::Ignored
            });
        }
        if revision != Some(self.model.revision) {
            let open = self.is_open();
            self.dismiss();
            return Ok(if open {
                Outcome::Stale
            } else {
                Outcome::Ignored
            });
        }
        if let Event::Press { x, y } = event {
            if let Some(group) = self.header_hit(x, y) {
                let enabled = self
                    .model
                    .roots
                    .get(group)
                    .and_then(|&i| self.model.node(i))
                    .is_some_and(|node| node.row.enabled);
                if !enabled {
                    return Ok(if self.is_open() {
                        Outcome::Consumed
                    } else {
                        Outcome::Ignored
                    });
                }
                if self.group == Some(group) && self.is_open() {
                    self.dismiss();
                    return Ok(Outcome::Dismissed);
                }
                self.open_bar(group)?;
                return Ok(Outcome::Changed);
            }
        }
        if !self.is_open() {
            return Ok(Outcome::Ignored);
        }
        match event {
            Event::Key { repeated: true, .. } | Event::Release | Event::Other => {
                Ok(Outcome::Consumed)
            }
            Event::Key {
                key,
                repeated: false,
            } => match key {
                Key::Up | Key::Down => self.navigate(key == Key::Up),
                Key::Activate => self.activate(),
                Key::Escape => Ok(self.pop()),
                Key::Dismiss => {
                    self.dismiss();
                    Ok(Outcome::Dismissed)
                }
                Key::Left => {
                    if self.levels.len() > 1 {
                        Ok(self.pop())
                    } else {
                        self.switch_group(true)
                    }
                }
                Key::Right => {
                    let branch = match self.selection() {
                        Selection::Node(i) => self
                            .model
                            .node(i)
                            .is_some_and(|n| matches!(n.item, Item::Submenu)),
                        _ => false,
                    };
                    if branch {
                        self.activate()
                    } else if self.levels.len() == 1 && self.model.kind == Kind::Bar {
                        self.switch_group(false)
                    } else {
                        Ok(Outcome::Consumed)
                    }
                }
            },
            Event::Move { x, y } | Event::Press { x, y } => {
                let press = matches!(event, Event::Press { .. });
                let Some((depth, selected)) = self.hit(x, y) else {
                    if press {
                        self.dismiss();
                        return Ok(Outcome::Dismissed);
                    }
                    return Ok(Outcome::Consumed);
                };
                if let Selection::Node(i) = selected {
                    if !self.model.node(i).is_some_and(|n| n.row.enabled) {
                        return Ok(Outcome::Consumed);
                    }
                }
                let same = self
                    .levels
                    .get(depth)
                    .is_some_and(|l| l.selected == selected);
                if !same || press {
                    self.levels.truncate(depth + 1);
                    if let Some(level) = self.levels.last_mut() {
                        level.selected = selected;
                    }
                }
                if press {
                    return self.activate();
                }
                let submenu = match selected {
                    Selection::Node(i) => self
                        .model
                        .node(i)
                        .is_some_and(|n| matches!(n.item, Item::Submenu)),
                    _ => false,
                };
                if submenu && self.levels.len() == depth + 1 {
                    return self.activate();
                }
                Ok(if same {
                    Outcome::Consumed
                } else {
                    Outcome::Changed
                })
            }
            Event::Wheel { x, y, rows } => {
                if rows == 0 {
                    return Ok(Outcome::Consumed);
                }
                let Some((depth, _)) = self.hit(x, y) else {
                    return Ok(Outcome::Consumed);
                };
                if self.fit == Fit::Complete && depth == 0 {
                    return Ok(Outcome::Consumed);
                }
                self.levels.truncate(depth + 1);
                let level = self.levels.get_mut(depth).ok_or(Error::InvalidEntry)?;
                let count = self.model.children(level.parent).count();
                let visible = level.panel.rows() - usize::from(level.back);
                let max = count.saturating_sub(visible);
                level.first = level.first.saturating_add_signed(rows).min(max);
                if let Selection::Node(index) = level.selected {
                    let shown = self
                        .model
                        .children(level.parent)
                        .skip(level.first)
                        .take(visible)
                        .any(|(i, _)| i == index);
                    if !shown {
                        level.selected = Selection::None;
                    }
                }
                Ok(Outcome::Changed)
            }
            Event::FocusLost | Event::Resize(_) => Ok(Outcome::Consumed),
        }
    }

    /// Only panels are emitted; a bar remains part of the consumer's chrome.
    pub fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        for level in self.levels.iter().skip(self.visible_start()) {
            let back = level.back.then_some(Row {
                label: "Back",
                shortcut: "<",
                enabled: true,
                checked: false,
            });
            let rows = self
                .model
                .children(level.parent)
                .skip(level.first)
                .map(|(_, node)| Row {
                    shortcut: if matches!(node.item, Item::Submenu) {
                        ">"
                    } else {
                        node.row.shortcut
                    },
                    ..node.row
                });
            let selected = match level.selected {
                Selection::Back => 0,
                Selection::Node(index) => self
                    .model
                    .children(level.parent)
                    .position(|(i, _)| i == index)
                    .and_then(|p| p.checked_sub(level.first))
                    .map(|p| p + usize::from(level.back))
                    .unwrap_or(usize::MAX),
                Selection::None => usize::MAX,
            };
            level
                .panel
                .emit(back.into_iter().chain(rows), selected, damage, sink);
        }
    }
}
