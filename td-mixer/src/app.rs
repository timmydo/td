//! The panel: one list of rows, the playing output's level first, then
//! every output, then each application's level, with a slider for the
//! focused level and a status row. Keyboard first: Up/Down or Tab move,
//! Left/Right step a level by `STEP`, M mutes it, Return chooses an output.
//! A change shows at once and is sent; the daemon's next snapshot, which
//! also brings changes made elsewhere, is what the panel then shows, with
//! the actions it has not yet answered shown over it.

use crate::model::{self, AppLevel, Job, Level, Output, Snapshot, Target};
use crate::worker::Reply;
use td_pulse::client::MAX_PERCENT;
use td_ui::chrome::{Item, List, Slider, Status as StatusRow, ROW};
use td_ui::keys::{self, Section};
use td_ui::raster::{
    self, Composition, Draw, GlyphStyle, Primitive, Raster, Rect, Surface, INK, PAPER,
};
use td_ui::window::{Input, PointerPhase};
use td_ui::CELL_WIDTH;

/// One step of a level, in whole percent: the volume keys' step.
pub const STEP: u32 = 5;

/// The window's title.
pub const TITLE: &str = "Sound";

/// How often the window polls the worker: a change made elsewhere shows
/// within this.
pub const POLL_MS: u64 = 100;

const KEYS_TITLE: &str = "Sound";
pub const KEYS: &[(&str, &str)] = &[
    ("Up/Down, Tab, wheel", "move to the row above or below"),
    ("Home/End", "move to the first or last row"),
    ("Left/Right, -/+", "lower or raise the level by 5%"),
    ("m", "mute or unmute the level"),
    ("Return, Space", "on an output, make it the one that plays"),
    ("Return, Space", "on a level, mute or unmute it"),
    ("Escape, C-q", "close the panel"),
];

/// Margin round the list and the slider band, in font pixels.
const MARGIN: usize = 8;
/// The slider band's label and value columns, in cells.
const LABEL_CELLS: usize = 16;
const VALUE_CELLS: usize = 11;

/// What a row is, kept across snapshots so focus stays on it when rows
/// come and go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Key {
    Master,
    Output(String),
    App(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Daemon {
    /// No reply yet.
    Waiting,
    Live,
    Unavailable,
}

enum Row<'a> {
    Master(Level),
    Output(&'a Output),
    App(&'a AppLevel),
}

impl Row<'_> {
    /// Whether this is the row `key` names.
    fn is(&self, key: &Key) -> bool {
        match (self, key) {
            (Self::Master(_), Key::Master) => true,
            (Self::Output(output), Key::Output(name)) => &output.name == name,
            (Self::App(app), Key::App(name)) => &app.name == name,
            _ => false,
        }
    }

    fn key(&self) -> Option<Key> {
        Some(match self {
            Self::Master(_) => Key::Master,
            Self::Output(output) => Key::Output(td_alloc::string(&output.name).ok()?),
            Self::App(app) => Key::App(td_alloc::string(&app.name).ok()?),
        })
    }

    fn level(&self) -> Option<Level> {
        match self {
            Self::Master(level) => Some(*level),
            Self::App(app) => Some(app.level),
            Self::Output(_) => None,
        }
    }

    fn target(&self) -> Option<Target> {
        match self {
            Self::Master(_) => Some(Target::Master),
            Self::App(app) => td_alloc::string(&app.name).ok().map(Target::App),
            Self::Output(_) => None,
        }
    }
}

struct Layout {
    list: Option<List>,
    slider: Option<Slider>,
    band: Rect,
}

pub struct App {
    surface: Surface,
    snapshot: Snapshot,
    daemon: Daemon,
    focus: Key,
    /// Where the focused row was, for the row in its place when it goes.
    at: usize,
    first: usize,
    message: String,
    /// Actions not yet handed to the worker, oldest first.
    waiting: Vec<Job>,
    /// Actions handed to the worker and not yet answered, oldest first.
    flight: Vec<Job>,
    /// The row a slider drag began on; the drag ends if it goes.
    dragging: Option<Key>,
    quitting: bool,
    redraw: bool,
    show_keys: bool,
}

impl App {
    pub fn new(surface: Surface) -> Self {
        Self {
            surface,
            snapshot: Snapshot::default(),
            daemon: Daemon::Waiting,
            focus: Key::Master,
            at: 0,
            first: 0,
            message: String::new(),
            waiting: Vec::new(),
            flight: Vec::new(),
            dragging: None,
            quitting: false,
            redraw: true,
            show_keys: false,
        }
    }

    pub fn title(&self) -> &str {
        TITLE
    }

    pub fn quitting(&self) -> bool {
        self.quitting
    }

    pub fn needs_redraw(&self) -> bool {
        self.redraw
    }

    pub fn daemon(&self) -> Daemon {
        self.daemon
    }

    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    pub fn focus(&self) -> &Key {
        &self.focus
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn wait_ms(&self) -> u64 {
        POLL_MS
    }

    pub fn key_list(&self) -> Vec<Section> {
        let mut sections = Vec::new();
        // A list that cannot be held shows the window's own keys alone.
        let _ = td_alloc::push(&mut sections, Section::new(KEYS_TITLE, KEYS));
        sections
    }

    pub fn take_show_keys(&mut self) -> bool {
        std::mem::take(&mut self.show_keys)
    }

    /// Hand the waiting actions to `send`, oldest first. One it gives
    /// back, the worker's queue full, waits with every newer one for the
    /// next flush; one it takes is in flight until a reply answers it.
    pub fn flush<E>(
        &mut self,
        mut send: impl FnMut(Job) -> Result<Option<Job>, E>,
    ) -> Result<(), E> {
        let mut waiting = std::mem::take(&mut self.waiting).into_iter();
        while let Some(job) = waiting.next() {
            // An action whose copy cannot be held is sent untracked: the
            // next snapshot shows what it did.
            let tracked = copy_job(&job);
            if let Some(back) = send(job)? {
                let _ = td_alloc::push(&mut self.waiting, back);
                for later in waiting {
                    let _ = td_alloc::push(&mut self.waiting, later);
                }
                return Ok(());
            }
            if let Some(tracked) = tracked {
                let _ = td_alloc::push(&mut self.flight, tracked);
            }
        }
        Ok(())
    }

    /// Whether an action waits or is in flight.
    pub fn pending(&self) -> bool {
        !self.waiting.is_empty() || !self.flight.is_empty()
    }

    fn answered(&mut self, answered: usize) {
        let answered = answered.min(self.flight.len());
        self.flight.drain(..answered);
    }

    pub fn reply(&mut self, reply: Reply) {
        match reply {
            Reply::Snapshot { snapshot, answered } => {
                self.answered(answered);
                self.snapshot = snapshot;
                // What the daemon has not answered yet still shows as sent,
                // so an older answer does not undo a newer action.
                for job in self.flight.iter().chain(&self.waiting) {
                    show(&mut self.snapshot, job);
                }
                if self.daemon != Daemon::Live {
                    self.daemon = Daemon::Live;
                    self.message.clear();
                }
                self.refocus();
            }
            Reply::Refused(said) => self.say(&said),
            Reply::Unavailable { answered, why } => {
                self.answered(answered);
                // The rows go, and with them what was asked of them.
                self.waiting.clear();
                self.dragging = None;
                self.daemon = Daemon::Unavailable;
                self.snapshot = Snapshot::default();
                self.focus = Key::Master;
                self.at = 0;
                self.first = 0;
                self.say(
                    why.as_deref()
                        .unwrap_or("td-audio is not running; trying again"),
                );
            }
        }
        self.redraw = true;
    }

    fn say(&mut self, text: &str) {
        self.message.clear();
        // A message that cannot be held is not shown.
        let _ = td_alloc::push_str(&mut self.message, text);
        self.redraw = true;
    }

    fn rows(&self) -> impl Iterator<Item = Row<'_>> {
        self.snapshot
            .master
            .map(Row::Master)
            .into_iter()
            .chain(self.snapshot.outputs.iter().map(Row::Output))
            .chain(self.snapshot.apps.iter().map(Row::App))
    }

    fn count(&self) -> usize {
        self.rows().count()
    }

    fn selected(&self) -> usize {
        self.rows()
            .position(|row| row.is(&self.focus))
            .unwrap_or(self.at)
    }

    fn focused(&self) -> Option<Row<'_>> {
        let at = self.selected();
        self.rows().nth(at)
    }

    /// Keep the focus on its row when the rows change, else on the row
    /// now where it was.
    fn refocus(&mut self) {
        let at = self.selected();
        self.select(at);
    }

    fn select(&mut self, index: usize) {
        let last = self.count().saturating_sub(1);
        let index = index.min(last);
        self.at = index;
        let key = self.rows().nth(index).and_then(|row| row.key());
        if let Some(key) = key {
            self.focus = key;
        }
        if let Some(list) = self.layout().list {
            self.first = list.reveal(self.count(), index, self.first);
        }
        self.redraw = true;
    }

    pub fn input(&mut self, input: Input<'_>) {
        match input {
            Input::Resize(surface) => {
                self.surface = surface;
                self.select(self.selected());
            }
            Input::Close => self.quitting = true,
            Input::Key { chord, repeat } => self.key(chord, repeat),
            Input::Pointer { phase, x, y, .. } => self.pointer(phase, x, y),
            Input::CancelPointer => self.dragging = None,
            Input::Wheel { rows, .. } => {
                let at = self.selected();
                self.select(if rows < 0 {
                    at.saturating_sub(rows.unsigned_abs())
                } else {
                    at.saturating_add(rows.unsigned_abs())
                });
            }
            _ => {}
        }
    }

    fn key(&mut self, chord: &str, repeat: bool) {
        let at = self.selected();
        match chord {
            "C-q" | "Escape" => self.quitting = true,
            "Up" | "S-Tab" => self.select(at.saturating_sub(1)),
            "Down" | "Tab" => self.select(at.saturating_add(1)),
            "Home" => self.select(0),
            "End" => self.select(self.count()),
            "Left" | "-" => self.step(false),
            "Right" | "+" | "=" => self.step(true),
            // A held key repeats a step, never a toggle or a switch.
            _ if repeat => {}
            "m" | "M" => self.toggle_mute(),
            "Return" | " " => self.activate(),
            _ => {}
        }
    }

    /// Step the focused level, unmuting it, as a volume key does.
    fn step(&mut self, up: bool) {
        let Some((target, level)) = self.adjustable() else {
            return;
        };
        let to = model::step(level.percent, up, STEP);
        self.set_level(target, to, level.muted);
    }

    /// The level first, then the unmute, as the volume keys write them, so
    /// a muted level is never heard at its old level.
    fn set_level(&mut self, target: Target, percent: u32, was_muted: bool) {
        let unmute = if was_muted { copy(&target) } else { None };
        self.send(Job::Level(target, percent.min(MAX_PERCENT)));
        if let Some(target) = unmute {
            self.send(Job::Mute(target, false));
        }
    }

    /// The focused row's target and level, unless it is the playing
    /// output's while a choice of output is unanswered: the level shown is
    /// still the output's being left, and a step from it would land on the
    /// one chosen.
    fn adjustable(&self) -> Option<(Target, Level)> {
        let row = self.focused()?;
        if matches!(row, Row::Master(_)) && self.choosing() {
            return None;
        }
        Some((row.target()?, row.level()?))
    }

    fn choosing(&self) -> bool {
        self.flight
            .iter()
            .chain(&self.waiting)
            .any(|job| matches!(job, Job::Choose(_)))
    }

    /// Show `job` at once and queue it; a new action clears what the
    /// status row said about an old one.
    fn send(&mut self, job: Job) {
        show(&mut self.snapshot, &job);
        if self.daemon == Daemon::Live {
            self.message.clear();
        }
        queue(&mut self.waiting, job);
        self.redraw = true;
    }

    fn toggle_mute(&mut self) {
        let Some((target, level)) = self.adjustable() else {
            return;
        };
        self.send(Job::Mute(target, !level.muted));
    }

    fn activate(&mut self) {
        let chosen = match self.focused() {
            Some(Row::Output(output)) if !output.playing => td_alloc::string(&output.name).ok(),
            Some(Row::Master(_) | Row::App(_)) => {
                self.toggle_mute();
                return;
            }
            _ => None,
        };
        if let Some(name) = chosen {
            self.send(Job::Choose(name));
        }
    }

    fn pointer(&mut self, phase: PointerPhase, x: i64, y: i64) {
        let layout = self.layout();
        match phase {
            PointerPhase::Press => {
                if StatusRow::new(self.surface).rect().contains(x, y) {
                    self.show_keys = true;
                    return;
                }
                if let Some(row) = layout.list.and_then(|list| list.hit(x, y)) {
                    // The list's rows below the last are empty.
                    let index = self.first.saturating_add(row);
                    if index < self.count() {
                        self.select(index);
                        if matches!(self.focused(), Some(Row::Output(_))) {
                            self.activate();
                        }
                    }
                    return;
                }
                if layout.slider.is_some_and(|slider| slider.hit(x, y)) {
                    self.dragging = self
                        .focused()
                        .filter(|row| row.level().is_some())
                        .and_then(|row| row.key());
                    self.slide(x);
                }
            }
            PointerPhase::Move if self.dragging.is_some() => self.slide(x),
            PointerPhase::Move => {}
            PointerPhase::Release => {
                if self.dragging.is_some() {
                    self.slide(x);
                }
                self.dragging = None;
            }
        }
    }

    /// Set the dragged row's level from `x`; the drag ends once the focus
    /// is no longer on the row it began on.
    fn slide(&mut self, x: i64) {
        let Some(slider) = self.layout().slider else {
            return;
        };
        let Some(dragged) = self.dragging.as_ref() else {
            return;
        };
        let Some((target, level)) = self
            .focused()
            .filter(|row| row.is(dragged))
            .and_then(|_| self.adjustable())
        else {
            self.dragging = None;
            return;
        };
        let to = u32::try_from(slider.value_at(x, MAX_PERCENT as usize)).unwrap_or(MAX_PERCENT);
        if to != level.percent || level.muted {
            self.set_level(target, to, level.muted);
        }
    }

    fn layout(&self) -> Layout {
        let surface = self.surface;
        let s = surface.scale.value();
        let status = StatusRow::new(surface).rect();
        let margin = (MARGIN * s) as i64;
        let row = (ROW * s) as i64;
        let width = i64::try_from(surface.width).unwrap_or(0);
        let band = Rect {
            x: margin,
            y: status.y - margin - row,
            width: u32::try_from(width - 2 * margin).unwrap_or(0),
            height: u32::try_from(row).unwrap_or(0),
        };
        let list = Rect {
            x: margin,
            y: margin,
            width: band.width,
            height: u32::try_from(band.y - 2 * margin).unwrap_or(0),
        };
        let cell = (CELL_WIDTH * s) as i64;
        let label = cell * LABEL_CELLS as i64;
        let value = cell * VALUE_CELLS as i64;
        let track = Rect {
            x: band.x + label,
            y: band.y,
            width: u32::try_from(i64::from(band.width) - label - value).unwrap_or(0),
            height: band.height,
        };
        Layout {
            list: List::new(surface, list),
            slider: Slider::new(surface, track),
            band,
        }
    }

    /// The status row's text: the last message, else the keys.
    pub fn status_line(&self) -> String {
        let shown = if self.message.is_empty() {
            "Left/Right: level  m: mute  Return: choose output"
        } else {
            self.message.as_str()
        };
        td_alloc::format(format_args!("{}: keys  {shown}", keys::CHORD)).unwrap_or_default()
    }

    pub fn frame(&self) -> impl Composition + '_ {
        Frame { app: self }
    }

    pub fn paint(&mut self, raster: &mut Raster<'_, '_>, surface: Surface) -> Result<(), String> {
        if surface != self.surface {
            self.surface = surface;
            self.select(self.selected());
        }
        self.redraw = false;
        raster
            .paint(&Frame { app: self }, surface.bounds())
            .map_err(|error| td_alloc::format(format_args!("{error}")).unwrap_or_default())
    }
}

/// Add `job`, replacing a waiting one it makes moot: a level or mute of
/// the same target, or another choice of output, so a drag or a held key
/// sends only where it ended. The one it replaces keeps its place, so a
/// level asked before an unmute stays before it however often it is
/// replaced. No level or mute of the playing output waits behind a choice
/// of output (`adjustable`), so none is replaced across one.
fn queue(jobs: &mut Vec<Job>, job: Job) {
    if let Some(slot) = jobs.iter_mut().find(|waiting| supersedes(&job, waiting)) {
        *slot = job;
        return;
    }
    // An action that cannot be held is not sent; the next snapshot shows
    // what the daemon has.
    let _ = td_alloc::push(jobs, job);
}

fn copy(target: &Target) -> Option<Target> {
    Some(match target {
        Target::Master => Target::Master,
        Target::App(name) => Target::App(td_alloc::string(name).ok()?),
    })
}

fn copy_job(job: &Job) -> Option<Job> {
    Some(match job {
        Job::Choose(name) => Job::Choose(td_alloc::string(name).ok()?),
        Job::Level(target, percent) => Job::Level(copy(target)?, *percent),
        Job::Mute(target, muted) => Job::Mute(copy(target)?, *muted),
    })
}

/// `job` as the snapshot will show it once the daemon has applied it.
fn show(snapshot: &mut Snapshot, job: &Job) {
    match job {
        Job::Choose(name) => {
            for output in &mut snapshot.outputs {
                output.playing = &output.name == name;
            }
        }
        Job::Level(target, percent) => {
            if let Some(level) = level_of(snapshot, target) {
                level.percent = *percent;
            }
        }
        Job::Mute(target, muted) => {
            if let Some(level) = level_of(snapshot, target) {
                level.muted = *muted;
            }
        }
    }
}

fn level_of<'a>(snapshot: &'a mut Snapshot, target: &Target) -> Option<&'a mut Level> {
    match target {
        Target::Master => snapshot.master.as_mut(),
        Target::App(name) => snapshot
            .apps
            .iter_mut()
            .find(|app| &app.name == name)
            .map(|app| &mut app.level),
    }
}

fn supersedes(new: &Job, old: &Job) -> bool {
    match (new, old) {
        (Job::Choose(_), Job::Choose(_)) => true,
        (Job::Level(one, _), Job::Level(two, _)) | (Job::Mute(one, _), Job::Mute(two, _)) => {
            one == two
        }
        _ => false,
    }
}

/// A level as a row's right column says it.
fn meta(level: Level) -> String {
    let said = if level.muted {
        td_alloc::format(format_args!("MUTED {}%", level.percent))
    } else {
        td_alloc::format(format_args!("{}%", level.percent))
    };
    said.unwrap_or_default()
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
        let surface = app.surface;
        let s = surface.scale;
        let bounds = surface.bounds();
        if let Some(clip) = bounds.intersection(damage) {
            sink(Draw {
                clip,
                primitive: Primitive::Fill {
                    rect: bounds,
                    color: PAPER,
                },
            });
        }
        let layout = app.layout();
        let selected = app.selected();
        if let Some(list) = layout.list {
            let mut metas = Vec::new();
            for row in app.rows().skip(app.first).take(list.rows()) {
                let said = match row {
                    Row::Master(level) => meta(level),
                    Row::App(entry) => meta(entry.level),
                    Row::Output(output) if output.playing => {
                        td_alloc::string("PLAYING").unwrap_or_default()
                    }
                    Row::Output(_) => String::new(),
                };
                // A row whose column cannot be held shows none.
                let _ = td_alloc::push(&mut metas, said);
            }
            let items = app
                .rows()
                .skip(app.first)
                .zip(
                    metas
                        .iter()
                        .map(String::as_str)
                        .chain(std::iter::repeat("")),
                )
                .take(list.rows())
                .map(|(row, meta)| match row {
                    Row::Master(_) => Item {
                        label: "Volume",
                        meta,
                        enabled: true,
                        marked: false,
                    },
                    Row::Output(output) => Item {
                        label: &output.label,
                        meta,
                        enabled: true,
                        marked: output.playing,
                    },
                    Row::App(entry) => Item {
                        label: &entry.label,
                        meta,
                        enabled: true,
                        marked: false,
                    },
                });
            list.emit(items, app.first, selected, app.count(), damage, sink);
        }
        let focused = app.focused();
        let level = focused.as_ref().and_then(Row::level);
        let name = match &focused {
            Some(Row::Master(_)) => "Volume",
            Some(Row::App(entry)) => entry.label.as_str(),
            Some(Row::Output(_)) => "Output",
            None => match app.daemon {
                Daemon::Waiting => "Connecting",
                Daemon::Live | Daemon::Unavailable => "No audio",
            },
        };
        let style = GlyphStyle::medium(INK, PAPER);
        let cell = (CELL_WIDTH * s.value()) as i64;
        let text_y = layout.band.y
            + (i64::from(layout.band.height) - (td_ui::CELL_HEIGHT * s.value()) as i64) / 2;
        let label_box = Rect {
            width: u32::try_from(cell * (LABEL_CELLS as i64 - 1)).unwrap_or(0),
            ..layout.band
        };
        raster::text_run(
            s,
            name.chars(),
            (layout.band.x, text_y),
            label_box,
            style,
            damage,
            sink,
        );
        if let Some(slider) = layout.slider {
            let value = level.map_or(0, |level| level.percent as usize);
            slider.emit(
                value,
                MAX_PERCENT as usize,
                // Drawn disabled while it would do nothing: muted, or the
                // playing output's while a choice of output is unanswered.
                level.is_some_and(|level| !level.muted)
                    && !(matches!(focused, Some(Row::Master(_))) && app.choosing()),
                damage,
                sink,
            );
            let said = level.map(meta).unwrap_or_default();
            let right = slider.rect().x + i64::from(slider.rect().width) + cell;
            let value_box = Rect {
                x: right,
                width: u32::try_from(cell * (VALUE_CELLS as i64 - 1)).unwrap_or(0),
                ..layout.band
            };
            raster::text_run(
                s,
                said.chars(),
                (right, text_y),
                value_box,
                style,
                damage,
                sink,
            );
        }
        StatusRow::new(surface).emit(app.status_line().chars(), damage, sink);
    }
}
