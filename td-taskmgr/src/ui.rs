//! One bounded task-manager interaction model shared by preview and Wayland.
use crate::budget::{Budget, Charge, MemoryVec};
use crate::contributors::{Colors, Metric};
use crate::device_selection::Selection;
use crate::format::Text;
use crate::history::{Interval, SampleId};
use crate::model::{Admission, Model};
use crate::plots::{Id, Kind, Plot};
use crate::projection::{Expansion, Key, Sort};
use crate::ranking::Ranking;
use crate::search::Search;
use crate::view::View;
use crate::worker::{Failure, Update};
use std::fmt::Write;
use std::sync::Arc;
use td_ui::raster::{
    self, Draw, GlyphStyle, Primitive, Rect, Surface, CHROME, INK, PAPER, SELECTED,
};
use td_ui::{charts, chrome, split, tree_table as tree};
const TABS: [&str; 5] = ["Overview", "CPU", "Memory", "Network", "Disk"];
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Focus {
    Tabs,
    Graph,
    Devices,
    Divider,
    Search,
    Tree,
    Actions,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    Press,
    Move,
    Release,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    Changed,
    Ignored,
    Interval(Interval),
    Quit,
}
#[derive(Clone, Copy, Debug)]
struct Slot {
    index: usize,
    rect: Rect,
    kind: Option<Kind>,
}
#[derive(Debug)]
struct Cached {
    slot: Slot,
    plot: Plot,
    state: charts::State<Id, SampleId>,
}
#[derive(Debug)]
pub struct State {
    pub model: Model,
    actions: crate::action_ui::Controller,
    budget: Arc<Budget>,
    surface: Surface,
    split: split::Controller,
    view: Option<View>,
    expansion: Expansion,
    sort: Sort,
    search: Search,
    tab: usize,
    focus: Focus,
    graph_first: usize,
    graph_focus: usize,
    graphs: MemoryVec<Cached>,
    devices: Selection,
    ranking: Option<Ranking>,
    device_first: usize,
    device_focus: usize,
    cpu_colors: Colors,
    rss_colors: Colors,
    held: bool,
    armed: Option<(i64, i64)>,
    now_ns: u64,
    notice: Text<256>,
    dirty: bool,
    refresh_pending: bool,
    group_selected: bool,
    keyboard_focus: bool,
    selection_refresh: bool,
    _charge: Charge,
}
fn error(e: impl std::fmt::Display) -> String {
    e.to_string()
}
fn region(surface: Surface) -> Rect {
    let s = surface.scale.value() as u32;
    let height = surface.height as u32;
    let y = (48 * s).min(height);
    Rect {
        x: 0,
        y: i64::from(y),
        width: surface.width as u32,
        height: height.saturating_sub(y + 24 * s),
    }
}
fn inset(rect: Rect, amount: u32) -> Rect {
    Rect {
        x: rect.x + i64::from(amount.min(rect.width)),
        y: rect.y + i64::from(amount.min(rect.height)),
        width: rect.width.saturating_sub(amount * 2),
        height: rect.height.saturating_sub(amount * 2),
    }
}
fn below(rect: Rect, height: u32) -> Rect {
    let offset = height.min(rect.height);
    Rect {
        y: rect.y + i64::from(offset),
        height: rect.height - offset,
        ..rect
    }
}
fn fill(rect: Rect, color: u32, damage: Rect, sink: &mut dyn FnMut(Draw)) {
    if let Some(clip) = rect.intersection(damage) {
        sink(Draw {
            clip,
            primitive: Primitive::Fill { rect, color },
        });
    }
}
fn label(
    surface: Surface,
    rect: Rect,
    text: &str,
    selected: bool,
    damage: Rect,
    sink: &mut dyn FnMut(Draw),
) {
    let background = if selected { SELECTED } else { CHROME };
    fill(rect, background, damage, sink);
    raster::text_run(
        surface.scale,
        text.chars(),
        (
            rect.x + 8 * surface.scale.value() as i64,
            rect.y + 4 * surface.scale.value() as i64,
        ),
        rect,
        GlyphStyle::medium(if selected { PAPER } else { INK }, background),
        damage,
        sink,
    );
}
fn button(
    surface: Surface,
    rect: Rect,
    text: &str,
    selected: bool,
    damage: Rect,
    sink: &mut dyn FnMut(Draw),
) {
    if let Some(button) = chrome::Button::new(surface, rect) {
        button.emit(text, selected, true, damage, sink);
    }
}
impl State {
    pub fn new(budget: &Arc<Budget>, surface: Surface) -> Result<Self, String> {
        Ok(Self {
            model: Model::new(budget, Interval::Second).map_err(error)?,
            actions: crate::action_ui::Controller::new(budget, surface)?,
            budget: Arc::clone(budget),
            surface,
            split: split::Controller::new(
                split::Config {
                    axis: split::Axis::Vertical,
                    first_min: 216,
                    second_min: 96,
                },
                split::Share::default(),
                surface,
                region(surface),
            )
            .map_err(error)?,
            view: None,
            expansion: Expansion::new(budget).map_err(error)?,
            sort: Sort::default(),
            search: Search::default(),
            tab: 0,
            focus: Focus::Tree,
            graph_first: 0,
            graph_focus: 0,
            graphs: MemoryVec::new(budget, 32).map_err(error)?,
            devices: Selection::new(budget)?,
            ranking: None,
            device_first: 0,
            device_focus: 0,
            cpu_colors: Colors::default(),
            rss_colors: Colors::default(),
            held: false,
            armed: None,
            now_ns: 0,
            notice: Text::default(),
            dirty: true,
            refresh_pending: false,
            group_selected: false,
            keyboard_focus: true,
            selection_refresh: false,
            _charge: budget.charge(std::mem::size_of::<Self>()).map_err(error)?,
        })
    }
    fn action_note(&mut self) {
        if let Some(note) = self.actions.take_note() {
            self.note(note.as_str());
        }
    }
    pub fn action_update(&mut self, update: crate::actions::Update) {
        self.actions.update(update);
        self.action_note();
        self.dirty = true;
    }
    pub fn action_unavailable(&mut self, reason: &str) {
        self.actions.disable(reason);
        self.action_note();
        self.dirty = true;
    }
    pub fn take_action_command(&mut self) -> Option<crate::actions::Command> {
        self.actions.take_command()
    }
    pub fn context_menu(&mut self, point: (i64, i64)) {
        if self.actions.busy() {
            return;
        }
        let target = self
            .view
            .as_ref()
            .and_then(|view| view.table.target(point.0, point.1));
        let key = match target {
            Some(tree::Target::Row(key) | tree::Target::Disclosure { id: key, .. }) => key,
            _ => return,
        };
        self.cancel_gesture();
        self.set_focus(Focus::Tree);
        if let Some(view) = &mut self.view {
            view.table.select(Some(key), false);
        }
        self.tree_outcome(tree::Outcome::Selected(key));
        self.process_menu(Some(point));
    }
    pub fn process_menu(&mut self, point: Option<(i64, i64)>) {
        self.cancel_gesture();
        let key = self.selected().and_then(|key| match key {
            Key::Process(key) => Some(key),
            Key::Unavailable => None,
        });
        self.actions.open(key, self.model.historical(), point);
        self.action_note();
        self.dirty = true;
    }
    pub fn surface(&self) -> Surface {
        self.surface
    }
    pub fn dirty(&self) -> bool {
        self.dirty
    }
    pub fn painted(&mut self) {
        self.dirty = false;
    }
    pub fn tab(&self) -> usize {
        self.tab
    }
    pub fn focus(&self) -> Focus {
        self.focus
    }
    pub fn query(&self) -> &str {
        self.search.text()
    }
    pub fn visible(&self) -> usize {
        self.view
            .as_ref()
            .map(|v| v.table.model().rows().len())
            .unwrap_or(0)
    }
    pub fn selected(&self) -> Option<Key> {
        self.view.as_ref().and_then(|v| v.table.selected())
    }
    pub fn notice(&self) -> &str {
        self.notice.as_str()
    }
    pub fn note(&mut self, text: &str) {
        let next = Text::truncated(text);
        if self.notice.as_str() != next.as_str() {
            self.notice = next;
            self.dirty = true;
        }
    }
    pub fn resize(&mut self, surface: Surface) -> Result<(), String> {
        if self.surface == surface {
            return Ok(());
        }
        self.surface = surface;
        self.actions.resize(surface);
        self.action_note();
        self.cancel_gesture();
        self.split
            .event(split::Event::Resize {
                surface,
                rect: region(surface),
            })
            .map_err(error)?;
        self.refresh(false);
        self.dirty = true;
        Ok(())
    }
    pub fn cancel_gesture(&mut self) {
        self.actions.cancel_gesture();
        self.dirty = true;
        self.held = false;
        self.armed = None;
        let _ = self.split.event(split::Event::FocusLost);
        if self.keyboard_focus && self.focus == Focus::Divider {
            let _ = self.split.event(split::Event::Focus);
        }
        if let Some(view) = &mut self.view {
            view.table.event(tree::Event::Other);
        }
        for graph in self.graphs.iter_mut() {
            graph.state.cancel_gesture();
        }
    }
    pub fn cancel(&mut self) {
        self.actions.focus_lost();
        self.action_note();
        self.keyboard_focus = false;
        self.cancel_gesture();
        let _ = self.split.event(split::Event::FocusLost);
        if let Some(view) = &mut self.view {
            view.table.event(tree::Event::FocusLost);
        }
    }
    pub fn restore_focus(&mut self) {
        self.keyboard_focus = true;
        if self.keyboard_focus && self.focus == Focus::Divider {
            let _ = self.split.event(split::Event::Focus);
        }
        if let Some(view) = &mut self.view {
            let _ = view
                .table
                .set_focus(if self.keyboard_focus && self.focus == Focus::Tree {
                    tree::Focus::Rows
                } else {
                    tree::Focus::None
                });
        }
        self.dirty = true;
    }
    fn set_focus(&mut self, focus: Focus) {
        self.focus = focus;
        if matches!(self.tab, 3 | 4) && matches!(focus, Focus::Devices | Focus::Graph) {
            let first = usize::from(focus == Focus::Devices);
            if !self
                .slots()
                .into_iter()
                .flatten()
                .any(|slot| slot.index == first)
            {
                self.graph_first = first;
                self.refresh(false);
            }
            self.reveal_device();
        }
        if self.keyboard_focus && focus == Focus::Divider {
            let _ = self.split.event(split::Event::Focus);
        } else if !self.split.dragging() {
            let _ = self.split.event(split::Event::FocusLost);
        }
        if let Some(view) = &mut self.view {
            let _ = view
                .table
                .set_focus(if self.keyboard_focus && focus == Focus::Tree {
                    tree::Focus::Rows
                } else {
                    tree::Focus::None
                });
        }
        self.dirty = true;
    }
    pub fn update(&mut self, update: Update, now_ns: u64) {
        let previous_second = self.now_ns / 1_000_000_000;
        self.now_ns = now_ns;
        self.model.receive(update);
        self.flush_selection();
        let mut reclaimed = false;
        if self.actions.needs_result_memory() {
            self.actions.retry_results();
            if self.actions.needs_result_memory() {
                reclaimed = self.model.reclaim_for_view();
            }
            self.action_note();
            self.dirty = true;
        }
        if self.refresh_pending {
            // Recover the visible working set before admitting another snapshot.
            self.refresh(false);
            if self.refresh_pending && !reclaimed && self.model.reclaim_for_view() {
                self.dirty = true;
            }
        } else if !self.held && !self.actions.busy() {
            match self.model.admit_pending() {
                Admission::Admitted(_) => {
                    if [
                        "Collection ",
                        "History memory limit",
                        "Resource observation failed",
                    ]
                    .iter()
                    .any(|prefix| self.notice.as_str().starts_with(prefix))
                    {
                        self.notice.clear();
                    }
                    self.refresh(false);
                    self.dirty = true;
                }
                Admission::Reclaimed => {
                    self.dirty = true;
                }
                Admission::Blocked => self
                    .note("History memory limit. Return to Live to release the inspected sample."),
                Admission::Rejected => self.note("Resource observation failed validation."),
                Admission::Idle => {}
            }
        }
        if let Some(failure) = self.model.failure() {
            match failure {
                Failure::Read(kind) => {
                    let mut message = Text::<128>::new("Collection unavailable: ");
                    let _ = write!(message, "{kind:?}; retaining last observation");
                    self.note(message.as_str());
                }
                Failure::InvalidObservation => self.note("Resource observation failed validation."),
                Failure::Memory => self.note("Collection memory limit; retaining inspected data."),
            }
        }
        if previous_second != now_ns / 1_000_000_000 {
            self.dirty = true;
        }
    }
    fn card_count(&self) -> usize {
        match self.tab {
            0 => 4,
            1 => {
                2 + self
                    .model
                    .history()
                    .selected()
                    .map(|s| s.value.cpus.len())
                    .unwrap_or(0)
            }
            2 => 3,
            3 | 4 => 2,
            _ => 0,
        }
    }
    fn kind(&self, index: usize) -> Option<Kind> {
        match self.tab {
            0 => [Kind::Cpu, Kind::Memory, Kind::Network, Kind::Disk]
                .get(index)
                .copied(),
            1 => match index {
                0 => Some(Kind::Cpu),
                1 => Some(Kind::ProcessCpu),
                _ => self
                    .model
                    .history()
                    .selected()
                    .and_then(|s| s.value.cpus.get(index - 2))
                    .map(|cpu| Kind::Core(cpu.id)),
            },
            2 => [Kind::Memory, Kind::ProcessRss, Kind::Swap]
                .get(index)
                .copied(),
            3 => (index == 0).then_some(Kind::Network),
            4 => (index == 0).then_some(Kind::Disk),
            _ => None,
        }
    }
    fn slots(&self) -> [Option<Slot>; 32] {
        let mut slots = [None; 32];
        let Some(layout) = self.split.layout() else {
            return slots;
        };
        let s = self.surface.scale.value() as u32;
        let area = below(layout.first, 24 * s);
        let columns = if area.width >= 1024 * s { 2 } else { 1 };
        let rows = if self.tab == 0 {
            (area.height / (176 * s)).clamp(1, 2)
        } else {
            1
        };
        let count = self.card_count();
        let first = (self.graph_first / columns as usize) * columns as usize;
        for (position, slot) in slots.iter_mut().take((rows * columns) as usize).enumerate() {
            let index = first + position;
            if index >= count {
                break;
            }
            let col = position as u32 % columns;
            let row = position as u32 / columns;
            let left = area.width * col / columns;
            let right = area.width * (col + 1) / columns;
            let top = area.height * row / rows;
            let bottom = area.height * (row + 1) / rows;
            *slot = Some(Slot {
                index,
                kind: self.kind(index),
                rect: inset(
                    Rect {
                        x: area.x + i64::from(left),
                        y: area.y + i64::from(top),
                        width: right - left,
                        height: bottom - top,
                    },
                    4 * s,
                ),
            });
        }
        slots
    }
    fn refresh(&mut self, rebuild_tree: bool) {
        self.refresh_pending = false;
        let result = self.try_refresh(rebuild_tree).or_else(|_| {
            self.view = None;
            self.graphs.clear();
            self.try_refresh(true)
        });
        if let Err(why) = result {
            self.refresh_pending = [
                crate::budget::Error::Limit.to_string(),
                crate::budget::Error::Allocation.to_string(),
                tree::ModelError::Allocation.to_string(),
            ]
            .contains(&why);
            // Never paint an older projection as if it were the selected sample.
            self.view = None;
            self.graphs.clear();
            self.note(&why);
        } else if self.notice.as_str().contains("memory budget")
            || self.notice.as_str().contains("allocation failed")
        {
            self.notice.clear();
        }
        self.dirty = true;
    }
    fn try_refresh(&mut self, rebuild_tree: bool) -> Result<(), String> {
        let Some(sample) = self.model.history().selected() else {
            return Ok(());
        };
        let mut previous = [None; 32];
        for (slot, graph) in previous.iter_mut().zip(self.graphs.iter()) {
            *slot = Some((
                graph.plot.kind,
                graph.state.selection().and_then(|s| s.series),
            ));
        }
        // Release replaced plots before charging the next visible working set.
        self.graphs.clear();
        self.expansion.retain_snapshot(&sample.value.processes);
        if let Some(devices) = &sample.value.devices {
            self.devices.initialize(devices)?;
        }
        if let Some(layout) = self.split.layout() {
            let rect = below(layout.second, 24 * self.surface.scale.value() as u32);
            let input = crate::view::Inputs {
                sample: sample.id,
                snapshot: &sample.value.processes,
                sort: self.sort,
                query: self.search.text(),
                selected: self.model.selected(),
                expansion: &self.expansion,
            };
            if let Some(view) = &mut self.view {
                if rebuild_tree || view.sample != sample.id {
                    view.replace(&self.budget, input)?;
                }
                view.table.resize(self.surface, rect).map_err(error)?;
            } else {
                self.view = Some(View::new(&self.budget, input, self.surface, rect)?);
            }
            if let Some(view) = &mut self.view {
                view.reserve_paint()?;
                if self.group_selected {
                    view.table.select(Some(Key::Unavailable), rebuild_tree);
                } else if let Some(selected) = self.model.selected() {
                    view.table
                        .select(Some(Key::Process(selected)), rebuild_tree);
                }
                view.table
                    .set_focus(if self.keyboard_focus && self.focus == Focus::Tree {
                        tree::Focus::Rows
                    } else {
                        tree::Focus::None
                    })
                    .map_err(error)?;
            }
        }
        self.graph_first = self.graph_first.min(self.card_count().saturating_sub(1));
        let slots = self.slots();
        for slot in slots.into_iter().flatten() {
            let Some(kind) = slot.kind else { continue };
            let colors = if kind == Kind::ProcessRss {
                &mut self.rss_colors
            } else {
                &mut self.cpu_colors
            };
            let plot = Plot::new(
                &self.budget,
                self.model.history(),
                kind,
                self.model.selected(),
                colors,
                &self.devices,
            )?;
            let previous = previous
                .iter()
                .flatten()
                .find(|(previous_kind, _)| *previous_kind == kind)
                .and_then(|(_, series)| *series);
            let preferred = if matches!(kind, Kind::ProcessCpu | Kind::ProcessRss) {
                self.model.selected().map(Id::Process).or(previous)
            } else {
                previous
            };
            let mut state = charts::State::default();
            let at = if self.model.historical() {
                Some(sample.time_ns)
            } else {
                plot.latest_selection().map(|selection| selection.at)
            };
            state.select(at.map(|at| {
                if self.group_selected && matches!(kind, Kind::ProcessCpu | Kind::ProcessRss) {
                    charts::Selection { at, series: None }
                } else {
                    plot.selection(at, preferred)
                }
            }));
            self.graphs
                .push(Cached { slot, plot, state })
                .map_err(|_| "visible graph limit")?;
        }
        let ranking_rows = self.ranking_list().map(|list| list.rows()).unwrap_or(0);
        if let Some(ranking) = &mut self.ranking {
            ranking.reserve_paint(ranking_rows).map_err(error)?;
        }
        self.graph_focus = self.graph_focus.min(self.graphs.len().saturating_sub(1));
        self.reveal_device();
        Ok(())
    }
    fn choose_tab(&mut self, index: usize) {
        self.ranking = None;
        if index < TABS.len() {
            self.tab = index;
            self.graph_first = 0;
            self.graph_focus = 0;
            self.device_first = 0;
            self.device_focus = 0;
            if self.focus == Focus::Devices {
                if matches!(index, 3 | 4) {
                    self.graph_first = 1;
                } else {
                    self.set_focus(Focus::Graph);
                }
            }
            self.refresh(false);
            self.dirty = true;
        }
    }
    fn live(&mut self) {
        self.ranking = None;
        self.model.live();
        self.notice.clear();
        self.refresh(true);
        self.dirty = true;
    }
    fn all_processes(&mut self) {
        self.cancel_gesture();
        self.ranking = None;
        self.group_selected = false;
        self.model.clear_selection();
        if let Some(view) = &mut self.view {
            view.table.select(None, false);
        }
        self.refresh(true);
    }
    fn interval(&mut self) -> Outcome {
        let interval = match self.model.history().interval() {
            Interval::HalfSecond => Interval::Second,
            Interval::Second => Interval::TwoSeconds,
            Interval::TwoSeconds => Interval::FiveSeconds,
            Interval::FiveSeconds => Interval::HalfSecond,
        };
        self.model.set_interval(interval);
        self.refresh(false);
        self.dirty = true;
        Outcome::Interval(interval)
    }
    fn tree_outcome(&mut self, outcome: tree::Outcome<Key>) {
        match outcome {
            tree::Outcome::Selected(Key::Unavailable) => {
                self.group_selected = true;
                self.model.clear_selection();
                self.selection_refresh = self.held;
                self.refresh(!self.held);
            }
            tree::Outcome::Selected(Key::Process(key)) => {
                self.group_selected = false;
                self.model.select(key);
                self.selection_refresh = self.held && !self.search.text().is_empty();
                self.refresh(!self.held && !self.search.text().is_empty());
            }
            tree::Outcome::Disclosure { id, expanded } => {
                if let Err(e) = self.expansion.set(id, expanded) {
                    self.note(&e.to_string());
                }
                self.refresh(true);
            }
            tree::Outcome::Sort(column) => {
                if let Some(column) = crate::view::column(column) {
                    self.sort.click(column);
                    self.refresh(true);
                    if let Some(view) = &mut self.view {
                        view.table.event(tree::Event::Scroll {
                            rows: i64::MIN + 1,
                            columns: 0,
                        });
                    }
                }
            }
            tree::Outcome::Activate(_) => self.process_menu(None),
            _ => {}
        }
        self.dirty = true;
    }
    fn graph_outcome(&mut self, outcome: charts::Outcome<Id>, kind: Kind) {
        if let charts::Outcome::Selected(mut selection) = outcome {
            // Chart-only gap markers are navigable; resolve in the travel direction.
            let history = self.model.history();
            if !history
                .samples()
                .iter()
                .any(|sample| sample.time_ns == selection.at)
            {
                let current = history.selected().map(|sample| sample.time_ns).unwrap_or(0);
                let target = if selection.at > current {
                    history
                        .samples()
                        .iter()
                        .find(|sample| sample.time_ns >= selection.at)
                } else {
                    history
                        .samples()
                        .iter()
                        .rev()
                        .find(|sample| sample.time_ns <= selection.at)
                };
                if let Some(target) = target {
                    selection.at = target.time_ns;
                }
            }
            match selection.series {
                Some(Id::Process(key)) => {
                    if self.model.select_contributor(selection.at, key) {
                        self.group_selected = false;
                        if let Some(sample) = self.model.history().selected() {
                            self.expansion.reveal(&sample.value.processes, key);
                        }
                        self.refresh(true);
                    } else {
                        self.model.inspect(selection.at);
                        self.refresh(true);
                        self.note("Process not observed at this time.");
                    }
                }
                _ => {
                    self.model.inspect(selection.at);
                    if matches!(selection.series, None | Some(Id::Other))
                        && matches!(
                            kind,
                            Kind::Cpu | Kind::Memory | Kind::ProcessCpu | Kind::ProcessRss
                        )
                    {
                        let metric = if matches!(kind, Kind::Cpu | Kind::ProcessCpu) {
                            Metric::Cpu
                        } else {
                            Metric::Rss
                        };
                        if let Some(sample) = self.model.history().selected() {
                            match Ranking::new(
                                &self.budget,
                                sample.id,
                                &sample.value.processes,
                                metric,
                            ) {
                                Ok(ranking) => {
                                    self.ranking = Some(ranking);
                                }
                                Err(why) => self.note(&why.to_string()),
                            }
                        }
                    }
                    self.refresh(true);
                }
            }
            self.dirty = true;
        }
    }
    fn search_rect(&self) -> Option<Rect> {
        let layout = self.split.layout()?;
        let s = self.surface.scale.value() as u32;
        Some(Rect {
            width: layout.second.width.saturating_sub(184 * s),
            height: (24 * s).min(layout.second.height),
            ..layout.second
        })
    }
    fn actions_rect(&self) -> Option<Rect> {
        let layout = self.split.layout()?;
        let entry = self.search_rect()?;
        Some(Rect {
            x: entry.x + i64::from(entry.width),
            width: layout.second.width - entry.width,
            ..entry
        })
    }
    fn device_list(&self) -> Option<chrome::List> {
        self.slots()
            .into_iter()
            .flatten()
            .find(|slot| slot.kind.is_none())
            .and_then(|slot| chrome::List::new(self.surface, self.device_rows_rect(slot.rect)))
    }
    fn device_rows_rect(&self, rect: Rect) -> Rect {
        let area = below(rect, 48 * self.surface.scale.value() as u32);
        Rect {
            height: area
                .height
                .saturating_sub(72 * self.surface.scale.value() as u32),
            ..area
        }
    }
    fn device_count(&self) -> usize {
        self.model
            .history()
            .selected()
            .and_then(|s| s.value.devices.as_ref())
            .map(|d| {
                if self.tab == 3 {
                    d.networks.len()
                } else {
                    d.disks.len()
                }
            })
            .unwrap_or(0)
    }
    fn reveal_device(&mut self) {
        let count = self.device_count();
        self.device_focus = self.device_focus.min(count.saturating_sub(1));
        self.device_first = self.device_first.min(count.saturating_sub(1));
        if let Some(list) = self.device_list() {
            self.device_first = list.reveal(count, self.device_focus, self.device_first);
        }
    }
    fn toggle_device(&mut self) {
        let result = if let Some(sample) = self.model.history().selected() {
            if let Some(devices) = &sample.value.devices {
                if self.tab == 3 {
                    self.devices.toggle_network(devices, self.device_focus)
                } else {
                    self.devices.toggle_disk(devices, self.device_focus)
                }
            } else {
                Err("Device observations unavailable".into())
            }
        } else {
            Err("No retained observation".into())
        };
        if let Err(why) = result {
            self.note(&why);
        } else {
            self.refresh(false);
            self.dirty = true;
        }
    }
    fn ranking_list(&self) -> Option<chrome::List> {
        self.ranking.as_ref()?;
        self.split.layout().and_then(|l| {
            chrome::List::new(
                self.surface,
                below(l.first, 24 * self.surface.scale.value() as u32),
            )
        })
    }
    fn select_ranked(&mut self) {
        let key = self.ranking.as_ref().and_then(|ranking| {
            let snapshot = &self
                .model
                .history()
                .samples()
                .iter()
                .find(|s| s.id == ranking.sample)?
                .value
                .processes;
            snapshot
                .processes()
                .get(*ranking.rows().get(ranking.selected)?)
                .map(|p| p.key)
        });
        if let Some(key) = key {
            self.group_selected = false;
            self.model.select(key);
            if let Some(sample) = self.model.history().selected() {
                self.expansion.reveal(&sample.value.processes, key);
            }
            self.refresh(true);
            self.dirty = true;
        }
    }
    fn ranking_key(&mut self, key: &str, repeated: bool) -> Outcome {
        let list = self.ranking_list();
        let Some(ranking) = &mut self.ranking else {
            return Outcome::Ignored;
        };
        let last = ranking.rows().len().saturating_sub(1);
        let page = list.map(|l| l.rows()).unwrap_or(1);
        match key {
            "Up" => ranking.selected = ranking.selected.saturating_sub(1),
            "Down" => ranking.selected = (ranking.selected + 1).min(last),
            "Home" => ranking.selected = 0,
            "End" => ranking.selected = last,
            "PageUp" => ranking.selected = ranking.selected.saturating_sub(page),
            "PageDown" => ranking.selected = (ranking.selected + page).min(last),
            "Return" | "Space" => {
                if !repeated {
                    self.select_ranked();
                }
                self.dirty = true;
                return Outcome::Changed;
            }
            _ => return Outcome::Ignored,
        }
        if let Some(list) = list {
            ranking.first = list.reveal(ranking.rows().len(), ranking.selected, ranking.first);
        }
        self.dirty = true;
        Outcome::Changed
    }
    fn emit_ranking(&self, rect: Rect, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let Some(ranking) = &self.ranking else { return };
        let Some(sample) = self
            .model
            .history()
            .samples()
            .iter()
            .find(|s| s.id == ranking.sample)
        else {
            return;
        };
        let header = if ranking.metric == Metric::Cpu {
            "CPU contributors at inspected time. Enter reveals. Esc returns to graphs."
        } else {
            "RSS contributors (shared pages counted). Enter reveals. Esc returns to graphs."
        };
        label(
            self.surface,
            Rect {
                height: 24 * self.surface.scale.value() as u32,
                ..rect
            },
            header,
            self.focus == Focus::Graph,
            damage,
            sink,
        );
        let Some(list) = self.ranking_list() else {
            return;
        };
        let mut values = match ranking.cells.try_borrow_mut() {
            Ok(v) => v,
            Err(_) => {
                label(
                    self.surface,
                    list.rect(),
                    "Contributor list unavailable",
                    false,
                    damage,
                    sink,
                );
                return;
            }
        };
        values.clear();
        for row in ranking.rows().iter().skip(ranking.first).take(list.rows()) {
            let value = sample
                .value
                .processes
                .processes()
                .get(*row)
                .map(|p| {
                    let metric = if ranking.metric == Metric::Cpu {
                        Text::<32>::percent(p.cpu, false)
                    } else {
                        Text::<32>::bytes(p.rss, false)
                    };
                    let mut detail = Text::<64>::default();
                    let _ = write!(detail, "PID {} | {}", p.key.pid, metric.as_str());
                    detail
                })
                .unwrap_or_default();
            if values.push(value).is_err() {
                return;
            }
        }
        let _ = sample.value.processes.with_identities(|names| {
            list.emit(
                ranking
                    .rows()
                    .iter()
                    .skip(ranking.first)
                    .take(list.rows())
                    .zip(values.iter())
                    .map(|(row, value)| chrome::Item {
                        label: names.get(*row).map(|n| n.name()).unwrap_or("Unavailable"),
                        meta: value.as_str(),
                        enabled: true,
                        marked: sample
                            .value
                            .processes
                            .processes()
                            .get(*row)
                            .is_some_and(|p| self.model.selected() == Some(p.key)),
                    }),
                ranking.first,
                ranking.selected,
                ranking.rows().len(),
                damage,
                sink,
            )
        });
    }

    fn toolbar(&self, index: usize) -> Rect {
        let s = self.surface.scale.value() as u32;
        let x = (index as u32 * 176 * s).min(self.surface.width as u32);
        Rect {
            x: i64::from(x),
            y: i64::from((24 * s).min(self.surface.height as u32)),
            width: (176 * s).min((self.surface.width as u32).saturating_sub(x)),
            height: (24 * s).min((self.surface.height as u32).saturating_sub(24 * s)),
        }
    }
    pub fn key(&mut self, key: &str, repeated: bool) -> Outcome {
        let key = if key == " " { "Space" } else { key };
        if self.actions.key(key, repeated) {
            self.action_note();
            self.dirty = true;
            return Outcome::Changed;
        }
        if matches!(key, "F10" | "S-F10") && !repeated {
            self.process_menu(None);
            return Outcome::Changed;
        }
        if self.held {
            self.cancel_gesture();
        }
        self.flush_selection();
        if key == "C-q" && !repeated {
            self.actions.cancel();
            return Outcome::Quit;
        }
        if key == "C-f" {
            self.set_focus(Focus::Search);
            return Outcome::Changed;
        }
        if key == "Escape" {
            if self.ranking.take().is_some() {
                self.refresh(false);
                self.dirty = true;
                return Outcome::Changed;
            }
            self.cancel_gesture();
            self.set_focus(Focus::Tree);
            return Outcome::Changed;
        }
        if matches!(key, "Tab" | "S-Tab") {
            let focuses: &[Focus] = if matches!(self.tab, 3 | 4) {
                &[
                    Focus::Tabs,
                    Focus::Graph,
                    Focus::Devices,
                    Focus::Divider,
                    Focus::Search,
                    Focus::Tree,
                    Focus::Actions,
                ]
            } else {
                &[
                    Focus::Tabs,
                    Focus::Graph,
                    Focus::Divider,
                    Focus::Search,
                    Focus::Tree,
                    Focus::Actions,
                ]
            };
            let at = focuses.iter().position(|f| *f == self.focus).unwrap_or(0);
            let next = if key == "Tab" {
                (at + 1) % focuses.len()
            } else {
                (at + focuses.len() - 1) % focuses.len()
            };
            if let Some(focus) = focuses.get(next) {
                self.set_focus(*focus);
            }
            return Outcome::Changed;
        }
        if key == "C-l" && !repeated {
            self.live();
            return Outcome::Changed;
        }
        if key == "C-a" && self.focus != Focus::Search && !repeated {
            self.all_processes();
            return Outcome::Changed;
        }
        if key == "C-i" && !repeated {
            return self.interval();
        }
        match self.focus {
            Focus::Tabs => {
                let nav = match key {
                    "Left" => Some(chrome::TabNavigation::Previous),
                    "Right" => Some(chrome::TabNavigation::Next),
                    "Home" => Some(chrome::TabNavigation::First),
                    "End" => Some(chrome::TabNavigation::Last),
                    _ => None,
                };
                if let Some(index) = nav.and_then(|nav| {
                    chrome::Strip::new(self.surface, 0, self.tab, TABS.len())
                        .and_then(|s| s.selection(nav))
                }) {
                    self.choose_tab(index);
                    return Outcome::Changed;
                }
            }
            Focus::Divider => {
                let key = match key {
                    "Left" | "Up" => Some(split::Key::Decrease),
                    "Right" | "Down" => Some(split::Key::Increase),
                    "Home" => Some(split::Key::First),
                    "End" => Some(split::Key::Last),
                    _ => None,
                };
                if let Some(key) = key {
                    let _ = self.split.event(split::Event::Key(key));
                    self.refresh(false);
                    self.dirty = true;
                    return Outcome::Changed;
                }
            }
            Focus::Search => {
                let changed = self.search.key(key);
                if changed {
                    self.refresh(true);
                }
                self.dirty = true;
                return Outcome::Changed;
            }
            Focus::Tree => {
                let key = match key {
                    "Up" => Some(tree::Key::Up),
                    "Down" => Some(tree::Key::Down),
                    "Left" => Some(tree::Key::Left),
                    "Right" => Some(tree::Key::Right),
                    "Home" => Some(tree::Key::First),
                    "End" => Some(tree::Key::Last),
                    "PageUp" => Some(tree::Key::PageUp),
                    "PageDown" => Some(tree::Key::PageDown),
                    "S-Left" => Some(tree::Key::ScrollLeft),
                    "S-Right" => Some(tree::Key::ScrollRight),
                    "Return" => Some(tree::Key::Activate),
                    _ => None,
                };
                if let Some(key) = key {
                    if let Some(view) = &mut self.view {
                        let outcome = view.table.event(tree::Event::Key { key, repeated });
                        self.tree_outcome(outcome);
                    }
                    return Outcome::Changed;
                }
            }
            Focus::Graph => {
                if self.ranking.is_some() {
                    return self.ranking_key(key, repeated);
                }
                if key == "C-Tab" && !repeated {
                    self.graph_focus = (self.graph_focus + 1) % self.graphs.len().max(1);
                    self.dirty = true;
                    return Outcome::Changed;
                }
                if matches!(key, "PageUp" | "PageDown") {
                    let delta = if key == "PageUp" { -1 } else { 1 };
                    self.graph_scroll(delta);
                    return Outcome::Changed;
                }
                let key = match key {
                    "Left" => Some(charts::Key::PreviousTime),
                    "Right" => Some(charts::Key::NextTime),
                    "Home" => Some(charts::Key::FirstTime),
                    "End" => Some(charts::Key::LastTime),
                    "Up" => Some(charts::Key::PreviousSeries),
                    "Down" => Some(charts::Key::NextSeries),
                    "Space" => Some(charts::Key::ClearSeries),
                    _ => None,
                };
                if let Some(key) = key {
                    let revision = self.model.history().samples().last().map(|s| s.id);
                    if let Some((graph, revision)) =
                        self.graphs.get_mut(self.graph_focus).zip(revision)
                    {
                        let kind = graph.plot.kind;
                        let outcome = graph.plot.with_chart(
                            self.surface,
                            below(graph.slot.rect, 24 * self.surface.scale.value() as u32),
                            |chart| {
                                graph.state.event(
                                    chart,
                                    revision,
                                    charts::Event::Key { key, repeated },
                                )
                            },
                        );
                        if let Ok(outcome) = outcome {
                            self.graph_outcome(outcome, kind);
                        }
                    }
                    return Outcome::Changed;
                }
            }
            Focus::Devices => {
                let count = self.device_count();
                match key {
                    "Up" => self.device_focus = self.device_focus.saturating_sub(1),
                    "Down" => {
                        self.device_focus = (self.device_focus + 1).min(count.saturating_sub(1))
                    }
                    "Home" => self.device_focus = 0,
                    "End" => self.device_focus = count.saturating_sub(1),
                    "PageUp" => {
                        self.device_focus = self
                            .device_focus
                            .saturating_sub(self.device_list().map(|l| l.rows()).unwrap_or(1))
                    }
                    "PageDown" => {
                        self.device_focus = (self.device_focus
                            + self.device_list().map(|l| l.rows()).unwrap_or(1))
                        .min(count.saturating_sub(1))
                    }
                    "Space" | "Return" => {
                        if !repeated {
                            self.toggle_device();
                        }
                    }
                    _ => return Outcome::Ignored,
                }
                self.reveal_device();
                self.dirty = true;
                return Outcome::Changed;
            }
            Focus::Actions => {
                if key == "Return" {
                    self.process_menu(None);
                    return Outcome::Changed;
                }
            }
        }
        Outcome::Ignored
    }
    fn graph_scroll(&mut self, delta: i64) {
        let columns = if self
            .split
            .layout()
            .is_some_and(|l| l.first.width >= 1024 * self.surface.scale.value() as u32)
        {
            2
        } else {
            1
        };
        self.graph_first = self
            .graph_first
            .saturating_add_signed((delta * columns).clamp(-4096, 4096) as isize)
            .min(self.card_count().saturating_sub(1));
        self.graph_focus = 0;
        self.refresh(false);
        self.dirty = true;
    }
    pub fn scroll(&mut self, x: i64, y: i64, rows: i64, columns: i64) -> Outcome {
        if self.actions.scroll(x, y, rows) {
            self.action_note();
            self.dirty = true;
            return Outcome::Changed;
        }
        if self.held {
            self.cancel_gesture();
        }
        self.flush_selection();
        if let Some(list) = self
            .ranking_list()
            .filter(|list| list.rect().contains(x, y))
        {
            if let Some(ranking) = &mut self.ranking {
                ranking.first = ranking
                    .first
                    .saturating_add_signed(rows.clamp(-32768, 32768) as isize)
                    .min(ranking.rows().len().saturating_sub(list.rows()));
                self.dirty = true;
                return Outcome::Changed;
            }
        }
        if let Some(list) = self.device_list().filter(|list| list.rect().contains(x, y)) {
            self.device_first = self
                .device_first
                .saturating_add_signed(rows.clamp(-1024, 1024) as isize)
                .min(self.device_count().saturating_sub(list.rows()));
            self.dirty = true;
            return Outcome::Changed;
        }
        if self.split.layout().is_some_and(|l| l.first.contains(x, y)) {
            self.graph_scroll(rows.signum());
            return Outcome::Changed;
        }
        if let Some(view) = &mut self.view {
            if !view
                .table
                .geometry()
                .is_some_and(|g| g.rect().contains(x, y))
            {
                return Outcome::Ignored;
            }
            let outcome = view.table.event(tree::Event::Scroll { rows, columns });
            self.tree_outcome(outcome);
            return Outcome::Changed;
        }
        Outcome::Ignored
    }
    fn flush_selection(&mut self) {
        if self.selection_refresh && !self.held {
            self.selection_refresh = false;
            self.refresh(true);
        }
    }
    pub fn pointer(&mut self, phase: Phase, x: i64, y: i64) -> Outcome {
        let outcome = self.pointer_inner(phase, x, y);
        self.flush_selection();
        outcome
    }
    fn pointer_inner(&mut self, phase: Phase, x: i64, y: i64) -> Outcome {
        if let Some(changed) = self.actions.pointer(phase, x, y) {
            self.action_note();
            self.dirty |= changed;
            return if changed {
                Outcome::Changed
            } else {
                Outcome::Ignored
            };
        }
        if phase == Phase::Move && !self.held {
            return Outcome::Ignored;
        }
        match phase {
            Phase::Press => {
                self.held = true;
                self.armed = Some((x, y));
            }
            Phase::Release => self.held = false,
            Phase::Move => {}
        }
        let released_arm = if phase == Phase::Release {
            self.armed.take()
        } else {
            None
        };
        let split_event = match phase {
            Phase::Press => split::Event::Press { x, y },
            Phase::Move => split::Event::Move { x, y },
            Phase::Release => split::Event::Release { x, y },
        };
        let split_outcome = self
            .split
            .event(split_event)
            .unwrap_or(split::Outcome::Ignored);
        if split_outcome != split::Outcome::Ignored {
            self.set_focus(Focus::Divider);
            if split_outcome == split::Outcome::Changed {
                self.reposition();
                self.refresh_pending = true;
            }
            self.dirty = true;
            return Outcome::Changed;
        }
        if phase != Phase::Press && self.view.as_ref().is_some_and(|view| view.table.captured()) {
            if let Some(view) = &mut self.view {
                let event = if phase == Phase::Move {
                    tree::Event::Move { x, y }
                } else {
                    tree::Event::Release { x, y }
                };
                let outcome = view.table.event(event);
                self.tree_outcome(outcome);
                return Outcome::Changed;
            }
        }
        if phase != Phase::Press && self.graph_pointer(phase, x, y) {
            if phase == Phase::Release {
                self.armed = None;
            }
            return Outcome::Changed;
        }
        if phase == Phase::Release {
            if let Some((px, py)) = released_arm {
                if let Some(rect) = self
                    .actions_rect()
                    .filter(|r| r.contains(px, py) && r.contains(x, y))
                {
                    let _ = rect;
                    self.set_focus(Focus::Actions);
                    self.process_menu(Some((x, y)));
                    return Outcome::Changed;
                }
                if let Some(list) = self.ranking_list() {
                    let first = list.hit(px, py);
                    let last = list.hit(x, y);
                    if first.is_some_and(|hit| {
                        self.ranking
                            .as_ref()
                            .is_some_and(|ranking| ranking.first + hit < ranking.rows().len())
                    }) && first == last
                    {
                        if let Some(ranking) = &mut self.ranking {
                            ranking.selected = ranking.first + first.unwrap_or(0);
                        }
                        self.set_focus(Focus::Graph);
                        self.select_ranked();
                        return Outcome::Changed;
                    }
                }
                if let Some(list) = self.device_list() {
                    let first = list.hit(px, py).map(|i| i + self.device_first);
                    let last = list.hit(x, y).map(|i| i + self.device_first);
                    if first.is_some_and(|index| index < self.device_count()) && first == last {
                        self.device_focus = first.unwrap_or(0);
                        self.set_focus(Focus::Devices);
                        self.toggle_device();
                        return Outcome::Changed;
                    }
                }
                let strip = chrome::Strip::new(self.surface, 0, self.tab, TABS.len())
                    .map(|s| s.with_close_buttons(false));
                if let Some(chrome::TabHit::Select(index)) = strip
                    .and_then(|s| s.hit(x, y))
                    .filter(|hit| strip.and_then(|s| s.hit(px, py)) == Some(*hit))
                {
                    self.set_focus(Focus::Tabs);
                    self.choose_tab(index);
                    return Outcome::Changed;
                }
                for index in 0..3 {
                    if self.toolbar(index).contains(px, py) && self.toolbar(index).contains(x, y) {
                        if index == 0 {
                            self.live();
                            return Outcome::Changed;
                        }
                        if index == 1 {
                            return self.interval();
                        }
                        self.all_processes();
                        return Outcome::Changed;
                    }
                }
            }
        }
        if let Some(layout) = self.split.layout() {
            let entry_rect = self.search_rect().unwrap_or(Rect {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            });
            if phase == Phase::Press && entry_rect.contains(x, y) {
                self.set_focus(Focus::Search);
                if let Some(entry) = chrome::TextEntry::new(self.surface, entry_rect) {
                    if let Some(caret) = entry.hit(
                        x,
                        y,
                        entry.reveal(self.search.text().chars().count(), self.search.caret(), 0),
                        self.search.text().chars().count(),
                    ) {
                        self.search.place(caret);
                    }
                }
                return Outcome::Changed;
            }
            if layout.first.contains(x, y) {
                if phase == Phase::Press {
                    let device = self
                        .device_list()
                        .is_some_and(|list| list.rect().contains(x, y));
                    if device {
                        self.set_focus(Focus::Devices);
                    } else if !matches!(self.tab, 3 | 4)
                        || self
                            .graphs
                            .iter()
                            .any(|graph| graph.slot.rect.contains(x, y))
                    {
                        self.set_focus(Focus::Graph);
                    }
                }
                if self.ranking.is_some() {
                    return Outcome::Changed;
                }
                if self.graph_pointer(phase, x, y) {
                    return Outcome::Changed;
                }
                return Outcome::Changed;
            }
        }
        if phase == Phase::Press
            && self
                .view
                .as_ref()
                .and_then(|view| view.table.geometry())
                .is_some_and(|geometry| geometry.rect().contains(x, y))
        {
            self.set_focus(Focus::Tree);
        }
        if let Some(view) = &mut self.view {
            let event = match phase {
                Phase::Press => tree::Event::Press { x, y },
                Phase::Move => tree::Event::Move { x, y },
                Phase::Release => tree::Event::Release { x, y },
            };
            let outcome = view.table.event(event);
            if outcome != tree::Outcome::Ignored {
                self.tree_outcome(outcome);
                return Outcome::Changed;
            }
        }
        Outcome::Ignored
    }
    // Drag geometry is cheap; chart data rebuilds coalesce at the next tick.
    fn reposition(&mut self) {
        let slots = self.slots();
        self.graphs.retain(|graph| {
            slots
                .iter()
                .flatten()
                .any(|slot| slot.kind == Some(graph.plot.kind))
        });
        for graph in self.graphs.iter_mut() {
            if let Some(slot) = slots
                .iter()
                .flatten()
                .find(|slot| slot.kind == Some(graph.plot.kind))
            {
                graph.slot = *slot;
            }
        }
        self.graph_focus = self.graph_focus.min(self.graphs.len().saturating_sub(1));
        if let Some(layout) = self.split.layout() {
            if let Some(view) = &mut self.view {
                let _ = view.table.resize(
                    self.surface,
                    below(layout.second, 24 * self.surface.scale.value() as u32),
                );
                if view.reserve_paint().is_err() {
                    self.view = None;
                }
            }
        }
    }
    fn graph_pointer(&mut self, phase: Phase, x: i64, y: i64) -> bool {
        if self.ranking.is_some() || (phase == Phase::Move && !self.held) {
            return false;
        }
        let revision = self.model.history().samples().last().map(|s| s.id);
        for (index, graph) in self.graphs.iter_mut().enumerate() {
            let Some(revision) = revision else { break };
            let event = match phase {
                Phase::Press => charts::Event::Press { x, y },
                Phase::Move => charts::Event::Move { x, y },
                Phase::Release => charts::Event::Release { x, y },
            };
            let result = graph.plot.with_chart(
                self.surface,
                below(graph.slot.rect, 24 * self.surface.scale.value() as u32),
                |chart| graph.state.event(chart, revision, event),
            );
            if let Ok(outcome) = result {
                if outcome != charts::Outcome::Ignored {
                    self.graph_focus = index;
                    let kind = graph.plot.kind;
                    self.graph_outcome(outcome, kind);
                    return true;
                }
            }
        }
        false
    }
    pub fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let surface = self.surface;
        let s = surface.scale.value() as u32;
        fill(surface.bounds(), CHROME, damage, sink);
        if let Some(strip) = chrome::Strip::new(surface, 0, self.tab, TABS.len()) {
            strip
                .with_close_buttons(false)
                .emit(TABS.iter().map(|s| (*s, false)), damage, sink);
        }
        if self.keyboard_focus && self.focus == Focus::Tabs {
            fill(
                Rect {
                    x: 0,
                    y: i64::from(22 * s),
                    width: surface.width as u32,
                    height: 2 * s,
                },
                SELECTED,
                damage,
                sink,
            );
        }
        button(
            surface,
            self.toolbar(0),
            if self.model.historical() {
                "Return to Live (C-L)"
            } else {
                "Live (C-L)"
            },
            !self.model.historical(),
            damage,
            sink,
        );
        let interval = match self.model.history().interval() {
            Interval::HalfSecond => "0.5 s",
            Interval::Second => "1 s",
            Interval::TwoSeconds => "2 s",
            Interval::FiveSeconds => "5 s",
        };
        let mut cadence = Text::<32>::new("Refresh: ");
        let _ = cadence.write_str(interval);
        button(
            surface,
            self.toolbar(1),
            cadence.as_str(),
            false,
            damage,
            sink,
        );
        button(
            surface,
            self.toolbar(2),
            "Compare all (C-A)",
            self.model.selected().is_none(),
            damage,
            sink,
        );
        if let Some(layout) = self.split.layout() {
            let mut title = Text::<128>::new("Resource history");
            let _ = write!(
                title,
                "   {}/{}   PgUp/PgDn: more | Click plot/legend to inspect",
                self.graph_first + 1,
                self.card_count()
            );
            label(
                surface,
                Rect {
                    height: 24 * s,
                    ..layout.first
                },
                title.as_str(),
                self.keyboard_focus && self.focus == Focus::Graph,
                damage,
                sink,
            );
            if self.ranking.is_some() {
                self.emit_ranking(layout.first, damage, sink);
            }
            for graph in self.graphs.iter().filter(|_| self.ranking.is_none()) {
                let mut title = Text::<128>::new(graph.plot.kind.title());
                if matches!(graph.plot.kind, Kind::ProcessCpu | Kind::ProcessRss) {
                    let _ = title.write_str(if self.model.selected().is_some() {
                        " | Selected process"
                    } else {
                        " | Top processes"
                    });
                }
                if let Kind::Core(id) = graph.plot.kind {
                    let _ = write!(title, " {id}");
                }
                if matches!(graph.plot.kind, Kind::Network | Kind::Disk) {
                    let network = graph.plot.kind == Kind::Network;
                    let count = if network {
                        self.devices.network_count()
                    } else {
                        self.devices.disk_count()
                    };
                    let _ = write!(title, " | {}", if count > 1 { "Sum: " } else { "" });
                    if count == 0 {
                        let _ = title.write_str("No selection");
                    }
                    let names: &mut dyn Iterator<Item = &str> = if network {
                        &mut self.devices.network_names()
                    } else {
                        &mut self.devices.disk_names()
                    };
                    for (index, name) in names.enumerate() {
                        if index > 0 {
                            let _ = title.write_str(", ");
                        }
                        if title.write_str(name).is_err() {
                            let _ = title.write_str("...");
                            break;
                        }
                    }
                }
                label(
                    surface,
                    Rect {
                        height: 24 * s,
                        ..graph.slot.rect
                    },
                    title.as_str(),
                    false,
                    damage,
                    sink,
                );
                if graph
                    .plot
                    .with_chart(surface, below(graph.slot.rect, 24 * s), |chart| {
                        chart.emit(
                            graph.state.selection(),
                            self.keyboard_focus
                                && self.focus == Focus::Graph
                                && self
                                    .graphs
                                    .get(self.graph_focus)
                                    .is_some_and(|g| g.slot.index == graph.slot.index),
                            damage,
                            sink,
                        )
                    })
                    .is_err()
                {
                    label(
                        surface,
                        below(graph.slot.rect, 24 * s),
                        "Resize to show graph",
                        false,
                        damage,
                        sink,
                    );
                }
            }
            for slot in self
                .slots()
                .into_iter()
                .flatten()
                .filter(|s| s.kind.is_none() && self.ranking.is_none())
            {
                self.emit_devices(slot.rect, damage, sink);
            }
            self.split.emit(damage, sink);
            let entry_rect = self.search_rect().unwrap_or(Rect {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            });
            if let Some(rect) = self.actions_rect() {
                button(
                    surface,
                    rect,
                    "Process actions (F10)",
                    self.keyboard_focus && self.focus == Focus::Actions,
                    damage,
                    sink,
                );
            }
            if let Some(entry) = chrome::TextEntry::new(surface, entry_rect) {
                let caret = self.search.caret();
                let first = entry.reveal(self.search.text().chars().count(), caret, 0);
                entry.emit(
                    chrome::Field {
                        text: self.search.text(),
                        placeholder: "Search processes (name, PID, UID)",
                        caret,
                        anchor: self.search.anchor(),
                        first,
                        masked: false,
                        focused: self.keyboard_focus && self.focus == Focus::Search,
                        caret_visible: self.keyboard_focus && self.focus == Focus::Search,
                    },
                    damage,
                    sink,
                );
            }
            if let Some(view) = &self.view {
                if let Some(sample) = self
                    .model
                    .history()
                    .samples()
                    .iter()
                    .find(|sample| sample.id == view.sample)
                {
                    if view
                        .paint(&sample.value.processes, self.sort, damage, sink)
                        .is_err()
                    {
                        label(
                            surface,
                            below(layout.second, 24 * s),
                            "Memory limit: process view unavailable",
                            false,
                            damage,
                            sink,
                        );
                    }
                }
            }
        } else {
            label(
                surface,
                region(surface),
                "Resize the window to show resource history and processes",
                false,
                damage,
                sink,
            );
        }
        let mut status = Text::<1024>::new(if self.sort.flat() {
            "RANKED LIST | "
        } else {
            "PROCESS TREE | "
        });
        if let Some(sample) = self.model.history().selected() {
            let age = self.now_ns.saturating_sub(sample.time_ns) / 1_000_000_000;
            let _ = write!(
                status,
                "{} | age {}s | {} processes{} | history {}s",
                if self.model.historical() {
                    "INSPECTING"
                } else {
                    "LIVE"
                },
                age,
                sample.value.processes.processes().len(),
                if sample.value.coverage.partial() {
                    " (partial, totals marked *)"
                } else {
                    ""
                },
                self.model.history().retained_duration_ns() / 1_000_000_000,
            );
            let coverage = sample.value.coverage;
            if coverage.partial() || coverage.omitted_cpus > 0 {
                let _ = write!(status,
                    " | coverage: unreadable {}, malformed {}, omitted processes >= {}, omitted CPUs {}{}",
                    coverage.unreadable, coverage.malformed, coverage.omitted_at_least,
                    coverage.omitted_cpus,
                    if coverage.enumeration_failed { "; enumeration failed" } else { "" });
            }
            let _ = write!(status, " | {}", self.notice.as_str());
        } else {
            let _ = write!(
                status,
                "Waiting for process observations | {}",
                self.notice.as_str()
            );
        }
        if let Some(sample) = self.model.history().selected() {
            if self.model.historical()
                && self.model.history().samples().last().is_some_and(|latest| {
                    latest.time_ns.saturating_sub(sample.time_ns) >= crate::history::WINDOW_NS
                })
            {
                let _ = status.write_str(" | Inspection predates plotted window");
            }
            if self.model.selected().is_some_and(|key| {
                sample
                    .value
                    .processes
                    .processes()
                    .binary_search_by_key(&key, |p| p.key)
                    .is_err()
            }) {
                let _ = status.write_str(" | Selected process no longer observed");
            }
        }
        chrome::Status::new(surface).emit(status.as_str().chars(), damage, sink);
        self.actions.emit(damage, sink);
    }
    fn emit_devices(&self, rect: Rect, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let Some(sample) = self
            .model
            .history()
            .selected()
            .and_then(|s| s.value.devices.as_ref())
        else {
            label(
                self.surface,
                rect,
                "Device observations unavailable",
                false,
                damage,
                sink,
            );
            return;
        };
        let network = self.tab == 3;
        let title = if network {
            "Interfaces (current network namespace)"
        } else {
            "Block devices"
        };
        label(
            self.surface,
            Rect {
                height: 24 * self.surface.scale.value() as u32,
                ..rect
            },
            title,
            false,
            damage,
            sink,
        );
        let count = if network {
            self.devices.network_count()
        } else {
            self.devices.disk_count()
        };
        let mut hint = Text::<256>::default();
        let _ = write!(
            hint,
            "{} selected. Space toggles.{}",
            count,
            if count > 1 {
                " Sum may count overlapping layers."
            } else {
                " * marks selection."
            }
        );
        let (omitted, unavailable) = if network {
            (sample.omitted_networks, sample.network_unavailable)
        } else {
            (sample.omitted_disks, sample.disk_unavailable)
        };
        if unavailable {
            let _ = hint.write_str(" Observations unavailable.");
        }
        if omitted > 0 {
            let _ = write!(hint, " Omitted: {omitted}.");
        }
        label(
            self.surface,
            Rect {
                y: rect.y + 24 * self.surface.scale.value() as i64,
                height: 24 * self.surface.scale.value() as u32,
                ..rect
            },
            hint.as_str(),
            self.keyboard_focus && self.focus == Focus::Devices,
            damage,
            sink,
        );
        if let Some(list) = chrome::List::new(self.surface, self.device_rows_rect(rect)) {
            let total = if network {
                sample.networks.len()
            } else {
                sample.disks.len()
            };
            list.emit(
                (self.device_first..total).map(|index| {
                    let (name, checked) = if network {
                        (
                            sample
                                .networks
                                .get(index)
                                .map(|d| d.name.as_str())
                                .unwrap_or(""),
                            self.devices.network_selected(sample, index),
                        )
                    } else {
                        (
                            sample
                                .disks
                                .get(index)
                                .map(|d| d.name.as_str())
                                .unwrap_or(""),
                            self.devices.disk_selected(sample, index),
                        )
                    };
                    chrome::Item {
                        label: name,
                        meta: "",
                        enabled: true,
                        marked: checked,
                    }
                }),
                self.device_first,
                self.device_focus,
                total,
                damage,
                sink,
            );
        }
        let mut details = [Text::<256>::default(); 3];
        if network {
            if let Some(n) = sample.networks.get(self.device_focus) {
                if let Some(line) = details.get_mut(0) {
                    let _ = write!(
                        line,
                        "{}: received {}",
                        n.name.as_str(),
                        Text::<32>::bytes(Some(n.received), false).as_str()
                    );
                }
                if let Some(line) = details.get_mut(1) {
                    let _ = write!(
                        line,
                        "Sent {} | ifindex {}",
                        Text::<32>::bytes(Some(n.sent), false).as_str(),
                        Text::<32>::number(n.ifindex.map(u64::from)).as_str()
                    );
                }
                if let Some(line) = details.get_mut(2) {
                    let _ = write!(
                        line,
                        "{} | {} | namespace-local counters",
                        if n.up { "Up" } else { "Down/unknown" },
                        if n.loopback {
                            "Loopback"
                        } else if n.device_link {
                            "Device link"
                        } else {
                            "Virtual/unknown"
                        }
                    );
                }
            }
        } else if let Some(d) = sample.disks.get(self.device_focus) {
            if let Some(line) = details.get_mut(0) {
                let _ = write!(
                    line,
                    "IOPS R {} W {} | busy {}",
                    Text::<32>::number(d.read_iops).as_str(),
                    Text::<32>::number(d.write_iops).as_str(),
                    Text::<32>::percent(d.busy, false).as_str()
                );
            }
            if let Some(line) = details.get_mut(1) {
                let _ = write!(
                    line,
                    "Totals R {} W {}",
                    Text::<32>::bytes(d.read_bytes, false).as_str(),
                    Text::<32>::bytes(d.written_bytes, false).as_str()
                );
            }
            if let Some(line) = details.get_mut(2) {
                let _ = write!(
                    line,
                    "{} {}:{} | {} | busy != saturation",
                    d.name.as_str(),
                    d.major,
                    d.minor,
                    match d.whole {
                        Some(true) => "Whole device",
                        Some(false) => "Partition",
                        None => "Topology unknown",
                    }
                );
            }
        }
        let s = self.surface.scale.value() as u32;
        let area = below(rect, 48 * s);
        let start = area.y + i64::from(area.height.saturating_sub(72 * s));
        for (index, text) in details.iter().enumerate() {
            let row = Rect {
                x: rect.x,
                y: start + index as i64 * i64::from(24 * s),
                width: rect.width,
                height: 24 * s,
            };
            if let Some(row) = row.intersection(area) {
                label(self.surface, row, text.as_str(), false, damage, sink);
            }
        }
    }
}

impl td_ui::raster::Composition for State {
    fn surface(&self) -> Surface {
        State::surface(self)
    }
    fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        State::emit(self, damage, sink);
    }
}
impl State {
    pub fn report(&self) -> String {
        let selected = match self.selected() {
            Some(Key::Process(key)) => {
                format!("{}:{}:{}", key.generation, key.pid, key.start_ticks)
            }
            Some(Key::Unavailable) => "group".into(),
            None => "none".into(),
        };
        format!("tab={}\tfocus={:?}\tquery={}\tselected={}\tlive={}\tinspected_ns={}\tnewest_ns={}\trows={}\tretained={}\tmodel_bytes={}\tactions={}",TABS.get(self.tab).copied().unwrap_or(""),self.focus,td_ui::control::hex(self.search.text().as_bytes()),selected,!self.model.historical(),self.model.history().selected().map(|s|s.time_ns).unwrap_or(0),self.model.history().samples().last().map(|s|s.time_ns).unwrap_or(0),self.visible(),self.model.history().samples().len(),self.budget.used(),self.actions.stage())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use crate::collector::Batch;
    use crate::hierarchy::{Input, ProcessKey};
    use crate::snapshot::Observed;
    fn key(pid: u32) -> ProcessKey {
        ProcessKey {
            generation: 1,
            pid,
            start_ticks: 1,
        }
    }
    fn rows() -> [Observed<'static>; 3] {
        std::array::from_fn(|index| Observed {
            input: Input {
                key: key(index as u32 + 1),
                parent_pid: Some(if index == 0 { 0 } else { 1 }),
                cpu: Some(index as u64 * 100),
                rss: Some(4096),
            },
            name: match index {
                0 => "parent",
                1 => "worker",
                _ => "helper",
            },
            uid: Some(1000),
            state: b'R',
        })
    }
    fn observation(state: &mut State, budget: &Arc<Budget>, at: u64) {
        state.update(
            Update {
                batch: Some(Batch::fixture(budget, at, &rows())),
                skipped: 0,
                failure: None,
            },
            at,
        );
    }
    #[test]
    fn sorting_reveals_highest_and_compare_button_clears_the_plot_filter() {
        let budget = Budget::new(crate::budget::LIMIT).unwrap();
        let mut state = State::new(
            &budget,
            Surface::new(1280, 960, Default::default()).unwrap(),
        )
        .unwrap();
        observation(&mut state, &budget, 1_000_000_000);
        state.choose_tab(1);
        state.tree_outcome(tree::Outcome::Selected(Key::Process(key(1))));
        state.tree_outcome(tree::Outcome::Sort(4));
        assert_eq!(
            state.view.as_ref().unwrap().table.first_anchor(),
            Some(Key::Process(key(3)))
        );
        assert!(state.sort.flat());
        let rect = state.toolbar(2);
        state.pointer(Phase::Press, rect.x + 5, rect.y + 5);
        state.pointer(Phase::Release, rect.x + 5, rect.y + 5);
        assert!(state.model.selected().is_none());
        assert!(state.selected().is_none());
        state.tree_outcome(tree::Outcome::Sort(0));
        assert!(!state.sort.flat());
        assert!(state
            .view
            .as_ref()
            .unwrap()
            .projection
            .rows()
            .iter()
            .any(|row| row.depth > 0));
    }
    #[test]
    fn right_click_selects_captured_row_and_modal_motion_needs_no_repaint() {
        let budget = Budget::new(crate::budget::LIMIT).unwrap();
        let mut state = State::new(
            &budget,
            Surface::new(1280, 960, Default::default()).unwrap(),
        )
        .unwrap();
        observation(&mut state, &budget, 1_000_000_000);
        state.action_update(crate::actions::Update::Available);
        let rect = state
            .view
            .as_ref()
            .unwrap()
            .table
            .geometry()
            .unwrap()
            .row(1)
            .unwrap();
        state.context_menu((rect.x + 100, rect.y + 12));
        assert_eq!(state.selected(), Some(Key::Process(key(2))));
        assert_eq!(state.actions.stage(), "menu");
        state.dirty = false;
        state.pointer(Phase::Move, 0, 0);
        assert!(!state.dirty);
    }
    #[test]
    fn graph_selection_pins_matching_tree_and_reveals_search_exception_across_tabs() {
        let budget = Budget::new(crate::budget::LIMIT).unwrap();
        let mut state = State::new(
            &budget,
            Surface::new(1280, 960, Default::default()).unwrap(),
        )
        .unwrap();
        observation(&mut state, &budget, 1_000_000_000);
        state.key("C-f", false);
        for ch in ["w", "o", "r", "k", "e", "r"] {
            state.key(ch, false);
        }
        assert_eq!(state.visible(), 2);
        observation(&mut state, &budget, 2_000_000_000);
        state.graph_outcome(
            charts::Outcome::Selected(charts::Selection {
                at: 1_000_000_000,
                series: Some(Id::Process(key(3))),
            }),
            Kind::ProcessCpu,
        );
        assert!(state.model.historical());
        assert_eq!(
            state.model.history().selected().unwrap().time_ns,
            1_000_000_000
        );
        assert_eq!(state.selected(), Some(Key::Process(key(3))));
        assert_eq!(state.visible(), 3);
        assert_eq!(state.query(), "worker");
        assert!(state
            .view
            .as_ref()
            .unwrap()
            .projection
            .rows()
            .iter()
            .any(|r| r.key == Key::Process(key(3)) && r.exception));
        for index in 0..5 {
            state.choose_tab(index);
            assert_eq!(state.selected(), Some(Key::Process(key(3))));
            assert_eq!(state.query(), "worker");
            assert!(state.model.historical());
        }
        state.key("C-l", false);
        assert!(!state.model.historical());
        assert_eq!(
            state.model.history().selected().unwrap().time_ns,
            2_000_000_000
        );
    }
    #[test]
    fn row_capture_freezes_admission_and_outside_release_retires_it() {
        let budget = Budget::new(crate::budget::LIMIT).unwrap();
        let mut state = State::new(
            &budget,
            Surface::new(1280, 960, Default::default()).unwrap(),
        )
        .unwrap();
        observation(&mut state, &budget, 1_000_000_000);
        let rect = state
            .view
            .as_ref()
            .unwrap()
            .table
            .geometry()
            .unwrap()
            .row(1)
            .unwrap();
        state.pointer(Phase::Press, rect.x + 100, rect.y + 12);
        assert!(state.view.as_ref().unwrap().table.captured());
        observation(&mut state, &budget, 2_000_000_000);
        assert_eq!(
            state.model.history().selected().unwrap().time_ns,
            1_000_000_000
        );
        state.pointer(Phase::Move, 10, 100);
        state.pointer(Phase::Release, 10, 100);
        assert!(!state.view.as_ref().unwrap().table.captured());
        assert!(state.armed.is_none());
        state.update(
            Update {
                batch: None,
                skipped: 0,
                failure: None,
            },
            2_000_000_000,
        );
        assert_eq!(
            state.model.history().selected().unwrap().time_ns,
            2_000_000_000
        );
    }
    #[test]
    fn tiny_window_preserves_state_and_every_draw_stays_within_surface() {
        let budget = Budget::new(crate::budget::LIMIT).unwrap();
        let mut state = State::new(
            &budget,
            Surface::new(1280, 960, Default::default()).unwrap(),
        )
        .unwrap();
        observation(&mut state, &budget, 1_000_000_000);
        state.key("C-f", false);
        state.key("x", false);
        for (width, height, scale) in [
            (1, 1, 1),
            (100, 50, 1),
            (640, 480, 1),
            (1280, 960, 1),
            (1280, 960, 2),
        ] {
            let surface =
                Surface::new(width, height, td_ui::raster::Scale::new(scale).unwrap()).unwrap();
            state.resize(surface).unwrap();
            let mut draws = 0;
            state.emit(surface.bounds(), &mut |draw| {
                assert_eq!(draw.clip.intersection(surface.bounds()), Some(draw.clip));
                draws += 1;
            });
            assert!(draws > 0);
            assert_eq!(state.query(), "x");
        }
        assert!(budget.peak() <= budget.maximum());
    }
    #[test]
    fn ranked_background_selection_reveals_context_and_canceled_graph_drag_cannot_select() {
        let budget = Budget::new(crate::budget::LIMIT).unwrap();
        let mut state = State::new(
            &budget,
            Surface::new(1280, 960, Default::default()).unwrap(),
        )
        .unwrap();
        observation(&mut state, &budget, 1_000_000_000);
        observation(&mut state, &budget, 2_000_000_000);
        state.graph_outcome(
            charts::Outcome::Selected(charts::Selection {
                at: 1_000_000_000,
                series: Some(Id::Other),
            }),
            Kind::ProcessCpu,
        );
        assert_eq!(state.ranking.as_ref().unwrap().rows(), &[2, 1, 0]);
        let pressure = budget.charge(budget.maximum() - budget.used()).unwrap();
        let mut drawn = String::new();
        state.emit_ranking(
            state.split.layout().unwrap().first,
            state.surface.bounds(),
            &mut |draw| {
                if let Primitive::Glyph { scalar, .. } = draw.primitive {
                    drawn.push(scalar);
                }
            },
        );
        assert!(drawn.contains("helper"));
        assert!(drawn.contains("PID 3"));
        drop(pressure);
        state.select_ranked();
        assert_eq!(state.model.selected(), Some(key(3)));
        state.live();
        let point = state
            .graphs
            .first()
            .unwrap()
            .plot
            .with_chart(
                state.surface,
                below(state.graphs.first().unwrap().slot.rect, 24),
                |chart| chart.plot(),
            )
            .unwrap();
        state.pointer(Phase::Press, point.x + 10, point.y + 10);
        state.pointer(Phase::Move, 0, state.surface.height as i64 - 1);
        state.pointer(Phase::Release, point.x + 10, point.y + 10);
        assert!(!state.model.historical());
        state.choose_tab(3);
        state
            .resize(Surface::new(800, 600, Default::default()).unwrap())
            .unwrap();
        state.set_focus(Focus::Devices);
        assert!(state.device_list().is_some());
        state.set_focus(Focus::Graph);
        assert!(!state.graphs.is_empty());
    }
    #[test]
    fn cancellation_repaints_focus_and_keyboard_or_wheel_retires_pointer_targets() {
        let budget = Budget::new(crate::budget::LIMIT).unwrap();
        let mut state = State::new(
            &budget,
            Surface::new(1280, 960, Default::default()).unwrap(),
        )
        .unwrap();
        observation(&mut state, &budget, 1_000_000_000);
        let mut batch = Batch::fixture(&budget, 2_000_000_000, &rows());
        batch.cpus = MemoryVec::new(&budget, 16).unwrap();
        for id in 0..16 {
            batch
                .cpus
                .push(crate::collector::LogicalCpu { id, usage: None })
                .unwrap();
        }
        state.update(
            Update {
                batch: Some(batch),
                skipped: 0,
                failure: None,
            },
            2_000_000_000,
        );
        for focus in [
            Focus::Divider,
            Focus::Tree,
            Focus::Graph,
            Focus::Tabs,
            Focus::Search,
            Focus::Actions,
        ] {
            state.restore_focus();
            state.set_focus(focus);
            if focus == Focus::Tree {
                state
                    .view
                    .as_mut()
                    .unwrap()
                    .table
                    .select(Some(Key::Process(key(1))), true);
            }
            let mut before = Vec::new();
            state.emit(state.surface.bounds(), &mut |draw| {
                before.push(format!("{draw:?}"))
            });
            state.painted();
            state.cancel();
            assert!(state.dirty());
            let mut after = Vec::new();
            state.emit(state.surface.bounds(), &mut |draw| {
                after.push(format!("{draw:?}"))
            });
            assert!(before != after, "focus {focus:?} must lose its highlight");
        }
        state.cancel();
        let divider = state.split.layout().unwrap().divider;
        let before = state.split.share();
        state.pointer(Phase::Press, divider.x + 10, divider.y + 2);
        state.pointer(Phase::Move, divider.x + 10, divider.y + 102);
        state.pointer(Phase::Release, divider.x + 10, divider.y + 102);
        assert_ne!(
            state.split.share(),
            before,
            "pointer-only divider must drag"
        );
        state.choose_tab(1);
        state.scroll(100, 100, 3, 0);
        assert_eq!(
            state.graph_first, 2,
            "one wheel detent advances one row of cards"
        );
        let button = state.toolbar(1);
        let interval = state.model.history().interval();
        state.pointer(Phase::Press, button.x + 2, button.y + 2);
        state.key("C-f", false);
        state.pointer(Phase::Release, button.x + 2, button.y + 2);
        assert_eq!(state.model.history().interval(), interval);
        state.pointer(Phase::Press, button.x + 2, button.y + 2);
        state.scroll(100, 100, 1, 0);
        state.pointer(Phase::Release, button.x + 2, button.y + 2);
        assert_eq!(state.model.history().interval(), interval);
    }
    #[test]
    fn dragging_down_keeps_dedicated_graphs_growing() {
        let budget = Budget::new(crate::budget::LIMIT).unwrap();
        let mut state = State::new(
            &budget,
            Surface::new(1200, 1400, Default::default()).unwrap(),
        )
        .unwrap();
        observation(&mut state, &budget, 1_000_000_000);
        state.choose_tab(1);
        let divider = state.split.layout().unwrap().divider;
        state.pointer(Phase::Press, 20, divider.y + 2);
        let mut before = state.slots()[0].unwrap().rect.height;
        for y in divider.y + 3..1250 {
            state.pointer(Phase::Move, 20, y);
            let next = state.slots()[0].unwrap().rect.height;
            assert!(next >= before, "graph shrank at {y}: {before} -> {next}");
            before = next;
        }
        state.pointer(Phase::Release, 20, 1250);
    }
    #[test]
    fn system_series_survive_refresh_and_resize_keeps_graph_navigation_available() {
        let budget = Budget::new(crate::budget::LIMIT).unwrap();
        let mut state = State::new(
            &budget,
            Surface::new(1280, 960, Default::default()).unwrap(),
        )
        .unwrap();
        observation(&mut state, &budget, 1_000_000_000);
        for tab in [0, 3, 4] {
            state.choose_tab(tab);
            state.set_focus(Focus::Graph);
            state.key("Down", false);
            assert!(state.ranking.is_none());
            assert_eq!(
                state
                    .graphs
                    .first()
                    .unwrap()
                    .state
                    .selection()
                    .unwrap()
                    .series,
                Some(Id::System(1))
            );
            state.refresh(false);
            assert_eq!(
                state
                    .graphs
                    .first()
                    .unwrap()
                    .state
                    .selection()
                    .unwrap()
                    .series,
                Some(Id::System(1))
            );
        }
        state.choose_tab(0);
        state.graph_focus = 3;
        state
            .resize(Surface::new(800, 600, Default::default()).unwrap())
            .unwrap();
        assert_eq!(state.graph_focus, 0);
        state.key("Space", false);
        assert!(state.ranking.is_some());
        let list = state.ranking_list().unwrap();
        let y = list.rect().y + 5 * 24 + 2;
        let selected = state.ranking.as_ref().unwrap().selected;
        state.pointer(Phase::Press, list.rect().x + 2, y);
        state.pointer(Phase::Release, list.rect().x + 2, y);
        assert_eq!(state.ranking.as_ref().unwrap().selected, selected);
        assert!(state.armed.is_none());
    }
    #[test]
    fn blank_device_rows_do_not_toggle_a_real_member() {
        let budget = Budget::new(crate::budget::LIMIT).unwrap();
        let mut state = State::new(
            &budget,
            Surface::new(1280, 960, Default::default()).unwrap(),
        )
        .unwrap();
        let mut batch = Batch::fixture(&budget, 1_000_000_000, &rows());
        let mut networks = MemoryVec::new(&budget, 1).unwrap();
        networks
            .push(crate::devices::Network {
                name: crate::budget::MemoryString::new(&budget, "eth0").unwrap(),
                ifindex: Some(1),
                up: true,
                device_link: true,
                received: 10,
                sent: 20,
                receive_rate: Some(1),
                send_rate: Some(2),
                loopback: false,
            })
            .unwrap();
        batch.devices = Some(crate::devices::Sample {
            networks,
            disks: MemoryVec::new(&budget, 0).unwrap(),
            omitted_networks: 0,
            omitted_disks: 0,
            network_unavailable: false,
            disk_unavailable: false,
            network_namespace: None,
        });
        state.update(
            Update {
                batch: Some(batch),
                skipped: 0,
                failure: None,
            },
            1_000_000_000,
        );
        state.choose_tab(3);
        let list = state.device_list().unwrap();
        assert!(list.rows() >= 3);
        let (x, y) = (list.rect().x + 2, list.rect().y + 24 * 2 + 2);
        state.pointer(Phase::Press, x, y);
        state.pointer(Phase::Release, x, y);
        assert_eq!(state.devices.network_count(), 1);
        assert_eq!(state.device_focus, 0);
        assert!(state.armed.is_none());
        state
            .resize(Surface::new(900, 1400, Default::default()).unwrap())
            .unwrap();
        state.graph_first = 1;
        state.refresh(false);
        state.set_focus(Focus::Tabs);
        let rect = state.device_list().unwrap().rect();
        state.pointer(Phase::Press, rect.x + 2, rect.y + 2);
        assert_eq!(
            state.device_list().unwrap().rect(),
            rect,
            "press must not move a visible card"
        );
        state.pointer(Phase::Release, rect.x + 2, rect.y + 2);
        assert_eq!(state.devices.network_count(), 0);
        state.toggle_device();
        state
            .resize(Surface::new(800, 600, Default::default()).unwrap())
            .unwrap();
        state.set_focus(Focus::Devices);
        for expected in [0, 1] {
            let rect = state.device_list().unwrap().rect();
            state.pointer(Phase::Press, rect.x + 2, rect.y + 2);
            state.pointer(Phase::Release, rect.x + 2, rect.y + 2);
            assert_eq!(state.devices.network_count(), expected);
            assert!(state.device_list().is_some());
        }
        state.device_focus = 99;
        state.device_first = 99;
        state.refresh(false);
        assert_eq!(state.device_focus, 0);
        assert_eq!(state.device_first, 0);
        state.choose_tab(4);
        assert_eq!(state.focus, Focus::Devices);
        assert!(
            state.device_list().is_some(),
            "device focus must remain visible"
        );
    }
    #[test]
    fn memory_recovery_preserves_inspection_and_newest_and_paints_without_allocating() {
        let budget = Budget::new(crate::budget::LIMIT).unwrap();
        let mut state = State::new(
            &budget,
            Surface::new(1280, 960, Default::default()).unwrap(),
        )
        .unwrap();
        for second in 1..=30 {
            observation(&mut state, &budget, second * 1_000_000_000);
        }
        let transient_pressure = budget.charge(budget.maximum() - budget.used()).unwrap();
        state.refresh(true);
        assert!(
            !state.refresh_pending,
            "old caches must be released before returning to paint: {}",
            state.notice()
        );
        assert_eq!(state.visible(), 3);
        drop(transient_pressure);
        assert!(state.model.inspect(1_000_000_000));
        state.refresh(true);
        let newest = state.model.history().samples().last().unwrap().id;
        state.view = None;
        state.graphs.clear();
        let pressure = budget.charge(budget.maximum() - budget.used() - 1).unwrap();
        state.refresh(true);
        assert!(state.refresh_pending);
        let before = state.model.history().samples().len();
        for tick in 0..30 {
            let count = state.model.history().samples().len();
            state.update(
                Update {
                    batch: None,
                    skipped: 0,
                    failure: None,
                },
                30_000_000_000 + tick * 50_000_000,
            );
            assert!(count - state.model.history().samples().len() <= 1);
            assert_eq!(
                state.model.history().selected().unwrap().time_ns,
                1_000_000_000
            );
            assert_eq!(state.model.history().samples().last().unwrap().id, newest);
            if !state.refresh_pending {
                break;
            }
        }
        assert!(!state.refresh_pending, "{}", state.notice());
        assert!(state.model.history().samples().len() < before);
        assert_eq!(state.visible(), 3);
        let final_pressure = budget.charge(budget.maximum() - budget.used()).unwrap();
        let sample = state.model.history().selected().unwrap();
        state
            .view
            .as_ref()
            .unwrap()
            .paint(
                &sample.value.processes,
                state.sort,
                state.surface.bounds(),
                &mut |_| {},
            )
            .unwrap();
        drop(final_pressure);
        drop(pressure);
    }
    #[test]
    fn synthetic_selection_and_keyboard_focus_survive_refresh_and_pointer_leave() {
        let budget = Budget::new(crate::budget::LIMIT).unwrap();
        let mut state = State::new(
            &budget,
            Surface::new(1280, 960, Default::default()).unwrap(),
        )
        .unwrap();
        let mut data = rows();
        data.get_mut(0).unwrap().input.parent_pid = Some(1000);
        for second in 1..=2 {
            state.update(
                Update {
                    batch: Some(Batch::fixture(&budget, second * 1_000_000_000, &data)),
                    skipped: 0,
                    failure: None,
                },
                second * 1_000_000_000,
            );
            if second == 2 {
                assert_eq!(state.selected(), Some(Key::Unavailable));
            }
            state.set_focus(Focus::Tree);
            state.key("Home", false);
            assert_eq!(state.selected(), Some(Key::Unavailable));
            state.key("Left", false);
            assert_eq!(state.selected(), Some(Key::Unavailable));
            state.key("Right", false);
            assert_eq!(state.selected(), Some(Key::Unavailable));
            state.cancel_gesture();
            state.key("Down", false);
            assert_eq!(state.selected(), Some(Key::Process(key(1))));
            state.key("Home", false);
        }
        state.set_focus(Focus::Divider);
        let before = state.split.share();
        state.cancel_gesture();
        state.key("Up", false);
        assert_ne!(state.split.share(), before);
        state.cancel();
        assert!(!state.split.focused());
        state.restore_focus();
        assert!(state.split.focused());
    }
    #[test]
    fn unchanged_failure_does_not_repaint_and_plot_window_uses_observation_time() {
        let budget = Budget::new(crate::budget::LIMIT).unwrap();
        let mut state = State::new(
            &budget,
            Surface::new(1280, 960, Default::default()).unwrap(),
        )
        .unwrap();
        observation(&mut state, &budget, 1_000_000_000);
        state.model.inspect(1_000_000_000);
        state.update(
            Update {
                batch: None,
                skipped: 0,
                failure: Some(Failure::Read(std::io::ErrorKind::PermissionDenied)),
            },
            200_000_000_000,
        );
        state.painted();
        state.update(
            Update {
                batch: None,
                skipped: 0,
                failure: None,
            },
            200_050_000_000,
        );
        assert!(!state.dirty());
        state
            .resize(Surface::new(4096, 960, Default::default()).unwrap())
            .unwrap();
        let status = |state: &State| {
            let mut drawn = String::new();
            state.emit(state.surface.bounds(), &mut |draw| {
                if let Primitive::Glyph { y, scalar, .. } = draw.primitive {
                    if y >= state.surface.height as i64 - 24 {
                        drawn.push(scalar);
                    }
                }
            });
            drawn
        };
        assert!(status(&state).contains("age 199s"));
        assert!(!status(&state).contains("Inspection predates plotted window"));
        observation(&mut state, &budget, 201_000_000_000);
        state.note("");
        assert!(status(&state).contains("Inspection predates plotted window"));
    }
    #[test]
    fn escape_keeps_keyboard_navigation_and_gap_markers_are_crossed_both_ways() {
        let budget = Budget::new(crate::budget::LIMIT).unwrap();
        let mut state = State::new(
            &budget,
            Surface::new(1280, 960, Default::default()).unwrap(),
        )
        .unwrap();
        for second in [1, 2, 4, 5] {
            state.update(
                Update {
                    batch: Some(Batch::fixture(&budget, second * 1_000_000_000, &rows())),
                    skipped: u64::from(second == 4),
                    failure: None,
                },
                second * 1_000_000_000,
            );
        }
        state.set_focus(Focus::Tree);
        state.key("Home", false);
        state.key("Escape", false);
        state.key("Down", false);
        assert_eq!(state.selected(), Some(Key::Process(key(2))));
        state.set_focus(Focus::Divider);
        let share = state.split.share();
        state.key("Up", false);
        assert_ne!(share, state.split.share());
        state.set_focus(Focus::Graph);
        state.key("Home", false);
        for (key, expected) in [("Right", 2), ("Right", 4), ("Left", 2), ("Left", 1)] {
            state.key(key, false);
            assert_eq!(
                state.model.history().selected().unwrap().time_ns,
                expected * 1_000_000_000
            );
        }
        state.graph_outcome(
            charts::Outcome::Selected(charts::Selection {
                at: 4_000_000_000,
                series: Some(Id::Process(key(999))),
            }),
            Kind::ProcessCpu,
        );
        assert_eq!(
            state.model.history().selected().unwrap().time_ns,
            4_000_000_000
        );
        assert_eq!(state.notice(), "Process not observed at this time.");
    }
    #[test]
    fn historical_search_selection_refreshes_labels_and_synthetic_selection_clears_exception() {
        let budget = Budget::new(crate::budget::LIMIT).unwrap();
        let mut state = State::new(
            &budget,
            Surface::new(1280, 960, Default::default()).unwrap(),
        )
        .unwrap();
        let mut data = rows();
        data.get_mut(0).unwrap().input.parent_pid = Some(1000);
        state.update(
            Update {
                batch: Some(Batch::fixture(&budget, 1_000_000_000, &data)),
                skipped: 0,
                failure: None,
            },
            1_000_000_000,
        );
        state.model.inspect(1_000_000_000);
        state.key("C-f", false);
        for key in ["h", "e", "l", "p"] {
            state.key(key, false);
        }
        state.graph_outcome(
            charts::Outcome::Selected(charts::Selection {
                at: 1_000_000_000,
                series: Some(Id::Process(key(2))),
            }),
            Kind::ProcessCpu,
        );
        assert_eq!(state.visible(), 4);
        state.set_focus(Focus::Tree);
        state.key("Home", false);
        assert_eq!(state.selected(), Some(Key::Unavailable));
        assert_eq!(state.visible(), 3);
        state.key("Down", false);
        assert!(state
            .view
            .as_ref()
            .unwrap()
            .projection
            .rows()
            .iter()
            .any(|row| row.key == Key::Process(key(1)) && row.exception));
    }
}
