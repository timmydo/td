//! Display-independent input controller. Adapters supply events, never mutable
//! model access; replay and the future Wayland adapter share this dispatcher.

use crate::keys::{Action, Keymap, Profile};
use crate::layout::{Affinity, Caret, Metrics, Viewport, CELL_HEIGHT, CELL_WIDTH};
use crate::model::{Command, Editor, Selection, TabId};
use crate::render::{Geometry, Label, Rect, Scale, Scene, View};
use crate::{Error, Result};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PointerPhase {
    Press,
    Move,
    Release,
}

pub enum Event<'a> {
    New,
    /// Byte fixture or already-authorized file-adapter completion, not a path.
    Load(&'a [u8]),
    SelectTab(TabId),
    Close {
        tab: TabId,
        revision: u64,
    },
    Edit {
        tab: TabId,
        revision: u64,
        command: Command,
    },
    Key {
        tab: TabId,
        revision: u64,
        chord: &'a str,
    },
    Resize {
        width: usize,
        height: usize,
        scale: u8,
    },
    Profile(Profile),
    Wrap {
        tab: TabId,
        revision: u64,
        enabled: bool,
    },
    Scroll {
        tab: TabId,
        revision: u64,
        rows: isize,
        columns: isize,
    },
    Pointer {
        tab: TabId,
        revision: u64,
        phase: PointerPhase,
        x: i64,
        y: i64,
        extend: bool,
    },
    Focus(bool),
    /// Milliseconds since controller creation. Supply a tick immediately before
    /// each timed input event as well as on timer wakes; there is no ambient clock.
    Tick(u64),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    Changed,
    Created(TabId),
    Prefix,
    /// A request is not successful I/O or a visible menu/dialog.
    Request {
        name: &'static str,
        tab: TabId,
        revision: u64,
    },
    Ignored,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TabView {
    pub viewport: Viewport,
    pub affinity: Affinity,
    pub desired_column: Option<usize>,
    pub soft_wrap: bool,
    pub metrics: Metrics,
    pub revision: u64,
}

pub struct Controller {
    editor: Editor,
    keys: Keymap,
    tabs: BTreeMap<TabId, TabView>,
    geometry: Geometry,
    mark: Option<TabId>,
    drag: Option<(TabId, usize)>,
    focused: bool,
    caret_visible: bool,
    clock: u64,
    blink_start: u64,
    generation: u64,
}

impl Default for Controller {
    fn default() -> Self {
        Self {
            editor: Editor::default(),
            keys: Keymap::default(),
            tabs: BTreeMap::new(),
            geometry: Geometry::default(),
            mark: None,
            drag: None,
            focused: true,
            caret_visible: true,
            clock: 0,
            blink_start: 0,
            generation: 0,
        }
    }
}

impl Controller {
    pub fn editor(&self) -> &Editor {
        &self.editor
    }
    pub fn keys(&self) -> &Keymap {
        &self.keys
    }
    pub fn geometry(&self) -> Geometry {
        self.geometry
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn focused(&self) -> bool {
        self.focused
    }
    pub fn tab_view(&self, tab: TabId) -> Result<TabView> {
        self.tabs.get(&tab).copied().ok_or(Error::MissingTab)
    }
    pub fn scene<'a>(&'a self, labels: &'a [Label<'a>]) -> Result<Scene<'a>> {
        let mut view = View {
            focused: self.focused,
            caret_visible: self.caret_visible,
            ..View::default()
        };
        if let Some(tab) = self.editor.active() {
            let state = self.tab_view(tab)?;
            view.origin = state.viewport.origin();
            view.soft_wrap = state.soft_wrap;
            view.affinity = state.affinity;
        }
        Scene::new(
            &self.editor,
            self.geometry,
            view,
            labels,
            self.keys.profile(),
        )
    }

    /// Successful commands conservatively dirty the window. Ignored input and
    /// rejected commands do not advance its generation. Check exhaustion first.
    pub fn dispatch(&mut self, event: Event<'_>) -> Result<Outcome> {
        let next = self.generation.checked_add(1).ok_or(Error::Exhausted)?;
        let outcome = self.apply(event)?;
        if outcome != Outcome::Ignored {
            self.generation = next;
        }
        Ok(outcome)
    }

    fn checked(&self, tab: TabId, revision: u64, active: bool) -> Result<()> {
        if self.editor.document(tab)?.revision() != revision {
            return Err(Error::StaleRevision);
        }
        if active && self.editor.active() != Some(tab) {
            return Err(Error::InvalidArgument);
        }
        Ok(())
    }

    fn reset_input(&mut self) {
        self.keys.reset();
        self.mark = None;
        self.drag = None;
    }
    fn wake_caret(&mut self) {
        self.blink_start = self.clock;
        self.caret_visible = self.focused;
    }

    // All geometry comes from Geometry; all text comes from private Editor.
    // Thus these checked constructors cannot reject an admitted controller
    // state. Cache metrics by revision, wrap mode and full-cell width.
    fn refresh(&mut self, reveal: Option<TabId>) -> Result<()> {
        let (columns, rows) = self.geometry.grid();
        let columns = columns.max(1);
        let rows = rows.max(1);
        self.tabs.retain(|id, _| self.editor.document(*id).is_ok());
        for (id, doc) in self.editor.tabs() {
            let new_view = Viewport::new(columns, rows)?;
            let state = self.tabs.entry(id).or_insert(TabView {
                viewport: new_view,
                affinity: Affinity::Downstream,
                desired_column: None,
                soft_wrap: true,
                metrics: Metrics {
                    rows: 0,
                    columns: 0,
                },
                revision: doc.revision(),
            });
            let reflow = state.revision != doc.revision()
                || state.viewport.dimensions().0 != columns
                || state.metrics.rows == 0;
            if reflow {
                let layout = new_view.layout(doc, state.soft_wrap)?;
                state.metrics = layout.metrics();
                state.desired_column = None;
                if state.revision != doc.revision() {
                    state.affinity = Affinity::Downstream;
                }
                state.revision = doc.revision();
            }
            if reflow || state.viewport.dimensions() != (columns, rows) {
                state.viewport.resize(
                    columns,
                    rows,
                    state.metrics.rows,
                    state.metrics.columns,
                    state.soft_wrap,
                )?;
            }
            if reveal == Some(id) && self.geometry.grid().0 != 0 && self.geometry.grid().1 != 0 {
                let position = state
                    .viewport
                    .layout(doc, state.soft_wrap)?
                    .position(Caret {
                        byte: doc.selection().caret,
                        affinity: state.affinity,
                    })?;
                state
                    .viewport
                    .reveal(position, state.metrics.rows, state.soft_wrap);
            }
        }
        Ok(())
    }

    fn apply(&mut self, event: Event<'_>) -> Result<Outcome> {
        match event {
            Event::New | Event::Load(_) => {
                let id = match event {
                    Event::Load(bytes) => self.editor.load_bytes(bytes)?,
                    _ => self.editor.new_tab()?,
                };
                self.reset_input();
                self.refresh(Some(id))?;
                self.wake_caret();
                Ok(Outcome::Created(id))
            }
            Event::SelectTab(id) => {
                self.editor.select_tab(id)?;
                self.reset_input();
                self.wake_caret();
                Ok(Outcome::Changed)
            }
            Event::Close { tab, revision } => {
                self.editor.close_tab(tab, revision)?;
                self.reset_input();
                self.refresh(None)?;
                self.wake_caret();
                Ok(Outcome::Changed)
            }
            Event::Edit {
                tab,
                revision,
                command,
            } => {
                self.editor.dispatch(tab, revision, command)?;
                self.reset_input();
                self.nonvertical(tab)?;
                Ok(Outcome::Changed)
            }
            Event::Key {
                tab,
                revision,
                chord,
            } => self.key(tab, revision, chord),
            Event::Resize {
                width,
                height,
                scale,
            } => {
                let geometry = Geometry::new(width, height, Scale::new(scale)?)?;
                if geometry == self.geometry {
                    return Ok(Outcome::Ignored);
                }
                self.geometry = geometry;
                self.drag = None;
                self.refresh(None)?;
                Ok(Outcome::Changed)
            }
            Event::Profile(profile) => {
                self.reset_input();
                self.keys.set_profile(profile);
                Ok(Outcome::Changed)
            }
            Event::Wrap {
                tab,
                revision,
                enabled,
            } => {
                self.checked(tab, revision, false)?;
                let state = self.tabs.get_mut(&tab).ok_or(Error::MissingTab)?;
                if state.soft_wrap == enabled {
                    return Ok(Outcome::Ignored);
                }
                state.soft_wrap = enabled;
                state.metrics.rows = 0;
                state.affinity = Affinity::Downstream;
                self.drag = None;
                self.refresh(Some(tab))?;
                Ok(Outcome::Changed)
            }
            Event::Scroll {
                tab,
                revision,
                rows,
                columns,
            } => {
                self.checked(tab, revision, false)?;
                let state = self.tabs.get_mut(&tab).ok_or(Error::MissingTab)?;
                let before = state.viewport;
                state.viewport.scroll(rows, state.metrics.rows);
                state
                    .viewport
                    .scroll_horizontal(columns, state.metrics.columns, state.soft_wrap);
                if state.viewport == before {
                    return Ok(Outcome::Ignored);
                }
                self.drag = None;
                Ok(Outcome::Changed)
            }
            Event::Pointer {
                tab,
                revision,
                phase,
                x,
                y,
                extend,
            } => self.pointer(tab, revision, phase, x, y, extend),
            Event::Focus(focused) => {
                if focused == self.focused {
                    return Ok(Outcome::Ignored);
                }
                self.focused = focused;
                if !focused {
                    self.reset_input();
                }
                self.wake_caret();
                Ok(Outcome::Changed)
            }
            Event::Tick(now) => {
                if now < self.clock {
                    return Err(Error::InvalidArgument);
                }
                let visible = self.focused && ((now - self.blink_start) / 500).is_multiple_of(2);
                self.clock = now;
                let changed = visible != self.caret_visible;
                self.caret_visible = visible;
                Ok(if changed {
                    Outcome::Changed
                } else {
                    Outcome::Ignored
                })
            }
        }
    }

    fn nonvertical(&mut self, tab: TabId) -> Result<()> {
        let state = self.tabs.get_mut(&tab).ok_or(Error::MissingTab)?;
        state.affinity = Affinity::Downstream;
        state.desired_column = None;
        self.drag = None;
        self.refresh(Some(tab))?;
        self.wake_caret();
        Ok(())
    }

    fn key(&mut self, tab: TabId, revision: u64, chord: &str) -> Result<Outcome> {
        self.checked(tab, revision, true)?;
        if !self.focused {
            return Err(Error::Unavailable);
        }
        let mut keys = self.keys.clone();
        let action = keys.translate(chord)?;
        let result = match action {
            Action::Edit(mut command) => {
                let moving = matches!(command, Command::Move { .. });
                if let Command::Move { extend, .. } = &mut command {
                    *extend |= self.mark == Some(tab);
                }
                self.editor.dispatch(tab, revision, command)?;
                if !moving {
                    self.mark = None;
                }
                self.nonvertical(tab)?;
                Outcome::Changed
            }
            Action::New => {
                let id = self.editor.new_tab()?;
                self.mark = None;
                self.refresh(Some(id))?;
                Outcome::Created(id)
            }
            Action::NextTab(backward) => {
                self.editor.next_tab(backward)?;
                self.mark = None;
                keys.reset();
                Outcome::Changed
            }
            Action::SelectAll => {
                let caret = self.editor.document(tab)?.text().len();
                self.editor.dispatch(
                    tab,
                    revision,
                    Command::Select(Selection { anchor: 0, caret }),
                )?;
                self.mark = None;
                self.nonvertical(tab)?;
                Outcome::Changed
            }
            Action::Cancel if keys.profile() == Profile::Windows => {
                self.mark = None;
                Outcome::Changed
            }
            Action::SetMark | Action::Cancel => {
                let caret = self.editor.document(tab)?.selection().caret;
                self.editor.dispatch(
                    tab,
                    revision,
                    Command::Select(Selection {
                        anchor: caret,
                        caret,
                    }),
                )?;
                self.mark = if chord == "C-Space" { Some(tab) } else { None };
                self.nonvertical(tab)?;
                Outcome::Changed
            }
            Action::Prefix => Outcome::Prefix,
            Action::Request(name) => match name {
                "up" | "down" | "select-up" | "select-down" | "page-up" | "page-down"
                | "select-page-up" | "select-page-down" => {
                    let page = name.contains("page");
                    let amount = if page {
                        self.tab_view(tab)?.viewport.dimensions().1 as isize
                    } else {
                        1
                    };
                    let delta = if name.ends_with("up") {
                        -amount
                    } else {
                        amount
                    };
                    self.vertical(
                        tab,
                        revision,
                        delta,
                        name.starts_with("select-") || self.mark == Some(tab),
                    )?;
                    Outcome::Changed
                }
                _ => Outcome::Request {
                    name,
                    tab,
                    revision,
                },
            },
        };
        self.keys = keys;
        self.drag = None;
        self.wake_caret();
        Ok(result)
    }

    fn vertical(&mut self, tab: TabId, revision: u64, delta: isize, extend: bool) -> Result<()> {
        if self.geometry.grid().0 == 0 || self.geometry.grid().1 == 0 {
            return Err(Error::Unavailable);
        }
        let state = self.tab_view(tab)?;
        let doc = self.editor.document(tab)?;
        let layout = state.viewport.layout(doc, state.soft_wrap)?;
        let caret = Caret {
            byte: doc.selection().caret,
            affinity: state.affinity,
        };
        let desired = match state.desired_column {
            Some(column) => column,
            None => layout.position(caret)?.column,
        };
        let next = layout.vertical(caret, delta, desired)?;
        let anchor = if extend {
            doc.selection().anchor
        } else {
            next.byte
        };
        self.editor.dispatch(
            tab,
            revision,
            Command::Select(Selection {
                anchor,
                caret: next.byte,
            }),
        )?;
        let state = self.tabs.get_mut(&tab).ok_or(Error::MissingTab)?;
        state.affinity = next.affinity;
        state.desired_column = Some(desired);
        self.refresh(Some(tab))?;
        Ok(())
    }

    fn pointer(
        &mut self,
        tab: TabId,
        revision: u64,
        phase: PointerPhase,
        x: i64,
        y: i64,
        extend: bool,
    ) -> Result<Outcome> {
        self.checked(tab, revision, true)?;
        if phase == PointerPhase::Press {
            if !contains(self.geometry.bounds(), x, y) {
                return Ok(Outcome::Ignored);
            }
            let count = self.editor.tabs().count();
            let active = self
                .editor
                .tabs()
                .position(|(id, _)| id == tab)
                .ok_or(Error::MissingTab)?;
            let hit = self
                .editor
                .tabs()
                .enumerate()
                .find_map(|(index, (id, doc))| {
                    let rect = self.geometry.tab(index, active, count)?;
                    let close = self.geometry.tab_close(index, active, count)?;
                    contains(rect, x, y).then_some((id, doc.revision(), close))
                });
            // Status is painted last and wins overlaps on tiny surfaces.
            if contains(self.geometry.status(), x, y) {
                return Ok(Outcome::Ignored);
            }
            if let Some((id, revision, close)) = hit {
                self.reset_input();
                if contains(close, x, y) {
                    return Ok(Outcome::Request {
                        name: "close-tab",
                        tab: id,
                        revision,
                    });
                }
                self.editor.select_tab(id)?;
                self.wake_caret();
                return Ok(Outcome::Changed);
            }
        } else if self.drag.is_none() {
            return Ok(Outcome::Ignored);
        }
        let (columns, rows) = self.geometry.grid();
        if columns == 0 || rows == 0 {
            return Ok(Outcome::Ignored);
        }
        let s = self.geometry.scale().value();
        let mut area = self.geometry.document();
        area.width = (columns * CELL_WIDTH * s) as u32;
        area.height = (rows * CELL_HEIGHT * s) as u32;
        if phase == PointerPhase::Press && !contains(area, x, y) {
            return Ok(Outcome::Ignored);
        }
        // All cell midpoints are integral font pixels. Ceiling preserves the
        // strict "past midpoint" decision for scaled subpixel coordinates.
        let px =
            (x.saturating_sub(area.x).clamp(0, i64::from(area.width) - 1) as usize).div_ceil(s);
        let py = y
            .saturating_sub(area.y)
            .clamp(0, i64::from(area.height) - 1) as usize
            / s;
        let state = self.tab_view(tab)?;
        let doc = self.editor.document(tab)?;
        let layout = state.viewport.layout(doc, state.soft_wrap)?;
        let row = state.viewport.origin().row + py / CELL_HEIGHT;
        let x = state.viewport.origin().column * CELL_WIDTH + px;
        let caret = layout.rows().nth(row).map_or(
            Caret {
                byte: doc.text().len(),
                affinity: Affinity::Downstream,
            },
            |row| row.hit_test(x),
        );
        let anchor = if phase == PointerPhase::Press {
            if extend {
                doc.selection().anchor
            } else {
                caret.byte
            }
        } else {
            let (id, anchor) = self.drag.ok_or(Error::InvalidArgument)?;
            if id != tab {
                return Err(Error::InvalidArgument);
            }
            anchor
        };
        self.editor.dispatch(
            tab,
            revision,
            Command::Select(Selection {
                anchor,
                caret: caret.byte,
            }),
        )?;
        self.reset_input();
        self.drag = if phase == PointerPhase::Release {
            None
        } else {
            Some((tab, anchor))
        };
        let state = self.tabs.get_mut(&tab).ok_or(Error::MissingTab)?;
        state.affinity = caret.affinity;
        state.desired_column = None;
        self.wake_caret();
        Ok(Outcome::Changed)
    }
}

fn contains(rect: Rect, x: i64, y: i64) -> bool {
    x >= rect.x
        && y >= rect.y
        && x < rect.x.saturating_add(i64::from(rect.width))
        && y < rect.y.saturating_add(i64::from(rect.height))
}
