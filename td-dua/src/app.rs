//! The window's state: the list above, the treemap below, the status row,
//! the delete list and the question that deletes it. Pure: it asks the
//! file system for nothing itself, but hands `Job`s out through
//! `take_jobs` and takes the worker's `Reply`s, so a test drives it whole.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use td_ui::chrome::Status;
use td_ui::confirmations::{self, Choice};
use td_ui::keys::{self, Section};
use td_ui::pointer::DoubleClick;
use td_ui::raster::{Composition, Draw, Primitive, Raster, Rect, Surface, PAPER};
use td_ui::split;
use td_ui::tree_table::{self, Cell, Direction, Model, Sort};
use td_ui::window::{Input, PointerPhase};

use crate::delete::Target;
use crate::tree::{Identity, Kind, Measure, NodeId, Tree, ROOT};
use crate::treemap::{self, Treemap};
use crate::view::{self, RowId, COLUMNS};
use crate::worker::{Job, Reply};

type Table = tree_table::Controller<RowId>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Act {
    DeleteList,
}

type Dialog = confirmations::Controller<Act, u64, ()>;

/// The window's own keys, as `key` binds them: the key list's first
/// section, and after `CHORD_HINT` `hint` for `--help` and the status row.
pub const KEYS: &[(&str, &str)] = &[
    ("d", "add to delete list"),
    ("D", "delete now"),
    ("x", "delete the list"),
    ("u", "undo add"),
    ("r", "refresh"),
    ("a", "allocated or apparent size"),
    ("C-q", "quit"),
];
/// The title of `KEYS`'s section in the key list.
const KEYS_TITLE: &str = "Disk usage";
/// The list's keys, `tree_table::Key::from_chord`'s.
const LIST_KEYS: &[(&str, &str)] = &[
    ("Up/Down", "select the entry above or below"),
    ("PageUp/PageDown", "select a page away"),
    ("Home/End", "select the first or last row"),
    ("Left", "collapse, or go to the parent"),
    ("Right", "expand, or go to the first child"),
    // `view::SHOWN` more, which a test holds the number to.
    (
        "Return/Space",
        "open or close a directory, or show 500 more",
    ),
    ("S-Left/S-Right", "scroll sideways"),
];

/// What `keys::CHORD`, or a press on the status row, does, first in
/// `hint` so a narrow row cannot clip it; the key list's own Window
/// section lists the chord.
const CHORD_HINT: (&str, &str) = (keys::CHORD, "keys");

/// `CHORD_HINT` and then `KEYS` as one line, `key: what` two spaces apart.
pub fn hint() -> String {
    format!("{}  {}", lead(), keys_hint())
}

/// `CHORD_HINT` as `hint` spells it: what a hinted status row starts with.
fn lead() -> String {
    format!("{}: {}", CHORD_HINT.0, CHORD_HINT.1)
}

/// `KEYS` as `hint` spells them.
fn keys_hint() -> String {
    KEYS.iter()
        .map(|(keys, what)| format!("{keys}: {what}"))
        .collect::<Vec<_>>()
        .join("  ")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Capture {
    Split,
    Table,
    Map,
}

pub struct App {
    surface: Surface,
    root: PathBuf,
    tree: Option<Tree>,
    measure: Measure,
    sort: Sort,
    expanded: HashSet<NodeId>,
    limits: HashMap<NodeId, usize>,
    split: Option<split::Controller>,
    table: Option<Table>,
    map: Treemap,
    map_stale: bool,
    /// The delete list changed since `doomed` was computed.
    doomed_stale: bool,
    /// The list stops at the toolkit's row limit.
    truncated: bool,
    /// Per tile of `map`, whether the delete list holds it.
    doomed: Vec<bool>,
    queue: Vec<NodeId>,
    dialog: Option<Dialog>,
    /// Bumped by every change to the tree or the delete list, so a
    /// question asked about an older state closes unanswered.
    revision: u64,
    jobs: Vec<Job>,
    pending: usize,
    deleting: bool,
    progress: (u64, u64),
    message: String,
    /// The status row shows the key hint around `message`: `lead` first,
    /// `KEYS` last.
    hinted: bool,
    title: String,
    capture: Option<Capture>,
    clicks: DoubleClick<RowId>,
    now_ms: u64,
    /// Cell texts for the rows the table shows, from `cache_first`.
    cache: Vec<[String; 5]>,
    cache_first: usize,
    redraw: bool,
    quitting: bool,
    /// A press on the status row asked for the window's key list.
    show_keys: bool,
}

/// Where a dialog may go: centred over the window's body.
fn dialog_rect(surface: Surface) -> Option<Rect> {
    let s = surface.scale.value() as i64;
    let cell = td_ui::CELL_WIDTH as i64 * s;
    let row = 24 * s;
    let body = surface.height as i64 - row;
    let width = (surface.width as i64 - 2 * cell).min(80 * cell);
    let height = (14 * row).min(body);
    (width > 0 && height > 0).then(|| Rect {
        x: (surface.width as i64 - width) / 2,
        y: (body - height).max(0) / 2,
        width: width as u32,
        height: height as u32,
    })
}

impl App {
    /// A window over `root`, an absolute directory, asking for its scan.
    pub fn new(root: PathBuf, surface: Surface) -> Self {
        let title = format!("td-dua: {}", view::display_name(root.as_os_str()));
        let mut app = Self {
            surface,
            root: root.clone(),
            tree: None,
            measure: Measure::Allocated,
            sort: Sort {
                column: view::SIZE,
                direction: Direction::Descending,
            },
            expanded: HashSet::new(),
            limits: HashMap::new(),
            split: None,
            table: None,
            map: Treemap::default(),
            map_stale: true,
            doomed_stale: true,
            truncated: false,
            doomed: Vec::new(),
            queue: Vec::new(),
            dialog: None,
            revision: 0,
            jobs: Vec::new(),
            pending: 0,
            deleting: false,
            progress: (0, 0),
            message: String::new(),
            hinted: true,
            title,
            capture: None,
            clicks: DoubleClick::default(),
            now_ms: 0,
            cache: Vec::new(),
            cache_first: 0,
            redraw: true,
            quitting: false,
            show_keys: false,
        };
        app.relayout();
        app.send(Job::Scan {
            path: root,
            at: None,
        });
        app
    }

    pub fn title(&self) -> &str {
        &self.title
    }
    pub fn quitting(&self) -> bool {
        self.quitting
    }
    pub fn needs_redraw(&self) -> bool {
        self.redraw
    }
    pub fn busy(&self) -> bool {
        self.pending > 0
    }
    pub fn tree(&self) -> Option<&Tree> {
        self.tree.as_ref()
    }
    pub fn queue(&self) -> &[NodeId] {
        &self.queue
    }
    /// The status row's message, without the key hint around it.
    pub fn message(&self) -> &str {
        &self.message
    }
    pub fn selected(&self) -> Option<NodeId> {
        match self.table.as_ref()?.selected()? {
            RowId::Node(id) => Some(id),
            RowId::More(_) => None,
        }
    }
    pub fn table(&self) -> Option<&Table> {
        self.table.as_ref()
    }
    pub fn dialog_open(&self) -> bool {
        self.dialog.is_some()
    }
    /// The key list's sections: the delete question's first while it is
    /// open, then the window's keys and the list's.
    pub fn key_list(&self) -> Vec<Section> {
        let question = Section::new("Delete question", confirmations::KEYS);
        let window = Section::new(KEYS_TITLE, KEYS);
        let list = Section::new("List", LIST_KEYS);
        if self.dialog_open() {
            vec![question, window, list]
        } else {
            vec![window, list, question]
        }
    }
    /// Whether a press on the status row asked for the key list since the
    /// last take. Only `input`'s live pointer sets it.
    pub fn take_show_keys(&mut self) -> bool {
        std::mem::take(&mut self.show_keys)
    }
    pub fn expanded(&self, id: NodeId) -> bool {
        self.expanded.contains(&id)
    }
    /// The treemap as last laid out.
    pub fn treemap(&self) -> &Treemap {
        &self.map
    }
    /// The treemap's rectangle, the split's lower child.
    pub fn treemap_rect(&self) -> Option<Rect> {
        Some(self.split.as_ref()?.layout()?.second)
    }

    /// The jobs asked since the last take, for the worker.
    pub fn take_jobs(&mut self) -> Vec<Job> {
        std::mem::take(&mut self.jobs)
    }

    fn send(&mut self, job: Job) {
        self.deleting = matches!(job, Job::Delete { .. });
        self.pending += 1;
        self.jobs.push(job);
        self.redraw = true;
    }

    /// Shows `message` on the status row without the key hint.
    fn say(&mut self, message: impl Into<String>) {
        self.message = message.into();
        self.hinted = false;
        self.redraw = true;
    }

    /// Shows `message` on the status row inside the key hint.
    fn say_hinted(&mut self, message: impl Into<String>) {
        self.say(message);
        self.hinted = true;
    }

    /// Whether the status row shows the key hint's lead whole: the row
    /// shows the hint, lies whole on the surface, and holds the lead's
    /// cells short of the ellipsis that ends a longer line.
    fn status_opens_keys(&self) -> bool {
        let status = Status::new(self.surface);
        let rect = status.rect();
        let lead = lead().chars().count();
        let columns = status.columns();
        self.hinted
            && rect.intersection(self.surface.bounds()) == Some(rect)
            && (lead < columns || self.status_line().chars().count() <= columns)
    }

    /// The worker's running count while a job is out.
    pub fn progress(&mut self, entries: u64, bytes: u64) {
        if self.pending > 0 && self.progress != (entries, bytes) {
            self.progress = (entries, bytes);
            self.redraw = true;
        }
    }

    pub fn tick(&mut self, now_ms: u64) {
        self.now_ms = now_ms;
    }

    /// The tree or the measure changed: the treemap is laid out again.
    fn changed(&mut self) {
        self.revision += 1;
        self.map_stale = true;
        self.redraw = true;
    }

    /// Only the delete list changed: the treemap is recoloured.
    fn queue_changed(&mut self) {
        self.revision += 1;
        self.doomed_stale = true;
        self.redraw = true;
    }

    // ---- layout

    fn main_rect(&self) -> Rect {
        let status = Status::new(self.surface).rect();
        Rect {
            x: 0,
            y: 0,
            width: self.surface.width as u32,
            height: u32::try_from(status.y.max(0)).unwrap_or(0),
        }
    }

    fn relayout(&mut self) {
        let main = self.main_rect();
        let surface = self.surface;
        let resized = match &mut self.split {
            Some(split) => split
                .event(split::Event::Resize {
                    surface,
                    rect: main,
                })
                .is_ok(),
            None => false,
        };
        if !resized {
            // Logical minima; the split scales them.
            self.split = split::Controller::new(
                split::Config {
                    axis: split::Axis::Vertical,
                    first_min: 96,
                    second_min: 48,
                },
                split::Share::new(1, 2).unwrap_or_default(),
                surface,
                main,
            )
            .ok();
        }
        self.place_table();
        if let Some(dialog) = &mut self.dialog {
            match dialog_rect(surface) {
                Some(rect) => {
                    let outcome = dialog.event(
                        Some(self.revision),
                        false,
                        confirmations::Event::Resize { surface, rect },
                    );
                    if matches!(outcome, confirmations::Outcome::Closed { .. }) {
                        self.dialog = None;
                    }
                }
                None => self.dialog = None,
            }
        }
        self.map_stale = true;
        self.redraw = true;
    }

    fn table_rect(&self) -> Option<Rect> {
        Some(self.split.as_ref()?.layout()?.first)
    }

    fn place_table(&mut self) {
        let rect = self.table_rect();
        let surface = self.surface;
        let Some(table) = &mut self.table else {
            return;
        };
        let rect = rect.unwrap_or(Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
        });
        if table.resize(surface, rect).is_err() {
            self.table = None;
            self.rebuild();
        }
    }

    /// Rebuilds the list's rows from the tree, keeping the selection.
    fn rebuild(&mut self) {
        let Some(tree) = &self.tree else {
            self.table = None;
            return;
        };
        let visible = view::visible(tree, &self.expanded, &self.limits, self.sort, self.measure);
        let model = match Model::new(&visible.rows, &COLUMNS) {
            Ok(model) => model,
            Err(error) => {
                self.say(format!("cannot list the tree: {error}"));
                return;
            }
        };
        self.truncated = visible.truncated;
        match &mut self.table {
            Some(table) => {
                if let Err(error) = table.replace(model) {
                    self.say(format!("cannot lay the list out: {error}"));
                }
            }
            None => {
                let rect = self.table_rect().unwrap_or(Rect {
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 0,
                });
                match Table::new(model, self.surface, rect) {
                    Ok(mut table) => {
                        let _ = table.set_focus(tree_table::Focus::Rows);
                        table.select(Some(RowId::Node(ROOT)), true);
                        self.table = Some(table);
                    }
                    Err(error) => self.say(format!("cannot lay the list out: {error}")),
                }
            }
        }
        self.redraw = true;
    }

    // ---- input

    pub fn input(&mut self, input: Input<'_>) {
        match input {
            Input::Resize(surface) => {
                self.surface = surface;
                self.relayout();
            }
            Input::Close => self.quitting = true,
            Input::Focus(false) => {
                self.capture = None;
                self.clicks.cancel();
                if let Some(table) = &mut self.table {
                    table.event(tree_table::Event::FocusLost);
                }
                if let Some(split) = &mut self.split {
                    let _ = split.event(split::Event::FocusLost);
                }
                self.dialog_event(confirmations::Event::FocusLost);
                self.redraw = true;
            }
            Input::Focus(true) => {
                if let Some(table) = &mut self.table {
                    let _ = table.set_focus(tree_table::Focus::Rows);
                }
                self.redraw = true;
            }
            Input::Key { chord, repeat } => self.key(chord, repeat),
            Input::Pointer { phase, x, y, .. } => self.pointer(phase, x, y),
            Input::CancelPointer => {
                self.capture = None;
                if let Some(table) = &mut self.table {
                    table.event(tree_table::Event::Other);
                }
                if let Some(split) = &mut self.split {
                    let _ = split.event(split::Event::FocusLost);
                }
                self.dialog_event(confirmations::Event::Other);
            }
            Input::Wheel { rows, columns } => {
                if self.dialog.is_some() {
                    return;
                }
                if let Some(table) = &mut self.table {
                    let outcome = table.event(tree_table::Event::Scroll {
                        rows: rows as i64 * 3,
                        columns: columns as i64,
                    });
                    self.table_outcome(outcome);
                    self.redraw = true;
                }
            }
            _ => {}
        }
    }

    fn dialog_event(&mut self, event: confirmations::Event) {
        let Some(dialog) = &mut self.dialog else {
            return;
        };
        match dialog.event(Some(self.revision), false, event) {
            confirmations::Outcome::Ignored | confirmations::Outcome::Consumed => {}
            confirmations::Outcome::Changed => self.redraw = true,
            confirmations::Outcome::Closed { choice, .. } => {
                self.dialog = None;
                self.redraw = true;
                match choice {
                    Choice::Confirmed(Act::DeleteList) => self.delete_list(),
                    Choice::Cancelled => self.say("Nothing deleted"),
                    Choice::Stale => self.say("The delete list changed; press x again"),
                    Choice::Unavailable(_) => self.say("The window is too small for the question"),
                }
            }
        }
    }

    fn key(&mut self, chord: &str, repeat: bool) {
        if self.dialog.is_some() {
            let event = match confirmations::Key::from_chord(chord) {
                Some(key) => confirmations::Event::Key {
                    key,
                    repeated: repeat,
                },
                None => confirmations::Event::Other,
            };
            self.dialog_event(event);
            return;
        }
        if !repeat {
            match chord {
                "C-q" => {
                    self.quitting = true;
                    return;
                }
                "d" => return self.queue_selected(),
                "D" => return self.delete_selected(),
                "x" => return self.ask_delete_list(),
                "u" => return self.undo(),
                "r" => return self.refresh(),
                "a" => return self.toggle_measure(),
                _ => {}
            }
        }
        let Some(key) = tree_table::Key::from_chord(chord) else {
            return;
        };
        if let Some(table) = &mut self.table {
            self.clicks.cancel();
            let outcome = table.event(tree_table::Event::Key {
                key,
                repeated: repeat,
            });
            self.table_outcome(outcome);
            self.redraw = true;
        }
    }

    fn pointer(&mut self, phase: PointerPhase, x: i64, y: i64) {
        if self.dialog.is_some() {
            self.dialog_event(match phase {
                PointerPhase::Press => confirmations::Event::Press { x, y },
                PointerPhase::Move => confirmations::Event::Move { x, y },
                PointerPhase::Release => confirmations::Event::Release { x, y },
            });
            return;
        }
        if phase == PointerPhase::Press
            && Status::new(self.surface).rect().contains(x, y)
            && self.status_opens_keys()
        {
            // The status row shows the key list's hint whole: a press on
            // it opens the list, and its release is nobody's.
            self.capture = None;
            self.clicks.cancel();
            self.show_keys = true;
            return;
        }
        let capture = match phase {
            PointerPhase::Press => {
                self.capture = None;
                let split = self
                    .split
                    .as_mut()
                    .map(|split| split.event(split::Event::Press { x, y }));
                if matches!(split, Some(Ok(outcome)) if outcome != split::Outcome::Ignored) {
                    Some(Capture::Split)
                } else if self.table_rect().is_some_and(|r| r.contains(x, y)) {
                    Some(Capture::Table)
                } else if self.treemap_rect().is_some_and(|r| r.contains(x, y)) {
                    Some(Capture::Map)
                } else {
                    None
                }
            }
            _ => self.capture,
        };
        if phase == PointerPhase::Press {
            self.capture = capture;
        }
        if phase == PointerPhase::Release {
            self.capture = None;
        }
        match capture {
            Some(Capture::Split) => {
                if phase != PointerPhase::Press {
                    if let Some(split) = &mut self.split {
                        let event = if phase == PointerPhase::Move {
                            split::Event::Move { x, y }
                        } else {
                            split::Event::Release { x, y }
                        };
                        let _ = split.event(event);
                    }
                }
                self.place_table();
                self.map_stale = true;
                self.redraw = true;
            }
            Some(Capture::Table) => {
                let Some(table) = &mut self.table else {
                    return;
                };
                let event = match phase {
                    PointerPhase::Press => tree_table::Event::Press { x, y },
                    PointerPhase::Move => tree_table::Event::Move { x, y },
                    PointerPhase::Release => tree_table::Event::Release { x, y },
                };
                let outcome = table.event(event);
                if let tree_table::Outcome::Selected(id) = outcome {
                    let s = self.surface.scale.value() as i64;
                    if self.clicks.completed(
                        id,
                        self.now_ms.saturating_mul(1_000_000),
                        x / s,
                        y / s,
                    ) {
                        self.activate(id);
                    }
                }
                self.table_outcome(outcome);
                self.redraw = true;
            }
            Some(Capture::Map) if phase == PointerPhase::Press => {
                self.clicks.cancel();
                self.lay_out_map();
                if let Some(id) = self.map.hit(x, y) {
                    self.reveal(id);
                }
            }
            Some(Capture::Map) | None => {}
        }
    }

    fn table_outcome(&mut self, outcome: tree_table::Outcome<RowId>) {
        match outcome {
            tree_table::Outcome::Disclosure {
                id: RowId::Node(id),
                expanded,
            } => {
                if expanded {
                    self.expanded.insert(id);
                } else {
                    self.expanded.remove(&id);
                }
                self.rebuild();
            }
            tree_table::Outcome::Activate(id) => self.activate(id),
            tree_table::Outcome::Sort(column) => {
                self.sort = if self.sort.column == column {
                    Sort {
                        column,
                        direction: match self.sort.direction {
                            Direction::Ascending => Direction::Descending,
                            Direction::Descending => Direction::Ascending,
                        },
                    }
                } else {
                    Sort {
                        column,
                        direction: view::first_direction(column),
                    }
                };
                self.rebuild();
            }
            _ => {}
        }
    }

    /// Enter or a double click: a directory opens or closes, the row past
    /// a directory's shown entries shows more.
    fn activate(&mut self, id: RowId) {
        match id {
            RowId::Node(node) => {
                let dir = self
                    .tree
                    .as_ref()
                    .and_then(|tree| tree.get(node))
                    .is_some_and(|n| n.is_directory() && !n.children.is_empty());
                if !dir {
                    return;
                }
                if !self.expanded.remove(&node) {
                    self.expanded.insert(node);
                }
                self.rebuild();
            }
            RowId::More(dir) => {
                let limit = self.limits.get(&dir).copied().unwrap_or(view::SHOWN);
                self.limits.insert(dir, limit.saturating_add(view::SHOWN));
                self.rebuild();
                // The first newly shown entry takes the selection.
                let next = self.tree.as_ref().and_then(|tree| {
                    view::sorted_children(tree, dir, self.sort, self.measure)
                        .get(limit)
                        .copied()
                });
                if let (Some(table), Some(next)) = (&mut self.table, next) {
                    table.select(Some(RowId::Node(next)), true);
                }
            }
        }
    }

    /// Opens every directory above `id` (and `id` itself when it is one),
    /// showing as many of each directory's entries as reach it, and
    /// selects it.
    pub fn reveal(&mut self, id: NodeId) {
        let Some(tree) = &self.tree else {
            return;
        };
        if tree.get(id).is_none() {
            return;
        }
        let mut child = id;
        for ancestor in tree.ancestors(id) {
            self.expanded.insert(ancestor);
            let order = view::sorted_children(tree, ancestor, self.sort, self.measure);
            if let Some(position) = order.iter().position(|c| *c == child) {
                let limit = self.limits.get(&ancestor).copied().unwrap_or(view::SHOWN);
                if position >= limit {
                    let needed = (position / view::SHOWN + 1) * view::SHOWN;
                    self.limits.insert(ancestor, needed);
                }
            }
            child = ancestor;
        }
        if tree.get(id).is_some_and(|node| node.is_directory()) {
            self.expanded.insert(id);
        }
        self.rebuild();
        let shown = self.table.as_mut().is_some_and(|table| {
            let _ = table.set_focus(tree_table::Focus::Rows);
            table.select(Some(RowId::Node(id)), true)
        });
        if !shown {
            // Nothing stays selected that a delete key could act on.
            if let Some(table) = &mut self.table {
                table.select(None, false);
            }
            let message = format!("{} cannot be shown in the list", self.describe(id));
            self.say(message);
        }
        self.redraw = true;
    }

    // ---- delete list and jobs

    fn describe(&self, id: NodeId) -> String {
        let Some(tree) = &self.tree else {
            return String::new();
        };
        let name = tree
            .path(id)
            .map(|path| view::display_name(path.as_os_str()))
            .unwrap_or_default();
        let size = tree
            .get(id)
            .map(|node| view::size(node.total.get(self.measure)))
            .unwrap_or_default();
        format!("{name} ({size})")
    }

    fn queued_size(&self) -> u64 {
        let Some(tree) = &self.tree else {
            return 0;
        };
        self.covering()
            .iter()
            .filter_map(|id| tree.get(*id))
            .map(|node| node.total.get(self.measure))
            .sum()
    }

    /// The queued nodes no other queued node lies above.
    fn covering(&self) -> Vec<NodeId> {
        let Some(tree) = &self.tree else {
            return Vec::new();
        };
        self.queue
            .iter()
            .copied()
            .filter(|id| tree.get(*id).is_some())
            .filter(|id| {
                !self
                    .queue
                    .iter()
                    .any(|other| other != id && tree.within(*id, *other))
            })
            .collect()
    }

    fn deletable(&mut self) -> Option<NodeId> {
        let Some(id) = self.selected() else {
            self.say("Select an entry first");
            return None;
        };
        if id == ROOT {
            self.say("The scanned directory itself cannot be deleted from here");
            return None;
        }
        if self
            .tree
            .as_ref()
            .and_then(|tree| tree.get(id))
            .is_some_and(|node| node.kind == Kind::Mount)
        {
            self.say("A directory on another file system is not deleted from here");
            return None;
        }
        let holds_mount = self.tree.as_ref().is_some_and(|tree| holds_mount(tree, id));
        if holds_mount {
            self.say("That directory holds another file system; delete inside it instead");
            return None;
        }
        Some(id)
    }

    fn queue_selected(&mut self) {
        let Some(id) = self.deletable() else {
            return;
        };
        let Some(tree) = &self.tree else {
            return;
        };
        if self.queue.contains(&id) {
            return self.say(format!(
                "{} is already in the delete list",
                self.describe(id)
            ));
        }
        if let Some(above) = self.queue.iter().find(|q| tree.within(id, **q)).copied() {
            return self.say(format!(
                "Already in the delete list through {}",
                self.describe(above)
            ));
        }
        self.queue.push(id);
        self.queue_changed();
        let message = format!(
            "Added {}; the delete list holds {} ({})",
            self.describe(id),
            self.covering().len(),
            view::size(self.queued_size())
        );
        self.say(message);
    }

    fn undo(&mut self) {
        let Some(id) = self.queue.pop() else {
            return self.say("The delete list is empty");
        };
        self.queue_changed();
        let message = format!(
            "Took {} off; the delete list holds {} ({})",
            self.describe(id),
            self.covering().len(),
            view::size(self.queued_size())
        );
        self.say(message);
    }

    fn target(&self, id: NodeId) -> Option<Target> {
        let tree = self.tree.as_ref()?;
        let node = tree.get(id)?;
        Some(Target {
            path: tree.path(id)?,
            identity: node.identity,
            directory: matches!(node.kind, Kind::Dir | Kind::Mount),
            length: node.length,
            mtime: node.mtime,
        })
    }

    fn refuse_busy(&mut self) -> bool {
        if self.pending > 0 {
            self.say("Wait for the scan or deletion in progress to finish");
            return true;
        }
        false
    }

    fn delete_selected(&mut self) {
        if self.refuse_busy() {
            return;
        }
        let Some(id) = self.deletable() else {
            return;
        };
        let Some(target) = self.target(id) else {
            return;
        };
        let message = format!("Deleting {}", self.describe(id));
        self.send(Job::Delete {
            targets: vec![(id, target)],
        });
        self.say(message);
    }

    fn ask_delete_list(&mut self) {
        if self.refuse_busy() {
            return;
        }
        let covering = self.covering();
        if covering.is_empty() {
            return self.say("The delete list is empty: add entries with d");
        }
        let total = view::size(self.queued_size());
        let title = format!(
            "Delete {} entr{} ({total}) for good?",
            covering.len(),
            if covering.len() == 1 { "y" } else { "ies" }
        );
        let mut details: Vec<String> = covering
            .iter()
            .take(confirmations::DETAILS - 1)
            .map(|id| self.describe(*id))
            .map(|text| {
                text.chars()
                    .take(confirmations::DETAIL_BYTES / 4)
                    .collect::<String>()
            })
            .collect();
        if covering.len() >= confirmations::DETAILS {
            details.push(format!(
                "and {} more",
                covering.len() + 1 - confirmations::DETAILS
            ));
        }
        let details: Vec<&str> = details.iter().map(String::as_str).collect();
        let Some(rect) = dialog_rect(self.surface) else {
            return self.say("The window is too small for the question");
        };
        let dialog =
            confirmations::Model::new(&title, "Delete", &details, Act::DeleteList, self.revision)
                .and_then(|model| Dialog::new(model, self.surface, rect, None));
        match dialog {
            Ok(dialog) => {
                self.dialog = Some(dialog);
                self.say("Tab to Delete and press Enter, or Escape to keep everything");
            }
            Err(error) => self.say(format!("cannot ask: {error}")),
        }
    }

    fn delete_list(&mut self) {
        if self.refuse_busy() {
            return;
        }
        let targets: Vec<(NodeId, Target)> = self
            .covering()
            .into_iter()
            .filter_map(|id| Some((id, self.target(id)?)))
            .collect();
        if targets.is_empty() {
            return self.say("The delete list is empty");
        }
        let message = format!("Deleting {} entries", targets.len());
        self.send(Job::Delete { targets });
        self.say(message);
    }

    fn refresh(&mut self) {
        if self.refuse_busy() {
            return;
        }
        let Some(tree) = &self.tree else {
            let root = self.root.clone();
            self.send(Job::Scan {
                path: root,
                at: None,
            });
            return;
        };
        // The row past a directory's shown entries refreshes that directory.
        let id = match self.table.as_ref().and_then(|table| table.selected()) {
            Some(RowId::Node(id) | RowId::More(id)) => id,
            None => ROOT,
        };
        // A file refreshes the directory holding it.
        let dir = match tree.get(id).map(|node| node.kind) {
            Some(Kind::Dir | Kind::Mount) => id,
            _ => tree.get(id).and_then(|node| node.parent).unwrap_or(ROOT),
        };
        let Some(path) = tree.path(dir) else {
            return;
        };
        let message = format!("Refreshing {}", view::display_name(path.as_os_str()));
        self.send(Job::Scan {
            path,
            at: Some(dir),
        });
        self.say(message);
    }

    fn toggle_measure(&mut self) {
        self.measure = match self.measure {
            Measure::Allocated => Measure::Apparent,
            Measure::Apparent => Measure::Allocated,
        };
        self.say(match self.measure {
            Measure::Allocated => "Sizes are the blocks allocated on disk",
            Measure::Apparent => "Sizes are the files' apparent lengths",
        });
        self.changed();
        self.rebuild();
    }

    /// The worker's answer to one job.
    pub fn reply(&mut self, reply: Reply) {
        // A reply may hand out new ids; a click pending across it pairs
        // with nothing.
        self.clicks.cancel();
        self.pending = self.pending.saturating_sub(1);
        if self.pending == 0 {
            self.progress = (0, 0);
        }
        match reply {
            Reply::Scanned {
                at: None,
                path,
                tree,
            } => match tree {
                Ok(tree) => {
                    self.tree = Some(tree);
                    self.expanded = [ROOT].into_iter().collect();
                    self.limits.clear();
                    self.queue.clear();
                    self.table = None;
                    self.changed();
                    self.rebuild();
                    self.scanned_message();
                }
                Err(error) => self.say(format!(
                    "cannot scan {}: {error}",
                    view::display_name(path.as_os_str())
                )),
            },
            Reply::Scanned {
                at: Some(at),
                path,
                tree,
            } => {
                let fresh = match tree {
                    Ok(fresh) => fresh,
                    Err(error) => {
                        return self.say(format!(
                            "cannot refresh {}: {error}",
                            view::display_name(path.as_os_str())
                        ))
                    }
                };
                self.refreshed(at, path, fresh);
            }
            Reply::Deleted { results } => self.deleted(results),
        }
    }

    /// Takes a refresh of `at` into the tree. The root's replaces the tree
    /// whole, so retired nodes do not pile up; another's is grafted. The
    /// selection, the open directories, how many entries each shows and
    /// the delete list follow their entries by path, and the delete list
    /// says what it lost.
    fn refreshed(&mut self, at: NodeId, path: PathBuf, fresh: Tree) {
        let Some(current) = &self.tree else {
            return;
        };
        // A node retired since the scan was asked takes nothing.
        if current.path(at).as_ref() != Some(&path) {
            return;
        }
        let under = |id: &NodeId| current.relative(*id, at);
        // Only a selection inside the refreshed directory is carried; one
        // elsewhere is the table's to keep, as it is.
        // The row's kind is carried too: a "more" row is no delete target,
        // and must not come back as its directory.
        let selected = match self.table.as_ref().and_then(|table| table.selected()) {
            Some(RowId::Node(id)) => under(&id).map(|names| (names, false)),
            Some(RowId::More(id)) => under(&id).map(|names| (names, true)),
            None => None,
        };
        // A listed entry is carried only to the same inode of the same kind.
        type Queued = (NodeId, Option<(Vec<std::ffi::OsString>, Identity, Kind)>);
        let queue: Vec<Queued> = self
            .queue
            .iter()
            .map(|id| {
                let seen = current.get(*id).map(|node| (node.identity, node.kind));
                (
                    *id,
                    under(id).zip(seen).map(|(names, (i, k))| (names, i, k)),
                )
            })
            .collect();
        let expanded: Vec<Vec<std::ffi::OsString>> =
            self.expanded.iter().filter_map(under).collect();
        let limits: Vec<(Vec<std::ffi::OsString>, usize)> = self
            .limits
            .iter()
            .filter_map(|(id, limit)| Some((under(id)?, *limit)))
            .collect();
        if at == ROOT {
            self.tree = Some(fresh);
            self.table = None;
        } else if let Some(current) = &mut self.tree {
            if let Err(error) = current.graft(at, fresh) {
                return self.say(format!(
                    "cannot refresh: {error}; refresh the top directory instead"
                ));
            }
        }
        let Some(tree) = &self.tree else {
            return;
        };
        let before = self.queue.len();
        self.queue = queue
            .into_iter()
            .filter_map(|(id, relative)| match relative {
                Some((names, identity, kind)) => tree.resolve(at, &names).filter(|new| {
                    tree.get(*new)
                        .is_some_and(|node| node.identity == identity && node.kind == kind)
                }),
                // Outside the refreshed directory; a root refresh has no
                // outside, and an old id there would name a new node.
                None if at == ROOT => None,
                None => tree.get(id).map(|_| id),
            })
            .collect();
        let lost = before - self.queue.len();
        if at == ROOT {
            self.expanded.clear();
            self.limits.clear();
        }
        self.expanded
            .extend(expanded.iter().filter_map(|names| tree.resolve(at, names)));
        self.expanded.insert(ROOT);
        for (names, limit) in &limits {
            if let Some(id) = tree.resolve(at, names) {
                self.limits.insert(id, *limit);
            }
        }
        // A carried selection whose entry is gone leaves nothing selected,
        // never its directory, which a delete key would then act on.
        let reselect = selected.map(|(names, more)| {
            tree.resolve(at, &names).map(|id| {
                if more {
                    RowId::More(id)
                } else {
                    RowId::Node(id)
                }
            })
        });
        self.prune();
        self.changed();
        self.queue_changed();
        self.rebuild();
        if let (Some(table), Some(reselect)) = (&mut self.table, reselect) {
            let shown = reselect.is_some_and(|row| table.select(Some(row), true));
            if !shown {
                table.select(None, false);
            }
        }
        self.scanned_message();
        if lost > 0 {
            let message = format!(
                "{} entr{} left the delete list: not found as listed.  {}",
                lost,
                if lost == 1 { "y" } else { "ies" },
                self.message
            );
            if self.hinted {
                self.say_hinted(message);
            } else {
                self.say(message);
            }
        }
    }

    fn scanned_message(&mut self) {
        let Some(tree) = &self.tree else {
            return;
        };
        let Some(root) = tree.root() else {
            return;
        };
        let mut message = format!(
            "{} in {} files",
            view::size(root.total.get(self.measure)),
            root.files
        );
        if tree.partial {
            message.push_str(" (some entries could not be read)");
        }
        message.push_str(".  ");
        self.say_hinted(message);
    }

    /// Drops what the tree no longer holds from the list's state.
    fn prune(&mut self) {
        let Some(tree) = &self.tree else {
            return;
        };
        self.queue.retain(|id| tree.get(*id).is_some());
        self.expanded.retain(|id| tree.get(*id).is_some());
        self.limits.retain(|id, _| tree.get(*id).is_some());
    }

    fn deleted(&mut self, results: Vec<(NodeId, Target, Result<(), String>)>) {
        let mut removed = 0usize;
        let mut freed = 0u64;
        let mut failures = Vec::new();
        let mut rescans = Vec::new();
        // Inodes whose bytes a deleted name owned while another name may live.
        let mut orphaned: HashMap<crate::tree::Identity, crate::tree::Size> = HashMap::new();
        for (id, target, result) in results {
            let Some(tree) = &mut self.tree else {
                break;
            };
            let current = tree.path(id).as_ref() == Some(&target.path);
            match result {
                Ok(()) => {
                    removed += 1;
                    if current {
                        for node in tree.subtree(id).iter().filter_map(|n| tree.get(*n)) {
                            if node.links && node.own != crate::tree::Size::default() {
                                orphaned.insert(node.identity, node.own);
                            }
                        }
                        freed = freed.saturating_add(
                            tree.get(id).map_or(0, |node| node.total.get(self.measure)),
                        );
                        let _ = tree.remove(id);
                    }
                }
                Err(error) => {
                    // A directory may have lost part of its entries.
                    if current && tree.get(id).is_some_and(|node| node.is_directory()) {
                        rescans.push((id, target.path.clone()));
                    }
                    failures.push(format!(
                        "{}: {error}",
                        view::display_name(target.path.as_os_str())
                    ));
                }
            }
        }
        freed = freed.saturating_sub(self.adopt(orphaned));
        // A deleted selection leaves nothing selected: a second delete key
        // must not climb to the directory that held it.
        self.prune();
        self.changed();
        self.queue_changed();
        self.rebuild();
        let mut message = format!("Deleted {removed}, freeing {}", view::size(freed));
        if let Some(first) = failures.first() {
            message.push_str(&format!("; {} failed: {first}", failures.len()));
        }
        self.say(message);
        for (id, path) in rescans {
            self.send(Job::Scan { path, at: Some(id) });
        }
    }

    /// Gives each orphaned inode's bytes to a surviving name of it, and
    /// answers the bytes so kept on disk.
    fn adopt(&mut self, mut orphaned: HashMap<crate::tree::Identity, crate::tree::Size>) -> u64 {
        let Some(tree) = &mut self.tree else {
            return 0;
        };
        if orphaned.is_empty() {
            return 0;
        }
        let mut kept = 0u64;
        for id in 0..tree.len() {
            let id = id as NodeId;
            let Some(node) = tree.get(id) else {
                continue;
            };
            if !node.links {
                continue;
            }
            if let Some(size) = orphaned.remove(&node.identity) {
                kept = kept.saturating_add(size.get(self.measure));
                let _ = tree.set_own(id, size);
            }
            if orphaned.is_empty() {
                break;
            }
        }
        kept
    }

    // ---- painting

    fn lay_out_map(&mut self) {
        let area = self.treemap_rect();
        if self.map_stale || self.map.area != area {
            self.map_stale = false;
            self.doomed_stale = true;
            self.map = Treemap::default();
            if let (Some(tree), Some(area)) = (&self.tree, area) {
                let min_side = 3.0 * self.surface.scale.value() as f64;
                self.map = treemap::layout(tree, self.measure, area, min_side);
            }
        }
        if !self.doomed_stale {
            return;
        }
        self.doomed_stale = false;
        self.doomed.clear();
        let Some(tree) = &self.tree else {
            return;
        };
        let queued: HashSet<NodeId> = self.queue.iter().copied().collect();
        let mut memo: HashMap<NodeId, bool> = HashMap::new();
        self.doomed = self
            .map
            .tiles
            .iter()
            .map(|tile| doomed(tree, tile.node, &queued, &mut memo))
            .collect();
    }

    /// The frame as it would be painted; call `prepare` first.
    pub fn frame(&self) -> impl Composition + '_ {
        Frame { app: self }
    }

    /// Prepares the shown rows' cell texts, then paints.
    pub fn paint(&mut self, raster: &mut Raster<'_, '_>, surface: Surface) -> Result<(), String> {
        if surface != self.surface {
            self.surface = surface;
            self.relayout();
        }
        self.prepare();
        self.redraw = false;
        raster
            .paint(&Frame { app: self }, surface.bounds())
            .map_err(|error| error.to_string())
    }

    /// Lays the treemap out and fills the cell cache for the shown rows.
    pub fn prepare(&mut self) {
        self.lay_out_map();
        self.cache.clear();
        let (Some(table), Some(tree)) = (&self.table, &self.tree) else {
            return;
        };
        let Some(geometry) = table.geometry() else {
            return;
        };
        let rows = table.model().rows();
        let first = geometry.first();
        self.cache_first = first;
        let queued: HashSet<NodeId> = self.queue.iter().copied().collect();
        for row in rows.iter().skip(first).take(geometry.visible() + 1) {
            let queued = matches!(row.id, RowId::Node(id) if queued.contains(&id));
            let shown = match row.id {
                RowId::More(dir) => self.limits.get(&dir).copied().unwrap_or(view::SHOWN),
                RowId::Node(_) => 0,
            };
            self.cache
                .push(view::cells(tree, row.id, self.measure, queued, shown));
        }
    }

    /// The status row's line: the key hint's lead when it shows the hint,
    /// the running job's count, the list's notes, the message, and the
    /// hint's `KEYS`.
    pub fn status_line(&self) -> String {
        let mut line = String::new();
        if self.hinted {
            line.push_str(&lead());
            line.push_str("  ");
        }
        if self.pending > 0 {
            if self.deleting {
                line.push_str("Deleting…  ");
            } else {
                line.push_str(&format!(
                    "Scanning: {} entries, {}…  ",
                    self.progress.0,
                    view::size(self.progress.1)
                ));
            }
        }
        if self.truncated {
            line.push_str(&format!("[list cut at {} rows]  ", tree_table::ROWS));
        }
        if !self.queue.is_empty() {
            line.push_str(&format!(
                "[delete list: {}, {}]  ",
                self.covering().len(),
                view::size(self.queued_size())
            ));
        }
        line.push_str(&self.message);
        if self.hinted {
            line.push_str(&keys_hint());
        }
        line
    }
}

/// Whether a mount lies beneath `id`, stopping at the first.
fn holds_mount(tree: &Tree, id: NodeId) -> bool {
    let mut stack: Vec<NodeId> = tree
        .get(id)
        .map(|node| node.children.clone())
        .unwrap_or_default();
    while let Some(at) = stack.pop() {
        let Some(node) = tree.get(at) else {
            continue;
        };
        if node.kind == Kind::Mount {
            return true;
        }
        stack.extend(node.children.iter().copied());
    }
    false
}

fn doomed(
    tree: &Tree,
    id: NodeId,
    queued: &HashSet<NodeId>,
    memo: &mut HashMap<NodeId, bool>,
) -> bool {
    let mut path = Vec::new();
    let mut at = Some(id);
    let mut answer = false;
    while let Some(current) = at {
        if let Some(known) = memo.get(&current) {
            answer = *known;
            break;
        }
        if queued.contains(&current) {
            answer = true;
            path.push(current);
            break;
        }
        path.push(current);
        at = tree.get(current).and_then(|node| node.parent);
    }
    for node in path {
        memo.insert(node, answer);
    }
    answer
}

struct Frame<'a> {
    app: &'a App,
}

impl Composition for Frame<'_> {
    fn surface(&self) -> Surface {
        self.app.surface
    }

    fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let app = self.app;
        let bounds = app.surface.bounds();
        if let Some(clip) = bounds.intersection(damage) {
            sink(Draw {
                clip,
                primitive: Primitive::Fill {
                    rect: bounds,
                    color: PAPER,
                },
            });
        }
        if let Some(table) = &app.table {
            let model = table.model();
            let mut values = |id: RowId, column: usize| -> Cell<'_> {
                model
                    .find(id)
                    .and_then(|index| index.checked_sub(app.cache_first))
                    .and_then(|index| app.cache.get(index))
                    .and_then(|cells| cells.get(column))
                    .and_then(|text| Cell::new(text).ok())
                    .unwrap_or_else(Cell::empty)
            };
            table.emit(Some(app.sort), damage, &mut values, sink);
        }
        if let Some(split) = &app.split {
            split.emit(damage, sink);
        }
        if let Some(area) = app.treemap_rect() {
            let Some(damage) = area.intersection(damage) else {
                return self.overlays(damage, sink);
            };
            for (index, tile) in app.map.tiles.iter().enumerate() {
                if tile.rect.intersection(damage).is_none() {
                    continue;
                }
                let color = if app.doomed.get(index).copied().unwrap_or(false) {
                    treemap::DOOMED
                } else {
                    tile.color
                };
                treemap::emit_tile(tile.rect, color, damage, sink);
            }
            if let Some(selected) = app.selected() {
                let frame = app.tree.as_ref().and_then(|tree| {
                    std::iter::once(selected)
                        .chain(tree.ancestors(selected))
                        .find_map(|id| app.map.frames.get(&id).copied())
                });
                if let Some(frame) = frame {
                    let s = app.surface.scale.value() as u32;
                    treemap::emit_outline(frame, 3 * s, 0x000000, damage, sink);
                    if frame.width > 2 * s && frame.height > 2 * s {
                        let inner = Rect {
                            x: frame.x + i64::from(s),
                            y: frame.y + i64::from(s),
                            width: frame.width - 2 * s,
                            height: frame.height - 2 * s,
                        };
                        treemap::emit_outline(inner, s, 0xffffff, damage, sink);
                    }
                }
            }
        }
        self.overlays(damage, sink);
    }
}

impl Frame<'_> {
    fn overlays(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let app = self.app;
        let line = app.status_line();
        Status::new(app.surface).emit(line.chars(), damage, sink);
        if let Some(dialog) = &app.dialog {
            dialog.emit(damage, sink);
        }
    }
}
